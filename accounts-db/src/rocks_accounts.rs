use {
    rocksdb::{
        BlockBasedOptions, Cache, ColumnFamilyDescriptor, ColumnFamilyRef, DB, Direction,
        IteratorMode, MergeOperands, Options, ReadOptions, WriteBatch,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    std::{
        fmt,
        path::{Path, PathBuf},
        sync::Arc,
    },
};

const ACCOUNT_META_CF: &str = "account_meta";
const ACCOUNT_DATA_CF: &str = "account_data";
pub(crate) const ACCOUNT_DATA_MIN_BLOB_SIZE: u64 = 1024;
const ACCOUNT_META_BLOCK_CACHE_SIZE: usize = 50 * 1024 * 1024 * 1024;
const ACCOUNT_DATA_BLOCK_CACHE_SIZE: usize = 50 * 1024 * 1024 * 1024;
const ACCOUNT_DATA_BLOB_CACHE_SIZE: usize = 10 * 1024 * 1024 * 1024;

const ACCOUNT_META_LEN: usize = 8 + 32 + 8 + 1 + 8;
const ACCOUNT_DATA_VALUE_MAGIC: &[u8; 16] = b"solAcctDataV001!";
const ACCOUNT_DATA_VALUE_HEADER_LEN: usize = ACCOUNT_DATA_VALUE_MAGIC.len() + 8 + 1 + 8;

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

enum AccountDataValue<'a> {
    Present { slot: Slot, data: &'a [u8] },
    Absent { slot: Slot },
    LegacyRaw { data: &'a [u8] },
}

impl<'a> AccountDataValue<'a> {
    fn slot(&self) -> Slot {
        match self {
            Self::Present { slot, .. } | Self::Absent { slot } => *slot,
            Self::LegacyRaw { .. } => 0,
        }
    }
}

fn encode_account_data_value(slot: Slot, data: Option<&[u8]>) -> Vec<u8> {
    let data_len = data.map_or(0, <[u8]>::len);
    let mut out = Vec::with_capacity(ACCOUNT_DATA_VALUE_HEADER_LEN + data_len);
    out.extend_from_slice(ACCOUNT_DATA_VALUE_MAGIC);
    out.extend_from_slice(&slot.to_le_bytes());
    out.push(u8::from(data.is_some()));
    out.extend_from_slice(&(data_len as u64).to_le_bytes());
    if let Some(data) = data {
        out.extend_from_slice(data);
    }
    out
}

fn decode_account_data_value(bytes: &[u8]) -> AccountDataValue<'_> {
    if bytes.len() < ACCOUNT_DATA_VALUE_HEADER_LEN
        || &bytes[..ACCOUNT_DATA_VALUE_MAGIC.len()] != ACCOUNT_DATA_VALUE_MAGIC
    {
        return AccountDataValue::LegacyRaw { data: bytes };
    }

    let slot_start = ACCOUNT_DATA_VALUE_MAGIC.len();
    let present_index = slot_start + 8;
    let len_start = present_index + 1;
    let data_start = len_start + 8;
    let Ok(slot) = bytes[slot_start..present_index].try_into() else {
        return AccountDataValue::LegacyRaw { data: bytes };
    };
    let Ok(data_len) = bytes[len_start..data_start].try_into() else {
        return AccountDataValue::LegacyRaw { data: bytes };
    };
    let slot = u64::from_le_bytes(slot);
    let data_len = u64::from_le_bytes(data_len) as usize;
    let Some(data_end) = data_start.checked_add(data_len) else {
        return AccountDataValue::LegacyRaw { data: bytes };
    };
    if data_end != bytes.len() {
        return AccountDataValue::LegacyRaw { data: bytes };
    }

    match bytes[present_index] {
        0 => AccountDataValue::Absent { slot },
        1 => AccountDataValue::Present {
            slot,
            data: &bytes[data_start..data_end],
        },
        _ => AccountDataValue::LegacyRaw { data: bytes },
    }
}

fn account_data_value_data(bytes: &[u8]) -> &[u8] {
    match decode_account_data_value(bytes) {
        AccountDataValue::Present { data, .. } | AccountDataValue::LegacyRaw { data } => data,
        AccountDataValue::Absent { .. } => &[],
    }
}

fn merge_account_meta(
    _key: &[u8],
    old_value: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    let mut latest_slot = old_value
        .and_then(AccountMeta::decode)
        .map(|meta| meta.slot)
        .unwrap_or_default();
    let mut latest_value = old_value.map(<[u8]>::to_vec);

    for operand in operands {
        let operand_meta = AccountMeta::decode(operand)?;
        if latest_value.is_none() || operand_meta.slot >= latest_slot {
            latest_slot = operand_meta.slot;
            latest_value = Some(operand.to_vec());
        }
    }

    latest_value
}

fn merge_account_data(
    _key: &[u8],
    old_value: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    let mut latest_slot = old_value
        .map(decode_account_data_value)
        .map(|value| value.slot())
        .unwrap_or_default();
    let mut latest_value = old_value.map(<[u8]>::to_vec);

    for operand in operands {
        let operand_value = decode_account_data_value(operand);
        if latest_value.is_none() || operand_value.slot() >= latest_slot {
            latest_slot = operand_value.slot();
            latest_value = Some(operand.to_vec());
        }
    }

    latest_value
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
        let rocks_parallelism = num_cpus::get().max(1).min(i32::MAX as usize) as i32;
        db_options.increase_parallelism(rocks_parallelism);
        db_options.set_max_background_jobs(rocks_parallelism);
        db_options.set_max_subcompactions(rocks_parallelism as u32);

        let mut cf_descriptors = vec![
            ColumnFamilyDescriptor::new(ACCOUNT_META_CF, Self::account_meta_options()),
            ColumnFamilyDescriptor::new(ACCOUNT_DATA_CF, Self::account_data_options()),
        ];

        const LEGACY_DB_META_CF: &str = "db_meta";
        let has_legacy_db_meta_cf = path.exists()
            && DB::list_cf(&db_options, &path)
                .map(|cfs| cfs.iter().any(|cf| cf == LEGACY_DB_META_CF))
                .unwrap_or(false);
        if has_legacy_db_meta_cf {
            cf_descriptors.push(ColumnFamilyDescriptor::new(
                LEGACY_DB_META_CF,
                Options::default(),
            ));
        }

        let mut db = DB::open_cf_descriptors(&db_options, &path, cf_descriptors)?;
        if has_legacy_db_meta_cf {
            db.drop_cf(LEGACY_DB_META_CF)?;
        }

        Ok(Self { db, path })
    }

    fn account_meta_options() -> Options {
        let mut options = Options::default();
        options.set_write_buffer_size(64 * 1024 * 1024);
        options.set_merge_operator_associative("account meta slot merge", merge_account_meta);

        let cache = Cache::new_lru_cache(ACCOUNT_META_BLOCK_CACHE_SIZE);
        let mut block_options = BlockBasedOptions::default();
        block_options.set_block_cache(&cache);
        block_options.set_cache_index_and_filter_blocks(true);
        options.set_block_based_table_factory(&block_options);

        options
    }

    fn account_data_options() -> Options {
        let mut options = Options::default();
        options.set_write_buffer_size(256 * 1024 * 1024);
        options.set_merge_operator_associative("account data slot merge", merge_account_data);
        options.set_enable_blob_files(true);
        options.set_min_blob_size(ACCOUNT_DATA_MIN_BLOB_SIZE);
        let block_cache = Cache::new_lru_cache(ACCOUNT_DATA_BLOCK_CACHE_SIZE);
        let mut block_options = BlockBasedOptions::default();
        block_options.set_block_cache(&block_cache);
        block_options.set_cache_index_and_filter_blocks(true);
        options.set_block_based_table_factory(&block_options);

        let blob_cache = Cache::new_lru_cache(ACCOUNT_DATA_BLOB_CACHE_SIZE);
        options.set_blob_cache(&blob_cache);
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
            .map(|old_data| old_data.as_deref().map(account_data_value_data) == Some(data))
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
            batch.put_cf(
                &self.data_cf(),
                pubkey.as_ref(),
                encode_account_data_value(slot, Some(data)),
            );
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
        let meta_cf = self.meta_cf();
        let data_cf = self.data_cf();
        let mut batch = WriteBatch::default();
        let mut stats = RocksAccountsStoreStats::default();

        for (pubkey, slot, account) in accounts {
            batch.merge_cf(
                &meta_cf,
                pubkey.as_ref(),
                AccountMeta::from_account(slot, account).encode(),
            );
            stats.accounts_written += 1;
            stats.metadata_values_written += 1;

            let data = account.data();
            if account.lamports() == 0 || data.is_empty() {
                batch.merge_cf(&data_cf, pubkey.as_ref(), encode_account_data_value(slot, None));
                stats.data_values_deleted += 1;
            } else {
                batch.merge_cf(
                    &data_cf,
                    pubkey.as_ref(),
                    encode_account_data_value(slot, Some(data)),
                );
                stats.data_values_written += 1;
                stats.data_bytes_written += data.len() as u64;
            };
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
            .map(|data| account_data_value_data(&data).to_vec())
            .unwrap_or_default();
        let account = meta.clone().into_account(data);
        Ok(Some((account, meta)))
    }

    pub(crate) fn scan_accounts(
        &self,
        mut callback: impl FnMut(&Pubkey, AccountSharedData, Slot),
    ) -> Result<(), rocksdb::Error> {
        self.scan_accounts_with_key_range(None, None, |pubkey, account, slot| {
            callback(pubkey, account, slot)
        })
    }

    pub(crate) fn scan_accounts_with_key_range(
        &self,
        lower_bound: Option<Vec<u8>>,
        upper_bound: Option<Vec<u8>>,
        mut callback: impl FnMut(&Pubkey, AccountSharedData, Slot),
    ) -> Result<(), rocksdb::Error> {
        let meta_cf = self.meta_cf();
        let data_cf = self.data_cf();
        let mut read_options = ReadOptions::default();
        if let Some(lower_bound) = lower_bound.as_ref() {
            read_options.set_iterate_lower_bound(lower_bound.clone());
        }
        if let Some(upper_bound) = upper_bound {
            read_options.set_iterate_upper_bound(upper_bound);
        }
        let iterator_mode = lower_bound.as_deref().map_or(IteratorMode::Start, |lower| {
            IteratorMode::From(lower, Direction::Forward)
        });

        for item in self
            .db
            .iterator_cf_opt(&meta_cf, read_options, iterator_mode)
        {
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
                .map(|data| account_data_value_data(&data).to_vec())
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
    fn test_db_meta_column_family_is_not_created() {
        let temp_dir = TempDir::new().unwrap();
        let db = RocksAccountsDb::open(temp_dir.path()).unwrap();
        drop(db);

        let column_families = DB::list_cf(&Options::default(), temp_dir.path()).unwrap();
        assert!(column_families.iter().any(|cf| cf == ACCOUNT_META_CF));
        assert!(column_families.iter().any(|cf| cf == ACCOUNT_DATA_CF));
        assert!(!column_families.iter().any(|cf| cf == "db_meta"));
    }

    #[test]
    fn test_legacy_db_meta_column_family_is_dropped() {
        let temp_dir = TempDir::new().unwrap();
        let mut db_options = Options::default();
        db_options.create_if_missing(true);
        db_options.create_missing_column_families(true);
        let db = DB::open_cf_descriptors(
            &db_options,
            temp_dir.path(),
            [
                ColumnFamilyDescriptor::new(
                    ACCOUNT_META_CF,
                    RocksAccountsDb::account_meta_options(),
                ),
                ColumnFamilyDescriptor::new(
                    ACCOUNT_DATA_CF,
                    RocksAccountsDb::account_data_options(),
                ),
                ColumnFamilyDescriptor::new("db_meta", Options::default()),
            ],
        )
        .unwrap();
        drop(db);

        let db = RocksAccountsDb::open(temp_dir.path()).unwrap();
        drop(db);

        let column_families = DB::list_cf(&Options::default(), temp_dir.path()).unwrap();
        assert!(!column_families.iter().any(|cf| cf == "db_meta"));
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
        assert_eq!(stats.accounts_written, 2);
        assert_eq!(stats.accounts_skipped, 0);
        let (loaded, slot) = db.load_account_with_slot(&pubkey).unwrap().unwrap();
        assert_eq!(slot, 10);
        assert_eq!(loaded, newer);

        let stats = db
            .store_startup_accounts_with_slots([(&pubkey, 7, &older)])
            .unwrap();
        assert_eq!(stats.accounts_written, 1);
        assert_eq!(stats.accounts_skipped, 0);
        let (loaded, slot) = db.load_account_with_slot(&pubkey).unwrap().unwrap();
        assert_eq!(slot, 10);
        assert_eq!(loaded, newer);
    }

    #[test]
    fn test_startup_store_allows_same_slot_duplicate() {
        let temp_dir = TempDir::new().unwrap();
        let db = RocksAccountsDb::open(temp_dir.path()).unwrap();
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let account = AccountSharedData::new(5, 4, &owner);

        db.store_startup_accounts_with_slots([(&pubkey, 10, &account), (&pubkey, 10, &account)])
            .unwrap();

        let (loaded, slot) = db.load_account_with_slot(&pubkey).unwrap().unwrap();
        assert_eq!(slot, 10);
        assert_eq!(loaded, account);
    }

    #[test]
    fn test_startup_store_newer_zero_lamport_removes_older_data() {
        let temp_dir = TempDir::new().unwrap();
        let db = RocksAccountsDb::open(temp_dir.path()).unwrap();
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let older = AccountSharedData::new(5, 4, &owner);
        let newer_zero_lamport = AccountSharedData::new(0, 0, &owner);

        db.store_startup_accounts_with_slots([
            (&pubkey, 10, &older),
            (&pubkey, 12, &newer_zero_lamport),
            (&pubkey, 11, &older),
        ])
        .unwrap();

        let (loaded, slot) = db.load_account_with_slot(&pubkey).unwrap().unwrap();
        assert_eq!(slot, 12);
        assert_eq!(loaded.lamports(), 0);
        assert!(loaded.data().is_empty());
    }
}
