use {
    super::Bank,
    rayon::prelude::*,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::{VoteAccount, VoteAccountsReader},
};

/// A lazy reader of current vote accounts, with stake weights fixed by the epoch.
/// Account versions are resolved by normal accounts-db loading.
#[derive(Clone, Copy)]
pub struct BankVoteAccounts<'a> {
    pub(super) bank: &'a Bank,
}

impl BankVoteAccounts<'_> {
    pub fn get(&self, pubkey: &Pubkey) -> Option<(u64, VoteAccount)> {
        let (stake, _) = self.bank.epoch_vote_accounts(self.bank.epoch().saturating_add(1))?.get(pubkey)?;
        Some((*stake, self.bank.get_vote_account(pubkey)?))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Pubkey, (u64, VoteAccount))> + Clone {
        self.bank.epoch_vote_accounts(self.bank.epoch().saturating_add(1)).into_iter()
            .flat_map(|accounts| accounts.keys())
            .filter_map(|pubkey| self.get(pubkey).map(|account| (pubkey, account)))
    }

    pub fn values(&self) -> impl Iterator<Item = (u64, VoteAccount)> + Clone {
        self.iter().map(|(_, account)| account)
    }

    pub fn par_iter(&self) -> impl ParallelIterator<Item = (&Pubkey, (u64, VoteAccount))> {
        self.bank.epoch_vote_accounts(self.bank.epoch().saturating_add(1)).into_par_iter()
            .flat_map(|accounts| accounts.par_iter())
            .filter_map(|(pubkey, _)| self.get(pubkey).map(|account| (pubkey, account)))
    }

    pub fn contains_key(&self, pubkey: &Pubkey) -> bool {
        self.get(pubkey).is_some()
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

impl VoteAccountsReader for BankVoteAccounts<'_> {
    fn iter(&self) -> impl Iterator<Item = (&Pubkey, (u64, VoteAccount))> + Clone {
        BankVoteAccounts::iter(self)
    }
}
