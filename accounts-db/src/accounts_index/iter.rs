use {
    super::{AccountsIndex, DiskIndexValue, IndexValue, in_mem_accounts_index::InMemAccountsIndex},
    solana_pubkey::Pubkey,
    std::sync::Arc,
};

pub const ITER_BATCH_SIZE: usize = 1000;

pub struct AccountsIndexPubkeyIterator<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    account_maps: &'a [Arc<InMemAccountsIndex<T, U>>],
    current_bin: usize,
    items: Vec<Pubkey>,
    iter_order: AccountsIndexPubkeyIterOrder,
}

impl<'a, T: IndexValue, U: DiskIndexValue + From<T> + Into<T>>
    AccountsIndexPubkeyIterator<'a, T, U>
{
    pub fn new(index: &'a AccountsIndex<T, U>, iter_order: AccountsIndexPubkeyIterOrder) -> Self {
        Self {
            account_maps: &index.account_maps,
            current_bin: 0,
            items: Vec::new(),
            iter_order,
        }
    }
}

/// Implement the Iterator trait for AccountsIndexIterator
impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> Iterator
    for AccountsIndexPubkeyIterator<'_, T, U>
{
    type Item = Vec<Pubkey>;
    fn next(&mut self) -> Option<Self::Item> {
        while self.items.len() < ITER_BATCH_SIZE {
            if self.current_bin >= self.account_maps.len() {
                break;
            }

            let map = &self.account_maps[self.current_bin];
            let mut items = map.keys();
            self.items.append(&mut items);
            self.current_bin += 1;
        }

        if self.iter_order == AccountsIndexPubkeyIterOrder::Sorted {
            self.items.sort_unstable();
        }

        (!self.items.is_empty()).then(|| std::mem::take(&mut self.items))
    }
}

/// Specify how the accounts index pubkey iterator should return pubkeys
///
/// Users should prefer `Unsorted`, unless required otherwise,
/// as sorting incurs additional runtime cost.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AccountsIndexPubkeyIterOrder {
    /// Returns pubkeys *not* sorted
    Unsorted,
    /// Returns pubkeys *sorted*
    Sorted,
}

#[cfg(test)]
mod tests {
    use {
        super::{
            super::{UpsertReclaim, secondary::AccountSecondaryIndexes},
            *,
        },
        crate::accounts_index::ReclaimsSlotList,
        solana_account::AccountSharedData,
        std::iter,
    };

    #[test]
    fn test_account_index_iter_batched() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        // this test requires the index to have more than one bin
        assert!(index.bins() > 1);
        // ensure each bin ends up with more than ITER_BATCH_SIZE items
        let num_pubkeys = ITER_BATCH_SIZE * (index.bins() + 1);
        let pubkeys = iter::repeat_with(solana_pubkey::new_rand)
            .take(num_pubkeys)
            .collect::<Vec<_>>();

        for key in pubkeys {
            let slot = 0;
            let value = true;
            let mut gc = ReclaimsSlotList::new();
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut gc,
                UpsertReclaim::PopulateReclaims,
            );
        }

        for iter_order in [
            AccountsIndexPubkeyIterOrder::Sorted,
            AccountsIndexPubkeyIterOrder::Unsorted,
        ] {
            let mut iter = index.iter(iter_order);
            // First iter.next() should return at least the batch size.
            let x = iter.next().unwrap();
            assert!(x.len() >= ITER_BATCH_SIZE);
            assert_eq!(
                x.is_sorted(),
                iter_order == AccountsIndexPubkeyIterOrder::Sorted
            );
            assert_eq!(iter.items.len(), 0); // should be empty.

            // Second iter.next() should return all remaining items.
            let num_remaining = num_pubkeys - x.len();
            let y = iter.next().unwrap();
            assert_eq!(y.len(), num_remaining);

            // Third iter.next() should return None.
            assert!(iter.next().is_none());
        }
    }

    #[test]
    fn test_accounts_iter_finished() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        index.add_root(0);
        let mut iter = index.iter(AccountsIndexPubkeyIterOrder::Sorted);
        assert!(iter.next().is_none());
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &solana_pubkey::new_rand(),
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );
        assert!(iter.next().is_none());
    }
}
