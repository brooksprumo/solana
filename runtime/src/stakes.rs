//! Decoded stake state used to derive epoch stake distributions.
#[cfg(feature = "stable-abi")]
use solana_frozen_abi::stable_abi::{context::SequenceLenMax, sample_collection_sized};
#[cfg(feature = "stable-abi")]
use solana_frozen_abi_macro::{StableAbi, StableAbiSample};
use {
    crate::{
        alpenglow_epoch_type::RewardEpochDelegatedStakes, bank::{rewards_calculation_thread_pool, StakeAccountsReader},
        stake_account, stake_history::StakeHistory,
    },
    solana_accounts_db::stakes_cache::StakesMap,
    num_derive::ToPrimitive,
    rayon::{ThreadPool, prelude::*},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_stake_history::StakeHistoryGetEntry,
    solana_stake_interface::{
        program as stake_program,
        state::{Delegation, StakeActivationStatus},
    },
    solana_vote::vote_account::{VoteAccount, VoteAccounts},
    solana_vote_interface::state::VoteStateVersions,
    std::{
        collections::HashMap,
        sync::Arc,
    },
    wincode::{SchemaWrite, containers::FromIntoIterator, len::BincodeLen},
};
#[cfg(feature = "dev-context-only-utils")]
use {
    qualifier_attr::{field_qualifiers, qualifiers},
    solana_stake_interface::state::Stake,
};

mod serde_stakes;
pub(crate) use serde_stakes::DeserializableDelegationStakes;
pub use serde_stakes::SerdeStakesToStakeFormat;
#[derive(Debug, Clone, PartialEq, Eq, ToPrimitive)]
pub enum InvalidCacheEntryReason {
    Missing,
    BadState,
    WrongOwner,
}

type StakeAccount = stake_account::StakeAccount<Delegation>;
pub(crate) type DelegatedStakes = StakesMap<u64>;

/// Results of [`Stakes::calculate_epoch_boundary`] at the epoch boundary.
pub(crate) struct EpochBoundaryStakes {
    pub(crate) stake_history: StakeHistory,
    pub(crate) vote_accounts: VoteAccounts,
    pub(crate) reward_epoch_delegated_stakes: RewardEpochDelegatedStakes,
}

/// The generic type T is either Delegation or StakeAccount.
/// [`Stakes<Delegation>`] is the snapshot layout. Only tests build it.
/// [`Stakes<StakeAccount>`] carries decoded epoch snapshots and test fixtures.
/// Live reward calculation streams accounts from accounts-db instead of building this stake table.
#[cfg_attr(feature = "stable-abi", derive(StableAbi, StableAbiSample))]
#[derive(Default, Clone, PartialEq, Debug, SchemaWrite)]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(
        vote_accounts(pub),
        stake_delegations(pub),
        delegated_stakes(pub),
        unused(pub),
        epoch(pub),
        stake_history(pub),
    )
)]
pub struct Stakes<T: Clone> {
    /// vote accounts
    vote_accounts: VoteAccounts,

    /// stake_delegations
    #[cfg_attr(
        feature = "stable-abi",
        stable_abi_sample(with = "sample_collection_sized(rng, SequenceLenMax(1))")
    )]
    #[wincode(with = "FromIntoIterator<StakesMap<T>, BincodeLen>")]
    stake_delegations: StakesMap<T>,

    /// current effective stake delegated to each vote account pubkey
    #[cfg_attr(feature = "stable-abi", stable_abi_sample(with = "Default::default()"))]
    #[wincode(skip)]
    delegated_stakes: DelegatedStakes,

    /// unused
    unused: u64,

    /// current epoch, used to calculate current stake
    epoch: Epoch,

    /// history of staking levels
    stake_history: StakeHistory,
}

impl<T: Clone> Stakes<T> {
    pub fn new(vote_accounts: VoteAccounts, epoch: Epoch) -> Stakes<T> {
        Stakes {
            vote_accounts,
            epoch,
            stake_delegations: StakesMap::default(),
            delegated_stakes: DelegatedStakes::default(),
            unused: 0,
            stake_history: StakeHistory::default(),
        }
    }

    pub fn clone_and_filter_for_vat(
        &self,
        max_vote_accounts: usize,
        minimum_vote_account_balance: u64,
        block_revenue_sharing: bool,
    ) -> Stakes<T> {
        Self::new(
            self.vote_accounts.clone_and_filter_for_vat(
                max_vote_accounts,
                minimum_vote_account_balance,
                block_revenue_sharing,
            ),
            self.epoch,
        )
    }

    pub fn vote_accounts(&self) -> &VoteAccounts {
        &self.vote_accounts
    }

    pub(crate) fn staked_nodes(&self) -> Arc<HashMap<Pubkey, u64>> {
        self.vote_accounts.staked_nodes()
    }

    /// Destructure self and return the fields needed by EpochStakes
    pub(crate) fn into_epoch_stakes_fields(self) -> (Epoch, VoteAccounts, StakeHistory) {
        let Self {
            vote_accounts,
            stake_delegations: _,
            delegated_stakes: _,
            unused: _,
            epoch,
            stake_history,
        } = self;
        (epoch, vote_accounts, stake_history)
    }
}

impl Stakes<StakeAccount> {
    /// Builds a decoded stakes view from the supplied accounts. Accounts that
    /// are gone, not delegated, or not valid vote accounts are skipped.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn load_from_accounts<F>(
        epoch: Epoch,
        stake_history: StakeHistory,
        new_rate_activation_epoch: Option<Epoch>,
        mut pubkeys_by_owner: ahash::HashMap<Pubkey, Vec<Pubkey>>,
        get_account: F,
    ) -> Self
    where
        F: Fn(&Pubkey) -> Option<AccountSharedData> + Sync,
    {
        let stake_pubkeys = pubkeys_by_owner
            .remove(&stake_program::id())
            .unwrap_or_default();
        let vote_pubkeys = pubkeys_by_owner
            .remove(&solana_vote_program::id())
            .unwrap_or_default();
        let stake_delegations = stake_pubkeys.into_par_iter()
            .fold(StakesMap::default, |mut map, pubkey| {
                if let Some(stake_account) = get_account(&pubkey).and_then(|account| StakeAccount::try_from(account).ok()) {
                    map.insert(pubkey, stake_account);
                }
                map
            })
            .reduce(StakesMap::default, merge_stake_accounts);
        let vote_accounts = vote_pubkeys
            .into_iter()
            .filter_map(|pubkey| {
                let account = get_account(&pubkey)?;
                if !VoteStateVersions::is_correct_size_and_initialized(account.data()) {
                    return None;
                }
                let vote_account = VoteAccount::try_from(account).ok()?;
                Some((pubkey, (0, vote_account)))
            })
            .collect();
        let (vote_accounts, delegated_stakes) = refresh_vote_accounts(
            rewards_calculation_thread_pool(),
            epoch,
            &vote_accounts,
            &stake_delegations,
            &stake_history,
            new_rate_activation_epoch,
        );
        Self {
            vote_accounts,
            stake_delegations,
            delegated_stakes,
            unused: 0,
            epoch,
            stake_history,
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(
        epoch: Epoch,
        vote_accounts: VoteAccounts,
        stake_delegations: StakesMap<StakeAccount>,
    ) -> Self {
        let stake_history = StakeHistory::default();
        let delegated_stakes =
            Self::calculate_delegated_stakes(&stake_delegations, epoch, &stake_history, None);
        Self {
            vote_accounts,
            stake_delegations,
            delegated_stakes,
            unused: 0,
            epoch,
            stake_history,
        }
    }

    #[cfg(test)]
    pub(crate) fn history(&self) -> &StakeHistory {
        &self.stake_history
    }

    /// Decode a rewards view of accounts-db's rooted accounts and visible writes.
    /// Epoch metadata comes from the bank epoch and stake history sysvar.
    #[cfg(any(test, feature = "dev-context-only-utils"))]
    pub(crate) fn reward_fixture(
        epoch: Epoch,
        stake_history: StakeHistory,
        vote_accounts: VoteAccounts,
        stake_delegations: StakesMap<StakeAccount>,
        rate_epoch: Option<Epoch>,
    ) -> Self {
        let delegated_stakes = Self::calculate_delegated_stakes(&stake_delegations, epoch, &stake_history, rate_epoch);
        Self { vote_accounts, stake_delegations, delegated_stakes, unused: 0, epoch, stake_history }
    }

    #[cfg(test)]
    pub(crate) fn from_reward_accounts(
        epoch: Epoch,
        stake_history: StakeHistory,
        accounts: &solana_accounts_db::stakes_cache::StakesCache,
        new_rate_activation_epoch: Option<Epoch>,
    ) -> Self {
        let stake_delegations = accounts.stake_accounts.par_iter()
            .fold(StakesMap::default, |mut map, (pubkey, (_, account))| {
                if let Ok(account) = StakeAccount::try_from(account.clone()) {
                    map.insert(*pubkey, account);
                }
                map
            })
            .reduce(StakesMap::default, merge_stake_accounts);
        let vote_accounts = accounts.vote_accounts.iter().filter_map(|(pubkey, (_, account))| {
            if !VoteStateVersions::is_correct_size_and_initialized(account.data()) {
                return None;
            }
            Some((*pubkey, (0, VoteAccount::try_from(account.clone()).ok()?)))
        }).collect();
        let (vote_accounts, delegated_stakes) = refresh_vote_accounts(
            rewards_calculation_thread_pool(), epoch, &vote_accounts,
            &stake_delegations, &stake_history, new_rate_activation_epoch,
        );
        Self { vote_accounts, stake_delegations, delegated_stakes, unused: 0, epoch, stake_history }
    }

    #[cfg(test)]
    pub(crate) fn calculate_activated_stake(
        &self,
        next_epoch: Epoch,
        thread_pool: &ThreadPool,
        new_rate_activation_epoch: Option<Epoch>,
        stake_delegations: &impl StakeAccountsReader,
    ) -> EpochBoundaryStakes {
        Self::calculate_epoch_boundary(self.epoch, self.stake_history.clone(), next_epoch,
            thread_pool, new_rate_activation_epoch, stake_delegations, |delegated_stakes| {
                self.vote_accounts.iter().map(|(key, account)| {
                    (*key, (delegated_stakes.get(key).copied().unwrap_or_default(), account.clone()))
                }).collect()
            })
    }

    pub(crate) fn calculate_epoch_boundary(
        epoch: Epoch,
        mut stake_history: StakeHistory,
        next_epoch: Epoch,
        thread_pool: &ThreadPool,
        new_rate_activation_epoch: Option<Epoch>,
        stake_delegations: &impl StakeAccountsReader,
        load_vote_accounts: impl FnOnce(&DelegatedStakes) -> VoteAccounts + Send,
    ) -> EpochBoundaryStakes {
        // Wrap up the prev epoch by adding new stake history entry for the
        // prev epoch.
        let identity = || {
            (
                StakeActivationStatus::default(),
                HashMap::default(),
            )
        };
        let (stake_history_entry, effective_delegated_stakes) =
            thread_pool.install(|| {
                stake_delegations.fold(
                        identity,
                        |(acc, mut delegated_stakes),
                         _stake_pubkey, stake_account| {
                            let delegation = stake_account.delegation();
                            let activation_status = delegation.stake_activating_and_deactivating_v2(
                                epoch,
                                &stake_history,
                                new_rate_activation_epoch,
                            );
                            *delegated_stakes.entry(delegation.voter_pubkey).or_default() +=
                                activation_status.effective;
                            (acc + activation_status, delegated_stakes)
                        },
                        |(activation_status_a, delegated_stakes_a),
                         (activation_status_b, delegated_stakes_b)| {
                            (
                                activation_status_a + activation_status_b,
                                merge_delegated_stakes(delegated_stakes_a, delegated_stakes_b),
                            )
                        },
                    )
            });
        stake_history.add(epoch, stake_history_entry);
        // Refresh the stake distribution of vote accounts for the next epoch,
        // using new stake history.
        let delegated_stakes = thread_pool.install(|| Self::calculate_delegated_stakes(
            stake_delegations, next_epoch, &stake_history, new_rate_activation_epoch,
        ));
        let vote_accounts = thread_pool.install(|| load_vote_accounts(&delegated_stakes));
        let reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
            epoch,
            delegated_stakes: effective_delegated_stakes,
        };
        EpochBoundaryStakes {
            stake_history,
            vote_accounts,
            reward_epoch_delegated_stakes,
        }
    }

    pub(crate) fn calculate_delegated_stakes(
        stake_delegations: &impl StakeAccountsReader,
        epoch: Epoch,
        stake_history: &StakeHistory,
        new_rate_activation_epoch: Option<Epoch>,
    ) -> DelegatedStakes {
        stake_delegations.fold(DelegatedStakes::default, |mut delegated_stakes, _, stake_account| {
            let delegation = stake_account.delegation();
            let stake = delegation.stake_v2(epoch, stake_history, new_rate_activation_epoch);
            if stake != 0 {
                *delegated_stakes.entry(delegation.voter_pubkey).or_default() += stake;
            }
            delegated_stakes
        }, |mut a, b| {
            for (key, value) in b { *a.entry(key).or_default() += value; }
            a
        })
    }

    /// Borrow the stake table directly, including for native parallel iteration.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn stake_delegations(&self) -> &StakesMap<StakeAccount> {
        &self.stake_delegations
    }


}

/// Macro to generate `From<Stakes<From>> for Stakes<To>` impls.
#[cfg(feature = "dev-context-only-utils")]
macro_rules! impl_stake_format_conversion {
    ($from:ty, $to:ty, |$binding:ident| $expr:expr) => {
        /// This conversion is memory intensive so should only be used in development contexts.
        impl From<Stakes<$from>> for Stakes<$to> {
            fn from(stakes: Stakes<$from>) -> Self {
                let Stakes {
                    vote_accounts,
                    stake_delegations,
                    delegated_stakes: _,
                    unused,
                    epoch,
                    stake_history,
                } = stakes;
                let stake_delegations = stake_delegations
                    .into_iter()
                    .map(|(pubkey, $binding)| (pubkey, $expr))
                    .collect();
                Self {
                    vote_accounts,
                    stake_delegations,
                    delegated_stakes: DelegatedStakes::default(),
                    unused,
                    epoch,
                    stake_history,
                }
            }
        }
    };
}

#[cfg(feature = "dev-context-only-utils")]
impl_stake_format_conversion!(StakeAccount, Delegation, |sa| *sa.delegation());

#[cfg(feature = "dev-context-only-utils")]
impl_stake_format_conversion!(StakeAccount, Stake, |sa| *sa.stake());

#[cfg(feature = "dev-context-only-utils")]
impl_stake_format_conversion!(Stake, Delegation, |stake| stake.delegation);

pub(crate) fn is_delegation_inert<T: StakeHistoryGetEntry>(
    delegation: &Delegation,
    epoch: Epoch,
    stake_history: &T,
    new_rate_activation_epoch: Option<Epoch>,
) -> bool {
    let activation_status = delegation.stake_activating_and_deactivating_v2(
        epoch,
        stake_history,
        new_rate_activation_epoch,
    );
    activation_status.effective == 0 && activation_status.activating == 0
}

fn merge_delegated_stakes(
    mut stakes: HashMap</*voter:*/ Pubkey, /*stake:*/ u64>,
    other: HashMap</*voter:*/ Pubkey, /*stake:*/ u64>,
) -> HashMap</*voter:*/ Pubkey, /*stake:*/ u64> {
    if stakes.len() < other.len() {
        return merge_delegated_stakes(other, stakes);
    }
    for (pubkey, stake) in other {
        *stakes.entry(pubkey).or_default() += stake;
    }
    stakes
}



fn merge_stake_accounts(mut accounts: StakesMap<StakeAccount>, other: StakesMap<StakeAccount>) -> StakesMap<StakeAccount> {
    if accounts.len() < other.len() {
        return merge_stake_accounts(other, accounts);
    }
    accounts.extend(other);
    accounts
}

fn refresh_vote_accounts(
    thread_pool: &ThreadPool,
    epoch: Epoch,
    vote_accounts: &VoteAccounts,
    stake_delegations: &StakesMap<StakeAccount>,
    stake_history: &StakeHistory,
    new_rate_activation_epoch: Option<Epoch>,
) -> (VoteAccounts, DelegatedStakes) {
    fn merge(mut stakes: DelegatedStakes, other: DelegatedStakes) -> DelegatedStakes {
        if stakes.len() < other.len() {
            return merge(other, stakes);
        }
        for (pubkey, stake) in other {
            *stakes.entry(pubkey).or_default() += stake;
        }
        stakes
    }
    let delegated_stakes = thread_pool.install(|| {
        stake_delegations
            .par_iter()
            .fold(
                DelegatedStakes::default,
                |mut delegated_stakes, (_stake_pubkey, stake_account)| {
                    let delegation = stake_account.delegation();
                    let stake =
                        delegation.stake_v2(epoch, stake_history, new_rate_activation_epoch);
                    if stake != 0 {
                        *delegated_stakes.entry(delegation.voter_pubkey).or_default() += stake;
                    }
                    delegated_stakes
                },
            )
            .reduce(DelegatedStakes::default, merge)
    });
    let vote_accounts = vote_accounts
        .iter()
        .map(|(&vote_pubkey, vote_account)| {
            let delegated_stake = delegated_stakes
                .get(&vote_pubkey)
                .copied()
                .unwrap_or_default();
            (vote_pubkey, (delegated_stake, vote_account.clone()))
        })
        .collect();
    (vote_accounts, delegated_stakes)
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::stake_utils,
        rayon::ThreadPoolBuilder,
        solana_account::state_traits::StateMutWincode as _,
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_stake_history::StakeHistoryEntry,
        solana_stake_interface::{self as stake, state::StakeStateV2},
        solana_vote_interface::state::{BLS_PUBLIC_KEY_COMPRESSED_SIZE, VoteStateV4},
        solana_vote_program::vote_state,
    };

    //  set up some dummies for a staked node
    pub(crate) fn create_staked_node_accounts_with_activation_and_deactivation_epochs(
        stake: u64,
        rent: &Rent,
        activation_epoch: Epoch,
        deactivation_epoch: Epoch,
    ) -> ((Pubkey, AccountSharedData), (Pubkey, AccountSharedData)) {
        let vote_pubkey = solana_pubkey::new_rand();
        let node_pubkey = solana_pubkey::new_rand();
        let vote_account = vote_state::create_v4_account_with_authorized(
            &node_pubkey,
            &vote_pubkey,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &vote_pubkey,
            0,
            &vote_pubkey,
            0,
            &node_pubkey,
            rent.minimum_balance(VoteStateV4::size_of()),
        );
        let stake_pubkey = solana_pubkey::new_rand();
        (
            (vote_pubkey, vote_account),
            (
                stake_pubkey,
                create_stake_account_with_activation_and_deactivation_epochs(
                    stake,
                    &vote_pubkey,
                    &stake_pubkey,
                    rent,
                    activation_epoch,
                    deactivation_epoch,
                ),
            ),
        )
    }

    //  set up some dummies for a staked node     ((     vote      )  (     stake     ))
    pub(crate) fn create_staked_node_accounts(
        stake: u64,
        rent: &Rent,
    ) -> ((Pubkey, AccountSharedData), (Pubkey, AccountSharedData)) {
        create_staked_node_accounts_with_activation_and_deactivation_epochs(
            stake,
            rent,
            Epoch::MAX,
            Epoch::MAX,
        )
    }

    //   add stake to a vote_pubkey
    pub(crate) fn create_stake_account_with_activation_and_deactivation_epochs(
        stake: u64,
        vote_pubkey: &Pubkey,
        stake_pubkey: &Pubkey,
        rent: &Rent,
        activation_epoch: Epoch,
        deactivation_epoch: Epoch,
    ) -> AccountSharedData {
        let node_pubkey = solana_pubkey::new_rand();
        let lamports = rent.minimum_balance(StakeStateV2::size_of()) + stake;
        stake_utils::create_stake_account_with_activation_and_deactivation_epochs(
            stake_pubkey,
            vote_pubkey,
            &vote_state::create_v4_account_with_authorized(
                &node_pubkey,
                vote_pubkey,
                [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                vote_pubkey,
                0,
                vote_pubkey,
                0,
                &node_pubkey,
                1,
            ),
            rent,
            lamports,
            activation_epoch,
            deactivation_epoch,
        )
    }

    //   add stake to a vote_pubkey                               (   stake    )
    pub(crate) fn create_stake_account(
        stake: u64,
        vote_pubkey: &Pubkey,
        stake_pubkey: &Pubkey,
        rent: &Rent,
    ) -> AccountSharedData {
        let node_pubkey = solana_pubkey::new_rand();
        let lamports = rent.minimum_balance(StakeStateV2::size_of()) + stake;
        stake_utils::create_stake_account(
            stake_pubkey,
            vote_pubkey,
            &vote_state::create_v4_account_with_authorized(
                &node_pubkey,
                vote_pubkey,
                [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
                vote_pubkey,
                0,
                vote_pubkey,
                0,
                &node_pubkey,
                1,
            ),
            rent,
            lamports,
        )
    }

    #[test]
    fn test_reward_accounts_calculate_epoch_stakes() {
        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &Rent::default());
        let stake_state: StakeStateV2 = stake_account.state().unwrap();
        let stake = stake_state.stake().unwrap();
        let accounts = solana_accounts_db::stakes_cache::StakesCache::new(
            [(stake_pubkey, stake_account)].into_iter().collect(),
            [(vote_pubkey, vote_account)].into_iter().collect(),
        );
        let stakes = Stakes::from_reward_accounts(0, StakeHistory::default(), &accounts, None);
        let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
        let boundary = stakes.calculate_activated_stake(
            3, &thread_pool, None, stakes.stake_delegations(),
        );
        assert_eq!(
            boundary.reward_epoch_delegated_stakes.delegated_stakes.get(&vote_pubkey),
            Some(&stake.stake_v2(0, stakes.history(), None)),
        );
        assert_eq!(
            boundary.vote_accounts.get_delegated_stake(&vote_pubkey),
            stake.stake_v2(3, &boundary.stake_history, None),
        );
    }

    #[test]
    fn test_load_from_accounts_keeps_only_delegated_stake_accounts() {
        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &Rent::default());
        let undelegated_pubkey = Pubkey::new_unique();
        let undelegated_account =
            AccountSharedData::new(1, StakeStateV2::size_of(), &stake::program::id());
        let other_pubkey = Pubkey::new_unique();
        let other_account = AccountSharedData::new(1, 0, &Pubkey::default());
        let missing_pubkey = Pubkey::new_unique();
        let accounts = [
            (vote_pubkey, vote_account),
            (stake_pubkey, stake_account),
            (undelegated_pubkey, undelegated_account),
            (other_pubkey, other_account),
        ];
        let get_account = |pubkey: &Pubkey| {
            accounts
                .iter()
                .find(|(key, _)| key == pubkey)
                .map(|(_, account)| account.clone())
        };
        // The list may repeat a key and may include accounts that are
        // not delegations.
        let stake_pubkeys = vec![
            stake_pubkey,
            stake_pubkey,
            undelegated_pubkey,
            other_pubkey,
            missing_pubkey,
        ];
        let pubkeys_by_owner = [
            (stake::program::id(), stake_pubkeys),
            (solana_vote_program::id(), vec![vote_pubkey]),
        ]
        .into_iter()
        .collect();
        let stakes = Stakes::load_from_accounts(
            0,
            StakeHistory::default(),
            None,
            pubkeys_by_owner,
            get_account,
        );
        assert_eq!(
            stakes.stake_delegations().keys().collect::<Vec<_>>(),
            vec![&stake_pubkey]
        );
    }

    #[test]
    fn test_load_from_accounts_rebuilds_vote_accounts() {
        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &Rent::default());
        let uninitialized_pubkey = Pubkey::new_unique();
        let uninitialized_account =
            AccountSharedData::new(1, VoteStateV4::size_of(), &solana_vote_program::id());
        let wrong_size_pubkey = Pubkey::new_unique();
        let wrong_size_account = AccountSharedData::new(1, 1, &solana_vote_program::id());
        let other_pubkey = Pubkey::new_unique();
        let other_account = AccountSharedData::new(1, 0, &Pubkey::default());
        let missing_pubkey = Pubkey::new_unique();
        let accounts = [
            (vote_pubkey, vote_account),
            (stake_pubkey, stake_account),
            (uninitialized_pubkey, uninitialized_account),
            (wrong_size_pubkey, wrong_size_account),
            (other_pubkey, other_account),
        ];
        let get_account = |pubkey: &Pubkey| {
            accounts
                .iter()
                .find(|(key, _)| key == pubkey)
                .map(|(_, account)| account.clone())
        };
        let mut stake_history = StakeHistory::default();
        stake_history.add(0, StakeHistoryEntry::with_effective(10));
        // The list may repeat a key and may include accounts that are
        // not valid vote accounts.
        let vote_pubkeys = vec![
            vote_pubkey,
            vote_pubkey,
            uninitialized_pubkey,
            wrong_size_pubkey,
            other_pubkey,
            missing_pubkey,
        ];
        let pubkeys_by_owner = [
            (stake::program::id(), vec![stake_pubkey]),
            (solana_vote_program::id(), vote_pubkeys),
        ]
        .into_iter()
        .collect();
        let stakes = Stakes::load_from_accounts(
            1,
            stake_history.clone(),
            None,
            pubkeys_by_owner,
            get_account,
        );
        assert_eq!(
            stakes
                .vote_accounts()
                .iter()
                .map(|(pubkey, _)| *pubkey)
                .collect::<Vec<_>>(),
            vec![vote_pubkey]
        );
        assert_eq!(stakes.vote_accounts().get_delegated_stake(&vote_pubkey), 10);
        assert_eq!(stakes.delegated_stakes.get(&vote_pubkey), Some(&10));
        assert_eq!(stakes.epoch, 1);
        assert_eq!(stakes.history(), &stake_history);
    }

    #[test]
    fn test_load_from_accounts_keeps_vote_account_without_stake() {
        let ((vote_pubkey, vote_account), _) = create_staked_node_accounts(10, &Rent::default());
        let get_account = |pubkey: &Pubkey| (*pubkey == vote_pubkey).then(|| vote_account.clone());
        let pubkeys_by_owner = [(solana_vote_program::id(), vec![vote_pubkey])]
            .into_iter()
            .collect();
        let stakes = Stakes::load_from_accounts(
            0,
            StakeHistory::default(),
            None,
            pubkeys_by_owner,
            get_account,
        );
        assert!(stakes.vote_accounts().get(&vote_pubkey).is_some());
        assert_eq!(stakes.vote_accounts().get_delegated_stake(&vote_pubkey), 0);
        assert!(stakes.delegated_stakes.is_empty());
    }

    #[test]
    fn test_load_from_accounts_keeps_delegation_without_vote_account() {
        let ((vote_pubkey, _), (stake_pubkey, stake_account)) =
            create_staked_node_accounts(10, &Rent::default());
        let get_account =
            |pubkey: &Pubkey| (*pubkey == stake_pubkey).then(|| stake_account.clone());
        let pubkeys_by_owner = [(stake::program::id(), vec![stake_pubkey])]
            .into_iter()
            .collect();
        let stakes = Stakes::load_from_accounts(
            0,
            StakeHistory::default(),
            None,
            pubkeys_by_owner,
            get_account,
        );
        assert!(stakes.stake_delegations().contains_key(&stake_pubkey));
        assert_eq!(stakes.delegated_stakes.get(&vote_pubkey), Some(&10));
        assert!(stakes.vote_accounts().get(&vote_pubkey).is_none());
    }

    #[test]
    fn test_load_from_accounts_uses_new_rate_activation_epoch() {
        let ((vote_pubkey, vote_account), (stake_pubkey, stake_account)) =
            create_staked_node_accounts_with_activation_and_deactivation_epochs(
                100,
                &Rent::default(),
                0,
                Epoch::MAX,
            );
        let accounts = [
            (vote_pubkey, vote_account),
            (stake_pubkey, stake_account.clone()),
        ];
        let get_account = |pubkey: &Pubkey| {
            accounts
                .iter()
                .find(|(key, _)| key == pubkey)
                .map(|(_, account)| account.clone())
        };
        // A stake still warming up: the rate decides how much of it is
        // effective at epoch 1.
        let mut stake_history = StakeHistory::default();
        stake_history.add(
            0,
            StakeHistoryEntry::with_effective_and_activating(1000, 1000),
        );
        let delegation = *StakeAccount::try_from(stake_account).unwrap().delegation();
        let load = |new_rate_activation_epoch| {
            let pubkeys_by_owner = [
                (stake::program::id(), vec![stake_pubkey]),
                (solana_vote_program::id(), vec![vote_pubkey]),
            ]
            .into_iter()
            .collect();
            Stakes::load_from_accounts(
                1,
                stake_history.clone(),
                new_rate_activation_epoch,
                pubkeys_by_owner,
                get_account,
            )
            .vote_accounts()
            .get_delegated_stake(&vote_pubkey)
        };
        let old_rate = delegation.stake_v2(1, &stake_history, None);
        let new_rate = delegation.stake_v2(1, &stake_history, Some(0));
        assert_ne!(old_rate, new_rate);
        assert_eq!(load(None), old_rate);
        assert_eq!(load(Some(0)), new_rate);
    }
}
