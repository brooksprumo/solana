//! Rooted stake and vote accounts used by epoch rewards, independently of RPC indexes.
use {
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_sdk_ids::{stake, vote},
};

/// Hash tables with native parallel iteration over their buckets.
pub type StakesMap<T> = hashbrown::HashMap<Pubkey, T, ahash::RandomState>;

/// All live accounts owned by the stake and vote programs. State validation belongs
/// to the consumer: uninitialized and otherwise undecodable accounts are retained.
#[derive(Debug, Default)]
pub struct StakesCache {
    pub stake_accounts: StakesMap<(Slot, AccountSharedData)>,
    pub vote_accounts: StakesMap<(Slot, AccountSharedData)>,
}

impl StakesCache {
    pub fn new(stake_accounts: StakesMap<AccountSharedData>, vote_accounts: StakesMap<AccountSharedData>) -> Self {
        Self {
            stake_accounts: stake_accounts.into_iter().map(|(key, account)| (key, (0, account))).collect(),
            vote_accounts: vote_accounts.into_iter().map(|(key, account)| (key, (0, account))).collect(),
        }
    }

    pub(crate) fn update(&mut self, pubkey: &Pubkey, slot: Slot, account: &AccountSharedData) {
        if account.lamports() != 0 && stake::check_id(account.owner()) {
            self.vote_accounts.remove(pubkey);
            self.stake_accounts.insert(*pubkey, (slot, account.clone()));
        } else if account.lamports() != 0 && vote::check_id(account.owner()) {
            self.stake_accounts.remove(pubkey);
            self.vote_accounts.insert(*pubkey, (slot, account.clone()));
        } else {
            // Closures and owner changes must also invalidate old entries.
            self.stake_accounts.remove(pubkey);
            self.vote_accounts.remove(pubkey);
        }
    }
}
