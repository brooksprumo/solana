use {
    rocksdb::{ColumnFamilyDescriptor, ColumnFamilyRef, DB, IteratorMode, Options, WriteBatch},
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    std::{
        cmp::Ordering,
        collections::{hash_map::Entry, HashMap},
        fmt,
        path::{Path, PathBuf},
        sync::Arc,
    },
};

const ACCOUNT_META_CF: &str = "account_meta";
const ACCOUNT_DATA_CF: &str = "account_data";
const DB_META_CF: &str = "db_meta";
pub(crate) const ACCOUNT_DATA_MIN_BLOB_SIZE: u64 = 1024;

const ACCOUNT_META_LEN: usize = 8 + 32 + 8 + 1 + 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountMeta {
    pub slot: Slot,
    pub owner: Pubkey,
    pub lamports: u64,
    pub executable: bool,
    pub rent_epoch: Epoch,
}

impl AccountMeta {
    fn from_account(slot: Slot, account: &impl ReadableAccount) -> Self {
        Self {
            slot,
            owner: *account.owner(),
            lamports: account.lamports(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
        }
    }

    fn encode(&self) -> [u8; ACCOUNT_META_LEN] {
        let mut out = [0; ACCOUNT_META_LEN];
        out[..8].copy_from_slice(&self.slot.to_le_bytes());
        out[8..40].copy_from_slice(self.owner.as_ref());
        out[40..48].copy_from_slice(&self.lamports.to_le_bytes());
        out[48] = u8::from(self.executable);
        out[49..57].copy_from_slice(&self.rent_epoch.to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != ACCOUNT_META_LEN {
            return None;
        }

        let slot = u64::from_le_bytes(bytes[..8].try_into().ok()?);
        let owner = Pubkey::new_from_array(bytes[8..40].try_into().ok()?);
        let lamports = u64::from_le_bytes(bytes[40..48].try_into().ok()?);
        let executable = match bytes[48] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let rent_epoch = u64::from_le_bytes(bytes[49..57].try_into().ok()?);

        Some(Self {
            slot,
            owner,
            lamports,
            executable,
            rent_epoch,
        })
    }

    fn into_account(self, data: Vec<u8>) -> AccountSharedData {
        AccountSharedData::create_from_existing_shared_data(
            self.lamports,
            Arc::new(data),
            self.owner,
            self.executable,
            self.rent_epoch,
        )
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RocksAccountsStoreStats {
    pub accounts_written: usize,
    pub accounts_skipped: usize,
    pub metadata_values_written: usize,
    pub data_values_written: usize,
    pub data_values_deleted: usize,
    pub data_bytes_written: u64,
}

impl RocksAccountsStoreStats {
    pub(crate) fn accumulate(&mut self, other: Self) {
        self.accounts_written += other.accounts_written;
        self.accounts_skipped += other.accounts_skipped;
        self.metadata_values_written += other.metadata_values_written;
        self.data_values_written += other.data_values_written;
        self.data_values_deleted += other.data_values_deleted;
        self.data_bytes_written += other.data_bytes_written;
    }
}

pub(crate) struct RocksAccountsDb {
    db: DB,
    path: PathBuf,
}

impl fmt::Debug for RocksAccountsDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocksAccountsDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl RocksAccountsDb {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, rocksdb::Error> {
        let path = path.as_ref().to_path_buf();
        let mut db_options = Options::default();
        db_options.create_if_missing(true);
        db_options.create_missing_column_families(true);
        db_options.set_max_background_jobs(4);

        let db = DB::open_cf_descriptors(
            &db_options,
            &path,
            [
                ColumnFamilyDescriptor::new(ACCOUNT_META_CF, Self::account_meta_options()),
                ColumnFamilyDescriptor::new(ACCOUNT_DATA_CF, Self::account_data_options()),
                ColumnFamilyDescriptor::new(DB_META_CF, Self::db_meta_options()),
            ],
        )?;

        Ok(Self { db, path })
    }

    fn account_meta_options() -> Options {
        let mut options = Options::default();
        options.set_write_buffer_size(64 * 1024 * 1024);
        options
    }

    fn account_data_options() -> Options {
        let mut options = Options::default();
        options.set_write_buffer_size(256 * 1024 * 1024);
        options.set_enable_blob_files(true);
        options.set_min_blob_size(ACCOUNT_DATA_MIN_BLOB_SIZE);
        options
    }

    fn db_meta_options() -> Options {
        let mut options = Options::default();
        options.set_write_buffer_size(1024 * 1024);
        options
    }

    fn cf(&self, name: &str) -> ColumnFamilyRef<'_> {
        self.db
            .cf_handle(name)
            .unwrap_or_else(|| panic!("missing RocksDB column family {name}"))
    }

    fn meta_cf(&self) -> ColumnFamilyRef<'_> {
        self.cf(ACCOUNT_META_CF)
    }

    fn data_cf(&self) -> ColumnFamilyRef<'_> {
        self.cf(ACCOUNT_DATA_CF)
    }

    pub(crate) fn load_meta(&self, pubkey: &Pubkey) -> Result<Option<AccountMeta>, rocksdb::Error> {
        self.db
            .get_cf(&self.meta_cf(), pubkey.as_ref())
            .map(|maybe_meta| {
                maybe_meta.map(|meta| {
                    AccountMeta::decode(&meta)
                        .unwrap_or_else(|| panic!("invalid Rocks account metadata for {pubkey}"))
                })
            })
    }

    fn data_matches(&self, pubkey: &Pubkey, data: &[u8]) -> Result<bool, rocksdb::Error> {
        self.db
            .get_cf(&self.data_cf(), pubkey.as_ref())
            .map(|old_data| old_data.as_deref().unwrap_or_default() == data)
    }

    fn add_account_to_batch(
        &self,
        batch: &mut WriteBatch,
        pubkey: &Pubkey,
        slot: Slot,
        account: &impl ReadableAccount,
        stats: &mut RocksAccountsStoreStats,
        compare_existing_data: bool,
    ) -> Result<(), rocksdb::Error> {
        let meta = AccountMeta::from_account(slot, account).encode();
        batch.put_cf(&self.meta_cf(), pubkey.as_ref(), meta);
        stats.accounts_written += 1;
        stats.metadata_values_written += 1;

        let data = account.data();
        if account.lamports() == 0 || data.is_empty() {
            batch.delete_cf(&self.data_cf(), pubkey.as_ref());
            stats.data_values_deleted += 1;
        } else if !compare_existing_data || !self.data_matches(pubkey, data)? {
            batch.put_cf(&self.data_cf(), pubkey.as_ref(), data);
            stats.data_values_written += 1;
            stats.data_bytes_written += data.len() as u64;
        }

        Ok(())
    }

    pub(crate) fn store_accounts<'a>(
        &self,
        slot: Slot,
        accounts: impl IntoIterator<Item = (&'a Pubkey, &'a AccountSharedData)>,
    ) -> Result<RocksAccountsStoreStats, rocksdb::Error> {
        let mut batch = WriteBatch::default();
        let mut stats = RocksAccountsStoreStats::default();
        for (pubkey, account) in accounts {
            self.add_account_to_batch(&mut batch, pubkey, slot, account, &mut stats, true)?;
        }

        if stats.accounts_written > 0 {
            self.db.write_without_wal(batch)?;
        }

        Ok(stats)
    }

    pub(crate) fn store_accounts_with_slots<'a>(
        &self,
        accounts: impl IntoIterator<Item = (&'a Pubkey, Slot, &'a AccountSharedData)>,
    ) -> Result<RocksAccountsStoreStats, rocksdb::Error> {
        let mut batch = WriteBatch::default();
        let mut stats = RocksAccountsStoreStats::default();
        for (pubkey, slot, account) in accounts {
            self.add_account_to_batch(&mut batch, pubkey, slot, account, &mut stats, true)?;
        }

        if stats.accounts_written > 0 {
            self.db.write_without_wal(batch)?;
        }
        Ok(stats)
    }

    pub(crate) fn store_startup_accounts_with_slots<'a>(
        &self,
        accounts: impl IntoIterator<Item = (&'a Pubkey, Slot, &'a AccountSharedData)>,
    ) -> Result<RocksAccountsStoreStats, rocksdb::Error> {
        let mut latest_accounts = HashMap::<Pubkey, (Slot, &'a AccountSharedData)>::new();
        let mut stats = RocksAccountsStoreStats::default();

        for (pubkey, slot, account) in accounts {
            match latest_accounts.entry(*pubkey) {
                Entry::Vacant(entry) => {
                    entry.insert((slot, account));
                }
                Entry::Occupied(mut entry) => match slot.cmp(&entry.get().0) {
                    Ordering::Greater => {
                        stats.accounts_skipped += 1;
                        entry.insert((slot, account));
                    }
                    Ordering::Less => {
                        stats.accounts_skipped += 1;
                    }
                    Ordering::Equal => {
                        panic!("Accounts may only be stored once per slot: ({slot}, {pubkey})");
                    }
                },
            }
        }

        let mut batch = WriteBatch::default();
        for (pubkey, (slot, account)) in latest_accounts {
            match self.load_meta(&pubkey)? {
                Some(existing_meta) => match slot.cmp(&existing_meta.slot) {
                    Ordering::Greater => {}
                    Ordering::Less => {
                        stats.accounts_skipped += 1;
                        continue;
                    }
                    Ordering::Equal => {
                        panic!("Accounts may only be stored once per slot: ({slot}, {pubkey})");
                    }
                },
                None => {}
            }

            self.add_account_to_batch(
                &mut batch,
                &pubkey,
                slot,
                account,
                &mut stats,
                false,
            )?;
        }

        if stats.accounts_written > 0 {
            self.db.write_without_wal(batch)?;
        }
        Ok(stats)
    }

    pub(crate) fn load_account_with_slot(
        &self,
        pubkey: &Pubkey,
    ) -> Result<Option<(AccountSharedData, Slot)>, rocksdb::Error> {
        self.load_account_with_meta(pubkey)
            .map(|maybe_account| maybe_account.map(|(account, meta)| (account, meta.slot)))
    }

    fn load_account_with_meta(
        &self,
        pubkey: &Pubkey,
    ) -> Result<Option<(AccountSharedData, AccountMeta)>, rocksdb::Error> {
        let Some(meta_bytes) = self.db.get_cf(&self.meta_cf(), pubkey.as_ref())? else {
            return Ok(None);
        };
        let meta = AccountMeta::decode(&meta_bytes)
            .unwrap_or_else(|| panic!("invalid Rocks account metadata for {pubkey}"));
        let data = self
            .db
            .get_cf(&self.data_cf(), pubkey.as_ref())?
            .unwrap_or_default();
        let account = meta.clone().into_account(data);
        Ok(Some((account, meta)))
    }

    pub(crate) fn scan_accounts(
        &self,
        mut callback: impl FnMut(&Pubkey, AccountSharedData, Slot),
    ) -> Result<(), rocksdb::Error> {
        let meta_cf = self.meta_cf();
        let data_cf = self.data_cf();
        for item in self.db.iterator_cf(&meta_cf, IteratorMode::Start) {
            let (key, meta_bytes) = item?;
            if key.len() != 32 {
                panic!("invalid Rocks account address key length: {}", key.len());
            }
            let pubkey = Pubkey::new_from_array(
                key.as_ref()
                    .try_into()
                    .expect("Rocks account address key is 32 bytes"),
            );
            let meta = AccountMeta::decode(&meta_bytes)
                .unwrap_or_else(|| panic!("invalid Rocks account metadata for {pubkey}"));
            let data = self
                .db
                .get_cf(&data_cf, pubkey.as_ref())?
                .unwrap_or_default();
            let slot = meta.slot;
            callback(&pubkey, meta.into_account(data), slot);
        }
        Ok(())
    }

    pub(crate) fn count_accounts_in_slot(&self, slot: Slot) -> Result<usize, rocksdb::Error> {
        let meta_cf = self.meta_cf();
        let mut count = 0;
        for item in self.db.iterator_cf(&meta_cf, IteratorMode::Start) {
            let (_key, meta_bytes) = item?;
            let meta = AccountMeta::decode(&meta_bytes)
                .unwrap_or_else(|| panic!("invalid Rocks account metadata"));
            if meta.slot == slot {
                count += 1;
            }
        }
        Ok(count)
    }

    pub(crate) fn contains(&self, pubkey: &Pubkey) -> Result<bool, rocksdb::Error> {
        self.db
            .get_pinned_cf(&self.meta_cf(), pubkey.as_ref())
            .map(|maybe_meta| maybe_meta.is_some())
    }

    pub(crate) fn flush(&self) -> Result<(), rocksdb::Error> {
        self.db.flush_cf(&self.meta_cf())?;
        self.db.flush_cf(&self.data_cf())?;
        self.db.flush_cf(&self.cf(DB_META_CF))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::WritableAccount,
        solana_pubkey::Pubkey,
        tempfile::TempDir,
    };

    #[test]
    fn test_store_and_load_account() {
        let temp_dir = TempDir::new().unwrap();
        let db = RocksAccountsDb::open(temp_dir.path()).unwrap();
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let mut account = AccountSharedData::new(42, 2048, &owner);
        account.set_data_from_slice(&vec![7; 2048]);
        account.set_executable(true);
        account.set_rent_epoch(11);

        let stats = db.store_accounts(7, [(&pubkey, &account)]).unwrap();
        assert_eq!(stats.accounts_written, 1);
        assert_eq!(stats.data_values_written, 1);

        let (loaded, slot) = db.load_account_with_slot(&pubkey).unwrap().unwrap();
        assert_eq!(slot, 7);
        assert_eq!(loaded.lamports(), 42);
        assert_eq!(loaded.owner(), &owner);
        assert!(loaded.executable());
        assert_eq!(loaded.rent_epoch(), 11);
        assert_eq!(loaded.data(), account.data());
    }

    #[test]
    fn test_metadata_only_update_skips_data_write() {
        let temp_dir = TempDir::new().unwrap();
        let db = RocksAccountsDb::open(temp_dir.path()).unwrap();
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let mut account = AccountSharedData::new(42, 2048, &owner);
        account.set_data_from_slice(&vec![9; 2048]);

        let stats = db.store_accounts(3, [(&pubkey, &account)]).unwrap();
        assert_eq!(stats.data_values_written, 1);

        let mut metadata_only_update = account.clone();
        metadata_only_update.set_lamports(43);
        let stats = db
            .store_accounts(4, [(&pubkey, &metadata_only_update)])
            .unwrap();
        assert_eq!(stats.metadata_values_written, 1);
        assert_eq!(stats.data_values_written, 0);

        let (loaded, slot) = db.load_account_with_slot(&pubkey).unwrap().unwrap();
        assert_eq!(slot, 4);
        assert_eq!(loaded.lamports(), 43);
        assert_eq!(loaded.data(), account.data());
    }

    #[test]
    fn test_startup_store_keeps_highest_slot() {
        let temp_dir = TempDir::new().unwrap();
        let db = RocksAccountsDb::open(temp_dir.path()).unwrap();
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let older = AccountSharedData::new(5, 4, &owner);
        let newer = AccountSharedData::new(10, 8, &owner);

        let stats = db
            .store_startup_accounts_with_slots([(&pubkey, 10, &newer), (&pubkey, 5, &older)])
            .unwrap();
        assert_eq!(stats.accounts_written, 1);
        assert_eq!(stats.accounts_skipped, 1);
        let (loaded, slot) = db.load_account_with_slot(&pubkey).unwrap().unwrap();
        assert_eq!(slot, 10);
        assert_eq!(loaded, newer);

        let stats = db
            .store_startup_accounts_with_slots([(&pubkey, 7, &older)])
            .unwrap();
        assert_eq!(stats.accounts_written, 0);
        assert_eq!(stats.accounts_skipped, 1);
        let (loaded, slot) = db.load_account_with_slot(&pubkey).unwrap().unwrap();
        assert_eq!(slot, 10);
        assert_eq!(loaded, newer);
    }

    #[test]
    #[should_panic(expected = "Accounts may only be stored once per slot:")]
    fn test_startup_store_panics_on_same_slot_duplicate() {
        let temp_dir = TempDir::new().unwrap();
        let db = RocksAccountsDb::open(temp_dir.path()).unwrap();
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let account = AccountSharedData::new(5, 4, &owner);

        db.store_startup_accounts_with_slots([(&pubkey, 10, &account), (&pubkey, 10, &account)])
            .unwrap();
    }
}
