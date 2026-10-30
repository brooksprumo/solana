use {
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_leader_schedule::SlotLeader,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::{Bank, StakeAccountsReader}, genesis_utils::create_genesis_config_with_leader},
    solana_signer::Signer,
    std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        hint::black_box,
        sync::Arc,
    },
};

struct CountAllocations;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

fn record_allocation() {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some(value) = count.get() {
            count.set(Some(value + 1));
        }
    });
}

unsafe impl GlobalAlloc for CountAllocations {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation();
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountAllocations = CountAllocations;

fn without_allocations<T>(read: impl FnOnce() -> T) -> T {
    ALLOCATIONS.with(|count| assert!(count.replace(Some(0)).is_none()));
    let result = black_box(read());
    let allocations = ALLOCATIONS.with(|count| count.replace(None).unwrap());
    assert_eq!(allocations, 0, "vote-account read allocated");
    result
}

fn check_reads(bank: &Bank, vote: &Pubkey, expected_lamports: u64) {
    without_allocations(|| black_box(bank.vote_accounts()));
    let account = without_allocations(|| bank.get_vote_account(vote)).unwrap();
    assert_eq!(account.lamports(), expected_lamports);
    let count = without_allocations(|| {
        bank.vote_accounts().iter().map(|(key, (stake, account))| {
            black_box((key, stake, account.vote_state_view().last_timestamp()));
            assert_eq!(account.lamports(), expected_lamports);
            1usize
        }).sum::<usize>()
    });
    assert_eq!(count, 1);
}

#[test]
fn test_vote_account_reads_allocate_nothing_across_flush_and_forks() {
    let genesis = create_genesis_config_with_leader(100_000, &Pubkey::new_unique(), 3);
    let vote = genesis.voting_keypair.pubkey();
    let bank0 = Arc::new(Bank::new_for_tests(&genesis.genesis_config));
    let original = bank0.get_vote_account(&vote).unwrap();
    check_reads(&bank0, &vote, original.lamports());
    bank0.squash();
    bank0.force_flush_accounts_cache();
    check_reads(&bank0, &vote, original.lamports());

    let bank1 = Arc::new(Bank::new_from_parent(bank0.clone(), SlotLeader::new_unique(), 1));
    let sibling = Bank::new_from_parent(bank0, SlotLeader::new_unique(), 2);
    let mut changed = original.account().clone();
    changed.set_lamports(original.lamports() + 10);
    bank1.store_account(&vote, &changed);
    check_reads(&bank1, &vote, original.lamports() + 10);
    check_reads(&sibling, &vote, original.lamports());
    bank1.squash();
    bank1.force_flush_accounts_cache();
    check_reads(&bank1, &vote, original.lamports() + 10);

    let bank3 = Bank::new_from_parent(bank1, SlotLeader::new_unique(), 3);
    bank3.store_account(&vote, &AccountSharedData::default());
    assert!(without_allocations(|| bank3.get_vote_account(&vote)).is_none());
    assert_eq!(without_allocations(|| bank3.vote_accounts().iter().count()), 0);
    bank3.squash();
    bank3.force_flush_accounts_cache();
    assert!(without_allocations(|| bank3.get_vote_account(&vote)).is_none());
    assert_eq!(without_allocations(|| bank3.vote_accounts().iter().count()), 0);
}

#[test]
fn test_reward_stake_inputs_allocate_nothing_across_flush_and_forks() {
    let genesis = create_genesis_config_with_leader(100_000, &Pubkey::new_unique(), 3);
    let stake_key = *genesis.genesis_config.accounts.iter()
        .find(|(_, account)| account.owner() == &solana_stake_interface::program::id()).unwrap().0;
    let bank0 = Arc::new(Bank::new_for_tests(&genesis.genesis_config));
    let original = bank0.get_account(&stake_key).unwrap();
    let invalid = AccountSharedData::new(10, solana_stake_interface::state::StakeStateV2::size_of(),
        &solana_stake_interface::program::id());
    bank0.store_account(&Pubkey::new_unique(), &invalid);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let check = |bank: &Bank, expected: usize| {
        pool.install(|| {
            // Warm worker-local scheduler state outside the allocation measurement.
            black_box(bank.stake_accounts_for_rewards().len());
            let count = without_allocations(|| bank.stake_accounts_for_rewards().fold(|| 0usize,
                |count, key, account| { black_box((key, account)); count + 1 }, |a, b| a + b));
            assert_eq!(count, expected);
            assert_eq!(without_allocations(|| bank.stake_accounts_for_rewards().len()), expected);
            let votes = without_allocations(|| bank.rc.accounts.accounts_db.fold_reward_accounts(
                &bank.ancestors, &solana_vote_program::id(), || 0usize,
                |count, key, account| {
                    let account = solana_vote::vote_account::VoteAccount::try_from(account).unwrap();
                    black_box((key, account));
                    count + 1
                }, |a, b| a + b));
            assert_eq!(votes, 1);
        });
    };
    check(&bank0, 1);
    bank0.squash();
    bank0.force_flush_accounts_cache();
    check(&bank0, 1);

    let bank1 = Arc::new(Bank::new_from_parent(bank0.clone(), SlotLeader::new_unique(), 1));
    let sibling = Bank::new_from_parent(bank0, SlotLeader::new_unique(), 2);
    let extra = Pubkey::new_unique();
    bank1.store_account(&extra, &original);
    bank1.store_account(&stake_key, &AccountSharedData::default());
    sibling.store_account(&Pubkey::new_unique(), &original);
    check(&bank1, 1);
    check(&sibling, 2);
    bank1.squash();
    bank1.force_flush_accounts_cache();
    check(&bank1, 1);

    let bank3 = Bank::new_from_parent(bank1, SlotLeader::new_unique(), 3);
    let mut moved = original.clone();
    moved.set_owner(Pubkey::new_unique());
    bank3.store_account(&extra, &moved);
    check(&bank3, 0);
    bank3.squash();
    bank3.force_flush_accounts_cache();
    check(&bank3, 0);
}
