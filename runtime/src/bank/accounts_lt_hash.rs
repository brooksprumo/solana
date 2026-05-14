use {
    super::{Bank, NUM_REPLAY_HASH_THREADS, replay_hash_thread_pool},
    ahash::AHashSet,
    crossbeam_queue::SegQueue,
    rayon::ThreadPool,
    solana_account::{AccountSharedData, ReadableAccount, accounts_equal},
    solana_accounts_db::{accounts_db::AccountsDb, storable_accounts::StorableAccounts},
    solana_lattice_hash::lt_hash::LtHash,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    std::{
        any::Any,
        cmp, iter,
        num::Saturating,
        panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
        sync::{
            Arc, LazyLock, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    },
};

impl Bank {
    /// Enqueues the accounts lt hash updates for `accounts` to the replay-hash thread pool.
    pub fn enqueue_accounts_lt_hash_updates<'a>(&self, accounts: &impl StorableAccounts<'a>) {
        if accounts.is_empty() {
            return;
        }

        let pending_updates_freelist = pending_updates_freelist();
        let mut pending_updates = pending_updates_freelist.pop().unwrap_or_default();
        let mut seen = AHashSet::with_capacity(accounts.len());
        // process accounts in reverse because we must only count the latest version of each account
        for index in (0..accounts.len()).rev() {
            let address = accounts.pubkey(index);
            if !seen.insert(*address) {
                // we've already enqueued a newer update for the same account; skip this one
                continue;
            }
            let prev_account = self
                .rc
                .accounts
                .load_with_fixed_root_do_not_populate_read_cache(&self.ancestors, address)
                .map(|(account, _slot)| account);
            let curr_account = accounts.account(index, |account| {
                (account.lamports() != 0).then(|| account.take_account())
            });
            match (&prev_account, &curr_account) {
                (None, None) => {
                    // the account is ephemeral; skip it
                }
                (Some(a), Some(b)) if accounts_equal(a, b) => {
                    // the account was not modified; skip it
                }
                _ => {
                    // the account was modified; enqueue this update
                    let hash_cost = calc_hash_cost(prev_account.as_ref())
                        + calc_hash_cost(curr_account.as_ref());
                    pending_updates.push(PendingUpdate {
                        hash_cost,
                        update: AccountsLtHashUpdate {
                            address: *address,
                            prev_account,
                            curr_account,
                        },
                    });
                }
            }
        }

        // Split the pending updates in batches; minimum one per replay-hash thread.
        // Attempt to evenly distribute the hashing work, based on the "hash cost",
        // which is effectively the number of bytes to hash per update.
        let num_updates = pending_updates.len();
        let num_batches = num_updates.div_ceil(MAX_BATCHED_UPDATES_PER_VEC);
        let num_batches = cmp::max(num_batches, NUM_REPLAY_HASH_THREADS);
        let batched_updates_freelist = batched_updates_freelist();
        let mut batches: Box<_> = iter::repeat_with(|| AccountsLtHashBatch {
            updates: batched_updates_freelist
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(MAX_BATCHED_UPDATES_PER_VEC)),
            hash_cost: 0,
        })
        .take(num_batches)
        .collect();

        pending_updates
            .sort_unstable_by_key(|pending_update| cmp::Reverse(pending_update.hash_cost));
        for pending_update in pending_updates.drain(..) {
            let batch = batches
                .iter_mut()
                .min_by_key(|batch| batch.hash_cost)
                .unwrap();
            let PendingUpdate { update, hash_cost } = pending_update;
            batch.hash_cost += hash_cost;
            batch.updates.push(update);
        }

        // Dispatch the batched updates to the replay-hash thread pool.
        // If any of the batches were unused, reclaim them and add them
        // back to the freelist.
        let async_progress = &self.accounts_lt_hash_async_progress;
        let thread_pool = replay_hash_thread_pool();
        for batch in batches {
            let updates = batch.updates;
            if !updates.is_empty() {
                async_progress.spawn(thread_pool, updates);
            } else {
                batched_updates_freelist.push(updates);
            }
        }

        // Reclaim the pending updates too!
        pending_updates_freelist.push(pending_updates);
    }

    /// Updates the accounts lt hash.
    ///
    /// When freezing a bank, we compute and update the accounts lt hash.
    /// For each account modified in this bank, we:
    /// - mix out its previous state, and
    /// - mix in its current state
    ///
    /// This function waits for any in-flight jobs on the replay-hash threads,
    /// computes their combined delta lt hash, then mixes it into the bank.
    ///
    /// Since this function is non-idempotent, it should only be called once per bank.
    pub(crate) fn finish_accounts_lt_hash_updates(&self) {
        let finish_time = Measure::start("");
        let (delta_lt_hash, stats, num_jobs_total, should_mix_in) =
            self.accounts_lt_hash_async_progress.finish();
        if should_mix_in {
            self.accounts_lt_hash
                .lock()
                .unwrap()
                .0
                .mix_in(&delta_lt_hash);
        }
        let finish_us = finish_time.end_as_us();
        let batched_updates_freelist = batched_updates_freelist();
        let batched_updates_freelist_capacity = batched_updates_freelist
            .total_capacity
            .load(Ordering::Relaxed);
        let pending_updates_freelist = pending_updates_freelist();
        let pending_updates_freelist_capacity = pending_updates_freelist
            .total_capacity
            .load(Ordering::Relaxed);
        datapoint_info!(
            "bank-accounts_lt_hash",
            ("slot", self.slot(), i64),
            ("num_jobs", num_jobs_total.0, i64),
            ("num_updates", stats.num_updates.0, i64),
            ("finish_us", finish_us, i64),
            (
                "batched_updates_freelist_num_vecs",
                batched_updates_freelist.num_vecs.load(Ordering::Relaxed),
                i64
            ),
            (
                "batched_updates_freelist_capacity_elems",
                batched_updates_freelist_capacity,
                i64
            ),
            (
                "batched_updates_freelist_capacity_bytes",
                batched_updates_freelist_capacity * size_of::<AccountsLtHashUpdate>(),
                i64
            ),
            (
                "pending_updates_freelist_num_vecs",
                pending_updates_freelist.num_vecs.load(Ordering::Relaxed),
                i64
            ),
            (
                "pending_updates_freelist_capacity_elems",
                pending_updates_freelist_capacity,
                i64
            ),
            (
                "pending_updates_freelist_capacity_bytes",
                pending_updates_freelist_capacity * size_of::<PendingUpdate>(),
                i64
            ),
        );
    }
}

/// Struct for tracking progress of the asynchronous accounts lt hashing for a Bank.
pub struct AccountsLtHashAsyncProgress {
    accumulator: Arc<Mutex<AccountsLtHashAccumulator>>,
    num_jobs_pending: Arc<AtomicUsize>,
    state: Mutex<AsyncProgressState>,
}

impl AccountsLtHashAsyncProgress {
    /// Creates a new AccountsLtHashAsyncProgress variable, which is suitable for a new Bank.
    pub fn new() -> Self {
        let accumulator = AccountsLtHashAccumulator {
            lt_hash: LtHash::identity(),
            stats: UpdateStats::default(),
            first_panic: None,
        };
        Self {
            accumulator: Arc::new(Mutex::new(accumulator)),
            num_jobs_pending: Arc::new(AtomicUsize::new(0)),
            state: Mutex::new(AsyncProgressState {
                num_jobs_total: Saturating(0),
                is_finalized: false,
            }),
        }
    }

    /// Enqueues `updates` into `thread_pool` for asynchronous processing.
    ///
    /// Panics if `updates` is empty, or if `self` was already finalized.
    fn spawn(&self, thread_pool: &'static ThreadPool, updates: Vec<AccountsLtHashUpdate>) {
        debug_assert!(!updates.is_empty());
        {
            let mut state = self.state.lock().unwrap();
            assert!(!state.is_finalized);
            state.num_jobs_total += 1;
            self.num_jobs_pending.fetch_add(1, Ordering::Relaxed);
        }
        let accumulator = Arc::clone(&self.accumulator);
        let num_jobs_pending = Arc::clone(&self.num_jobs_pending);
        thread_pool.spawn(move || {
            let mut updates = updates;
            let result = catch_unwind(AssertUnwindSafe(|| {
                let num_updates = Saturating(updates.len() as u64);
                let lt_hash = Self::process(&mut updates);
                let mut accumulator = accumulator.lock().unwrap_or_else(|err| err.into_inner());
                accumulator.lt_hash.mix_in(&lt_hash);
                accumulator.stats.num_updates += num_updates;
            }));

            // make sure to clear the updates vec just in case the drain() was interrupted
            updates.clear();
            batched_updates_freelist().push(updates);

            if let Err(payload) = result {
                let mut accumulator = accumulator.lock().unwrap_or_else(|err| err.into_inner());
                if accumulator.first_panic.is_none() {
                    accumulator.first_panic = Some(payload);
                }
            }

            num_jobs_pending.fetch_sub(1, Ordering::Release);
        });
    }

    /// Processes `updates` and returns their overall accounts lt hash.
    fn process(updates: &mut Vec<AccountsLtHashUpdate>) -> LtHash {
        let mut accum_lt_hash = LtHash::identity();
        for update in updates.drain(..) {
            let AccountsLtHashUpdate {
                address,
                prev_account,
                curr_account,
            } = update;
            if let Some(prev_account) = prev_account {
                let prev_lt_hash = AccountsDb::lt_hash_account(&prev_account, &address);
                accum_lt_hash.mix_out(&prev_lt_hash.0);
            }
            if let Some(curr_account) = curr_account {
                let curr_lt_hash = AccountsDb::lt_hash_account(&curr_account, &address);
                accum_lt_hash.mix_in(&curr_lt_hash.0);
            }
        }
        accum_lt_hash
    }

    /// Finalizes the asynchronous accounts lt hash updates.
    ///
    /// This fn waits for all pending jobs to complete, then returns:
    /// * the overall accounts lt hash
    /// * the stats from all the updates
    /// * the number of asynchronous jobs
    /// * if this was the first time finish() was called
    fn finish(&self) -> (LtHash, UpdateStats, Saturating<u64>, bool) {
        // make sure to lock `state` before spinning on num_jobs_pending
        // to ensure no new jobs are added
        let mut state = self.state.lock().unwrap();
        while self.num_jobs_pending.load(Ordering::Acquire) > 0 {
            // Spin, do not yield! This is called by Bank::freeze() and we want to be fast.
        }

        let mut accumulator = self
            .accumulator
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(payload) = accumulator.first_panic.take() {
            resume_unwind(payload);
        }
        let was_finalized = state.is_finalized;
        state.is_finalized = true;
        (
            accumulator.lt_hash.clone(),
            accumulator.stats.clone(),
            state.num_jobs_total,
            !was_finalized,
        )
    }
}

/// A batch of accounts lt hash updates to process.
#[derive(Default)]
struct AccountsLtHashBatch {
    updates: Vec<AccountsLtHashUpdate>,
    hash_cost: usize,
}

/// The struct to accumulate results from processing a batch of updates.
struct AccountsLtHashAccumulator {
    lt_hash: LtHash,
    stats: UpdateStats,
    first_panic: Option<Box<dyn Any + Send + 'static>>,
}

/// Stats from processing a batch of updates.
#[derive(Clone, Debug, Default)]
struct UpdateStats {
    num_updates: Saturating<u64>,
}

/// The state of the asynchronous progress itself.
#[derive(Debug)]
struct AsyncProgressState {
    num_jobs_total: Saturating<u64>,
    is_finalized: bool,
}

/// A single accounts lt hash update to process.
#[derive(Debug)]
struct AccountsLtHashUpdate {
    address: Pubkey,
    prev_account: Option<AccountSharedData>,
    curr_account: Option<AccountSharedData>,
}

// brooks TODO: doc
//│ 23 │/// The number of lt hash updates to batch up before sending to the async thread pool.                                      │    │
//│ 24 │///                                                                                                                         │    │
//│ 25 │/// The 14 KiB number is the current largest size of jemalloc's "small slab" bins,                                          │    │
//│ 26 │/// which should help with reuse.                                                                                           │    │
//│ 27 │const MAX_BATCH_SIZE: usize = 14 * 1024 / size_of::<AccountsLtHashUpdate>();                                                │    │
const MAX_BATCHED_UPDATES_VEC_BYTES: usize = 14 * 1024;
const MAX_BATCHED_UPDATES_PER_VEC: usize =
    MAX_BATCHED_UPDATES_VEC_BYTES / size_of::<AccountsLtHashUpdate>();
const _: () = assert!(MAX_BATCHED_UPDATES_PER_VEC > 0);

/// Get the freelist of vectors to use for batching updates.
fn batched_updates_freelist() -> &'static VecFreelist<AccountsLtHashUpdate> {
    static FREELIST: LazyLock<VecFreelist<AccountsLtHashUpdate>> = LazyLock::new(VecFreelist::new);
    &FREELIST
}

/// A pending update, used to put into batches for processing.
#[derive(Debug)]
struct PendingUpdate {
    update: AccountsLtHashUpdate,
    hash_cost: usize,
}

/// Get the freelist of vectors to use for holding pending updates.
fn pending_updates_freelist() -> &'static VecFreelist<PendingUpdate> {
    static FREELIST: LazyLock<VecFreelist<PendingUpdate>> = LazyLock::new(VecFreelist::new);
    &FREELIST
}

/// Freelist of vectors, to avoid repeat allocations/deallocations.
#[derive(Debug)]
struct VecFreelist<T> {
    list: SegQueue<Vec<T>>,

    // stats
    num_vecs: AtomicUsize,
    total_capacity: AtomicUsize,
}

impl<T> VecFreelist<T> {
    /// Creates a new, empty, freelist.
    fn new() -> Self {
        Self {
            list: SegQueue::new(),
            num_vecs: AtomicUsize::new(0),
            total_capacity: AtomicUsize::new(0),
        }
    }

    /// Pushes `vec` on to the freelist (IFF its capacity is greater than zero).
    ///
    /// Panics if `vec` is not empty.
    fn push(&self, vec: Vec<T>) {
        // If the capacity is zero, then the Vec never allocated.  In that case, don't waste time
        // putting it back into the freelist, since there's nothing of value to reuse.
        let capacity = vec.capacity();
        if capacity != 0 {
            assert!(vec.is_empty());
            self.list.push(vec);
            self.num_vecs.fetch_add(1, Ordering::Relaxed);
            self.total_capacity.fetch_add(capacity, Ordering::Relaxed);
        }
    }

    /// Pops a vec off the freelist and returns it.
    ///
    /// The returned vec will always be empty.
    fn pop(&self) -> Option<Vec<T>> {
        let vec = self.list.pop()?;
        assert!(vec.is_empty());
        self.num_vecs.fetch_sub(1, Ordering::Relaxed);
        self.total_capacity
            .fetch_sub(vec.capacity(), Ordering::Relaxed);
        Some(vec)
    }
}

/// Calculates the cost of hashing an account.
///
/// Which is an approximation based on the number of bytes to hash.
#[inline]
fn calc_hash_cost(account: Option<&impl ReadableAccount>) -> usize {
    const ACCOUNT_HASH_METADATA_BYTES: usize = 8 /* lamports */
    + 1 /* executable */
    + 32 /* owner */
    + 32 /* address */;

    account.map_or(0, |account| {
        if account.lamports() == 0 {
            0
        } else {
            account.data().len() + ACCOUNT_HASH_METADATA_BYTES
        }
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            genesis_utils::create_genesis_config_with_leader_ex, runtime_config::RuntimeConfig,
            snapshot_bank_utils, snapshot_utils,
        },
        agave_feature_set::FeatureSet,
        agave_snapshots::snapshot_config::SnapshotConfig,
        solana_accounts_db::{
            accounts_db::{ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDbConfig},
            accounts_index::{ACCOUNTS_INDEX_CONFIG_FOR_TESTING, AccountsIndexConfig, IndexLimit},
        },
        solana_cluster_type::ClusterType,
        solana_fee_calculator::FeeRateGovernor,
        solana_genesis_config::{self, GenesisConfig},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_pubkey::{self as pubkey, Pubkey},
        solana_rent::Rent,
        solana_signer::Signer as _,
        std::{
            cmp, iter,
            str::FromStr as _,
            sync::Arc,
            thread,
            time::{Duration, Instant},
        },
        tempfile::TempDir,
        test_case::{test_case, test_matrix},
    };

    /// What features should be enabled?
    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    enum Features {
        /// Do not enable any features
        None,
        /// Enable all features
        All,
    }

    /// Creates a genesis config with `features` enabled
    fn genesis_config_with(features: Features) -> (GenesisConfig, Keypair) {
        let mint_keypair = Keypair::new();
        let mint_lamports = 123_456_789 * LAMPORTS_PER_SOL;
        let validator_lamports = 100 * LAMPORTS_PER_SOL;
        let validator_stake_lamports = 10 * LAMPORTS_PER_SOL;
        let validator_pubkey = Pubkey::new_unique();
        let vote_account_pubkey = Pubkey::new_unique();
        let stake_account_pubkey = Pubkey::new_unique();
        let feature_set = match features {
            Features::None => FeatureSet::default(),
            Features::All => FeatureSet::all_enabled(),
        };

        let config = create_genesis_config_with_leader_ex(
            mint_lamports,
            &mint_keypair.pubkey(),
            &validator_pubkey,
            &vote_account_pubkey,
            &stake_account_pubkey,
            None,
            validator_stake_lamports,
            validator_lamports,
            FeeRateGovernor::default(),
            Rent::default(),
            ClusterType::Development,
            &feature_set,
            vec![],
        );

        (config, mint_keypair)
    }

    #[test]
    fn test_update_accounts_lt_hash() {
        // Write to address 1, 2, and 5 in first bank, so that in second bank we have
        // updates to these three accounts.  Make address 2 go to zero (dead).  Make address 1 and 3 stay
        // alive.  Make address 5 unchanged.  Ensure the updates are expected.
        //
        // 1: alive -> alive
        // 2: alive -> dead
        // 3: dead -> alive
        // 4. dead -> dead
        // 5. alive -> alive *unchanged*

        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();
        let keypair4 = Keypair::new();
        let keypair5 = Keypair::new();

        let (mut genesis_config, mint_keypair) =
            solana_genesis_config::create_genesis_config(123_456_789 * LAMPORTS_PER_SOL);
        // This test requires zero fees so that we can easily transfer an account's entire balance.
        genesis_config.fee_rate_governor = FeeRateGovernor::new(0, 0);
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // send lamports to accounts 1, 2, and 5 so they are alive,
        // and so we'll have a delta in the next bank
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair1.pubkey())
            .unwrap();
        bank.transfer(amount, &mint_keypair, &keypair2.pubkey())
            .unwrap();
        bank.transfer(amount, &mint_keypair, &keypair5.pubkey())
            .unwrap();

        // manually freeze the bank to trigger updating the accounts lt hash
        bank.freeze();
        let prev_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // save the initial values of the accounts to use for asserts later
        let prev_mint = bank.get_account_with_fixed_root(&mint_keypair.pubkey());
        let prev_account1 = bank.get_account_with_fixed_root(&keypair1.pubkey());
        let prev_account2 = bank.get_account_with_fixed_root(&keypair2.pubkey());
        let prev_account3 = bank.get_account_with_fixed_root(&keypair3.pubkey());
        let prev_account4 = bank.get_account_with_fixed_root(&keypair4.pubkey());
        let prev_account5 = bank.get_account_with_fixed_root(&keypair5.pubkey());

        assert!(prev_mint.is_some());
        assert!(prev_account1.is_some());
        assert!(prev_account2.is_some());
        assert!(prev_account3.is_none());
        assert!(prev_account4.is_none());
        assert!(prev_account5.is_some());

        // These sysvars are also updated, but outside of transaction processing.  This means they
        // will not be in the accounts lt hash cache, but *will* be in the list of modified
        // accounts.  They must be included in the accounts lt hash.
        let sysvars = [
            Pubkey::from_str("SysvarS1otHashes111111111111111111111111111").unwrap(),
            Pubkey::from_str("SysvarC1ock11111111111111111111111111111111").unwrap(),
            Pubkey::from_str("SysvarRecentB1ockHashes11111111111111111111").unwrap(),
            Pubkey::from_str("SysvarS1otHistory11111111111111111111111111").unwrap(),
        ];
        let prev_sysvar_accounts: Vec<_> = sysvars
            .iter()
            .map(|address| bank.get_account_with_fixed_root(address))
            .collect();

        let bank = {
            let slot = bank.slot() + 1;
            Bank::new_from_parent_with_bank_forks(&bank_forks, bank, SlotLeader::default(), slot)
        };

        // send from account 2 to account 1; account 1 stays alive, account 2 ends up dead
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &keypair2, &keypair1.pubkey())
            .unwrap();

        // send lamports to account 4, then turn around and send them to account 3
        // account 3 will be alive, and account 4 will end dead
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &mint_keypair, &keypair4.pubkey())
            .unwrap();
        bank.register_unique_recent_blockhash_for_test();
        bank.transfer(amount, &keypair4, &keypair3.pubkey())
            .unwrap();

        // store account 5 into this new bank, unchanged
        bank.store_account(&keypair5.pubkey(), prev_account5.as_ref().unwrap());

        // freeze the bank to trigger updating the accounts lt hash
        bank.freeze();

        let post_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // ensure the bank's accounts lt hash is only updated once,
        // even if finish() is called multiple times
        bank.finish_accounts_lt_hash_updates();
        assert_eq!(
            *bank.accounts_lt_hash.lock().unwrap(),
            post_accounts_lt_hash,
        );

        let post_mint = bank.get_account_with_fixed_root(&mint_keypair.pubkey());
        let post_account1 = bank.get_account_with_fixed_root(&keypair1.pubkey());
        let post_account2 = bank.get_account_with_fixed_root(&keypair2.pubkey());
        let post_account3 = bank.get_account_with_fixed_root(&keypair3.pubkey());
        let post_account4 = bank.get_account_with_fixed_root(&keypair4.pubkey());
        let post_account5 = bank.get_account_with_fixed_root(&keypair5.pubkey());

        assert!(post_mint.is_some());
        assert!(post_account1.is_some());
        assert!(post_account2.is_none());
        assert!(post_account3.is_some());
        assert!(post_account4.is_none());
        assert!(post_account5.is_some());

        let post_sysvar_accounts: Vec<_> = sysvars
            .iter()
            .map(|address| bank.get_account_with_fixed_root(address))
            .collect();

        let mut expected_accounts_lt_hash = prev_accounts_lt_hash;
        let mut updater =
            |address: &Pubkey, prev: Option<AccountSharedData>, post: Option<AccountSharedData>| {
                // if there was an alive account, mix out
                if let Some(prev) = prev {
                    let prev_lt_hash = AccountsDb::lt_hash_account(&prev, address);
                    expected_accounts_lt_hash.0.mix_out(&prev_lt_hash.0);
                }

                // mix in the new one
                let post = post.unwrap_or_default();
                let post_lt_hash = AccountsDb::lt_hash_account(&post, address);
                expected_accounts_lt_hash.0.mix_in(&post_lt_hash.0);
            };
        updater(&mint_keypair.pubkey(), prev_mint, post_mint);
        updater(&keypair1.pubkey(), prev_account1, post_account1);
        updater(&keypair2.pubkey(), prev_account2, post_account2);
        updater(&keypair3.pubkey(), prev_account3, post_account3);
        updater(&keypair4.pubkey(), prev_account4, post_account4);
        updater(&keypair5.pubkey(), prev_account5, post_account5);
        for (i, sysvar) in sysvars.iter().enumerate() {
            updater(
                sysvar,
                prev_sysvar_accounts[i].clone(),
                post_sysvar_accounts[i].clone(),
            );
        }

        // now make sure the accounts lt hashes match
        let expected = expected_accounts_lt_hash.0.checksum();
        let actual = post_accounts_lt_hash.0.checksum();
        assert_eq!(
            expected, actual,
            "accounts_lt_hash, expected: {expected}, actual: {actual}",
        );
    }

    /// Ensure that the accounts lt hash is correct for slot 0
    ///
    /// This test does a simple transfer in slot 0 so that a primordial account is modified.
    ///
    /// Slot 0 is special because primordial accounts have no previous accounts lt hash entry.
    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_slot0_accounts_lt_hash(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        // ensure this bank is for slot 0, otherwise this test doesn't actually do anything...
        assert_eq!(bank.slot(), 0);

        // process a transaction that modifies a primordial account
        bank.transfer(LAMPORTS_PER_SOL, &mint_keypair, &Pubkey::new_unique())
            .unwrap();

        // manually freeze the bank to trigger updating the accounts lt hash
        bank.freeze();
        let actual_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // ensure the actual accounts lt hash matches the value calculated from the index
        let calculated_accounts_lt_hash = bank
            .rc
            .accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&bank.ancestors);
        assert_eq!(actual_accounts_lt_hash, calculated_accounts_lt_hash);
    }

    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_calculate_accounts_lt_hash_at_startup_from_index(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks and transfers are arbitrary)
        for _ in 0..7 {
            let slot = bank.slot() + 1;
            bank = Bank::new_from_parent_with_bank_forks(
                &bank_forks,
                bank,
                SlotLeader::default(),
                slot,
            );
            for _ in 0..13 {
                bank.register_unique_recent_blockhash_for_test();
                // note: use a random pubkey here to ensure accounts
                // are spread across all the index bins
                bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                    .unwrap();
            }
            bank.freeze();
        }
        let expected_accounts_lt_hash = bank.accounts_lt_hash.lock().unwrap().clone();

        // root the bank and flush the accounts write cache to disk
        // (this more accurately simulates startup, where accounts are in storages on disk)
        bank.squash();
        bank.force_flush_accounts_cache();

        // call the fn that calculates the accounts lt hash at startup, then ensure it matches
        let calculated_accounts_lt_hash = bank
            .rc
            .accounts
            .accounts_db
            .calculate_accounts_lt_hash_at_startup_from_index(&bank.ancestors);
        assert_eq!(expected_accounts_lt_hash, calculated_accounts_lt_hash);
    }

    #[test_matrix(
        [Features::None, Features::All],
        [IndexLimit::Minimal, IndexLimit::InMemOnly]
    )]
    fn test_verify_accounts_lt_hash_at_startup(
        features: Features,
        accounts_index_limit: IndexLimit,
    ) {
        let (mut genesis_config, mint_keypair) = genesis_config_with(features);
        // This test requires zero fees so that we can easily transfer an account's entire balance.
        genesis_config.fee_rate_governor = FeeRateGovernor::new(0, 0);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // Write to this pubkey multiple times, so there are guaranteed duplicates in the storages.
        let duplicate_pubkey = pubkey::new_rand();

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks and transfers are arbitrary)
        for _ in 0..9 {
            let slot = bank.slot() + 1;
            let leader = *bank.leader();
            bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, leader, slot);
            for _ in 0..3 {
                bank.register_unique_recent_blockhash_for_test();
                bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                    .unwrap();
                bank.register_unique_recent_blockhash_for_test();
                bank.transfer(amount, &mint_keypair, &duplicate_pubkey)
                    .unwrap();
            }

            // flush the write cache to disk to ensure there are duplicates across the storages
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }

        // Create a few more storages to exercise the zero lamport duplicates handling during
        // generate_index(), which is used for the lattice-based accounts verification.
        // There needs to be accounts that only have a single duplicate (i.e. there are only two
        // versions of the accounts), and toggle between non-zero and zero lamports.
        // One account will go zero -> non-zero, and the other will go non-zero -> zero.
        let num_accounts = 2;
        let accounts: Vec<_> = iter::repeat_with(Keypair::new).take(num_accounts).collect();
        for i in 0..num_accounts {
            let slot = bank.slot() + 1;
            let leader = *bank.leader();
            bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, leader, slot);
            bank.register_unique_recent_blockhash_for_test();

            // transfer into the accounts so they start with a non-zero balance
            for account in &accounts {
                bank.transfer(amount, &mint_keypair, &account.pubkey())
                    .unwrap();
                assert_ne!(bank.get_balance(&account.pubkey()), 0);
            }

            // then transfer *out* all the lamports from one of 'em
            bank.transfer(
                bank.get_balance(&accounts[i].pubkey()),
                &accounts[i],
                &pubkey::new_rand(),
            )
            .unwrap();
            assert_eq!(bank.get_balance(&accounts[i].pubkey()), 0);

            // flush the write cache to disk to ensure the storages match the accounts written here
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }
        bank.set_block_id(Some(Hash::default()));

        // verification happens at startup, so mimic the behavior by loading from a snapshot
        let bank_snapshots_dir = TempDir::new().unwrap();
        let snapshot_archives_dir = TempDir::new().unwrap();
        let snapshot_config = SnapshotConfig {
            full_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
            incremental_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
            bank_snapshots_dir: bank_snapshots_dir.path().to_path_buf(),
            ..SnapshotConfig::default()
        };
        let snapshot =
            snapshot_bank_utils::bank_to_full_snapshot_archive(&snapshot_config, &bank).unwrap();
        let (_accounts_tempdir, accounts_dir) = snapshot_utils::create_tmp_accounts_dir_for_tests();
        let accounts_index_config = AccountsIndexConfig {
            index_limit: accounts_index_limit,
            ..ACCOUNTS_INDEX_CONFIG_FOR_TESTING
        };
        let accounts_db_config = AccountsDbConfig {
            index: Some(accounts_index_config),
            ..ACCOUNTS_DB_CONFIG_FOR_TESTING
        };
        let roundtrip_bank = snapshot_bank_utils::bank_from_snapshot_archives(
            &[accounts_dir],
            &snapshot,
            None,
            &snapshot_config,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None, // leader_for_tests
            None,
            false,
            false,
            false,
            accounts_db_config,
            None,
            Arc::default(),
        )
        .unwrap();

        // Correctly restoring the accounts lt hash in Bank::new_from_snapshot() depends on the
        // bank already being frozen so pending per-slot LT hash updates cannot be replayed.
        assert!(roundtrip_bank.is_frozen());

        assert_eq!(roundtrip_bank, *bank);
    }

    /// Ensure that the snapshot hash is correct
    #[test_case(Features::None; "no features")]
    #[test_case(Features::All; "all features")]
    fn test_snapshots(features: Features) {
        let (genesis_config, mint_keypair) = genesis_config_with(features);
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = cmp::max(
            bank.get_minimum_balance_for_rent_exemption(0),
            LAMPORTS_PER_SOL,
        );

        // create some banks with some modified accounts so that there are stored accounts
        // (note: the number of banks is arbitrary)
        for _ in 0..3 {
            let slot = bank.slot() + 1;
            let leader = *bank.leader();
            bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, leader, slot);
            bank.register_unique_recent_blockhash_for_test();
            bank.transfer(amount, &mint_keypair, &pubkey::new_rand())
                .unwrap();
            bank.fill_bank_with_ticks_for_tests();
            bank.squash();
            bank.force_flush_accounts_cache();
        }
        bank.set_block_id(Some(Hash::default()));

        let bank_snapshots_dir = TempDir::new().unwrap();
        let snapshot_archives_dir = TempDir::new().unwrap();
        let snapshot_config = SnapshotConfig {
            full_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
            incremental_snapshot_archives_dir: snapshot_archives_dir.path().to_path_buf(),
            bank_snapshots_dir: bank_snapshots_dir.path().to_path_buf(),
            ..SnapshotConfig::default()
        };
        let snapshot =
            snapshot_bank_utils::bank_to_full_snapshot_archive(&snapshot_config, &bank).unwrap();
        let (_accounts_tempdir, accounts_dir) = snapshot_utils::create_tmp_accounts_dir_for_tests();
        let roundtrip_bank = snapshot_bank_utils::bank_from_snapshot_archives(
            &[accounts_dir],
            &snapshot,
            None,
            &snapshot_config,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None, // leader_for_tests
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();

        assert_eq!(roundtrip_bank, *bank);
    }

    /// Ensure that spawn() and finish() do not race.
    #[test]
    fn test_finish_prevents_subsequent_spawn() {
        fn new_accounts_lt_hash_update() -> AccountsLtHashUpdate {
            let curr_account = Some(AccountSharedData::new(42, 0, &Pubkey::default()));
            AccountsLtHashUpdate {
                address: Pubkey::new_unique(),
                prev_account: None,
                curr_account,
            }
        }

        // spin up a thread pool that'll process the async updates
        let thread_pool: &'static ThreadPool = Box::leak(Box::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .unwrap(),
        ));

        // create some channels that'll initially block the thread pool,
        // then unblock later after spawn() and finish() have been called
        let (block_sender, block_receiver) = crossbeam_channel::unbounded();
        let (unblock_sender, unblock_receiver) = crossbeam_channel::unbounded();
        thread_pool.spawn(move || {
            block_sender.send(()).unwrap();
            unblock_receiver.recv().unwrap();
        });
        block_receiver.recv().unwrap();

        // send updates to be processed asynchronously
        let async_progress = Arc::new(AccountsLtHashAsyncProgress::new());
        async_progress.spawn(thread_pool, vec![new_accounts_lt_hash_update()]);
        assert_eq!(async_progress.num_jobs_pending.load(Ordering::Acquire), 1);

        // call finish() to prevent additional async updates from being processed
        let finish_thread = thread::spawn({
            let async_progress = Arc::clone(&async_progress);
            move || async_progress.finish()
        });

        // wait and ensure finish() has started
        let start = Instant::now();
        while async_progress.state.try_lock().is_ok() {
            assert!(start.elapsed() < Duration::from_secs(1));
            thread::yield_now();
        }

        // Send more async updates, which is not allowed since finish() already ran.
        // We do this in another thread and catch the panic to not abort the test.
        let spawn_thread = thread::spawn({
            let async_progress = Arc::clone(&async_progress);
            move || {
                catch_unwind(AssertUnwindSafe(|| {
                    async_progress.spawn(thread_pool, vec![new_accounts_lt_hash_update()]);
                }))
            }
        });

        // unblock the thread pool, which allows the async job to finally be processed
        unblock_sender.send(()).unwrap();

        // ensure after finish() completes that only a single job ran
        let (_lt_hash, _stats, num_jobs_total, should_mix) = finish_thread.join().unwrap();
        assert!(should_mix);
        assert_eq!(num_jobs_total.0, 1);
        assert_eq!(async_progress.num_jobs_pending.load(Ordering::Acquire), 0);

        // and ensure the second spawn() fails
        assert!(spawn_thread.join().unwrap().is_err());
        assert_eq!(async_progress.num_jobs_pending.load(Ordering::Acquire), 0);
    }
}
