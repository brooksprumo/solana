use {
    super::Bank,
    crate::stake_account::StakeAccount,
    rayon::prelude::*,
    solana_accounts_db::stakes_cache::StakesMap,
    solana_pubkey::Pubkey,
    solana_stake_interface::{program, state::Delegation},
};

/// Repeatable stake-account inputs; accounts are decoded on the stack as they are read.
/// Visible banks must not be written to while a scan is in progress.
pub trait StakeAccountsReader: Sync {
    fn fold<T: Send>(
        &self,
        identity: impl Fn() -> T + Send + Sync,
        fold: impl Fn(T, &Pubkey, &StakeAccount<Delegation>) -> T + Send + Sync,
        reduce: impl Fn(T, T) -> T + Send + Sync,
    ) -> T;

    fn get(&self, pubkey: &Pubkey) -> Option<StakeAccount<Delegation>>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn len(&self) -> usize {
        self.fold(|| 0, |count, _, _| count + 1, |a, b| a + b)
    }
}

#[derive(Clone, Copy)]
pub struct BankStakeAccounts<'a> {
    pub(super) bank: &'a Bank,
}

impl StakeAccountsReader for BankStakeAccounts<'_> {
    fn fold<T: Send>(
        &self,
        identity: impl Fn() -> T + Send + Sync,
        fold: impl Fn(T, &Pubkey, &StakeAccount<Delegation>) -> T + Send + Sync,
        reduce: impl Fn(T, T) -> T + Send + Sync,
    ) -> T {
        self.bank.rc.accounts.accounts_db.fold_reward_accounts(&self.bank.ancestors, &program::id(),
            identity, |acc, key, account| {
                match StakeAccount::from_account_if_delegated(account) {
                    Some(account) => fold(acc, key, &account),
                    None => acc,
                }
            }, reduce)
    }

    fn get(&self, pubkey: &Pubkey) -> Option<StakeAccount<Delegation>> {
        StakeAccount::from_account_if_delegated(self.bank.get_account_do_not_populate_read_cache(pubkey)?)
    }
}

impl StakeAccountsReader for StakesMap<StakeAccount<Delegation>> {
    fn fold<T: Send>(
        &self,
        identity: impl Fn() -> T + Send + Sync,
        fold: impl Fn(T, &Pubkey, &StakeAccount<Delegation>) -> T + Send + Sync,
        reduce: impl Fn(T, T) -> T + Send + Sync,
    ) -> T {
        self.par_iter().fold(&identity, |acc, (key, account)| fold(acc, key, account)).reduce(&identity, reduce)
    }

    fn get(&self, pubkey: &Pubkey) -> Option<StakeAccount<Delegation>> {
        StakesMap::get(self, pubkey).cloned()
    }

    fn len(&self) -> usize {
        StakesMap::len(self)
    }
}
