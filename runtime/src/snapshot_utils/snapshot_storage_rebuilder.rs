//! Provides interfaces for rebuilding snapshot storages

use {
    super::{SnapshotError, SnapshotFrom},
    crate::serde_snapshot::{
        SerdeObsoleteAccounts, SerdeObsoleteAccountsMap, reconstruct_single_storage, reconstruct_storage,
        remap_and_reconstruct_single_storage,
    },
    agave_fs::FileInfo,
    log::*,
    solana_accounts_db::{
        account_storage::AccountStorageMap,
        accounts_db::{AccountsFileId, AtomicAccountsFileId},
        accounts_file::AccountsFile,
    },
    solana_clock::Slot,
    std::{
        collections::HashMap,
        path::PathBuf,
        str::FromStr as _,
        sync::{Arc, atomic::Ordering},
        time::{Duration, Instant},
    },
};

const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// Stores state for rebuilding snapshot storages
#[derive(Debug)]
pub(crate) struct SnapshotStorageRebuilder {
    /// Container for storing rebuilt snapshot storages
    storage: AccountStorageMap,
    /// Tracks next append_vec_id
    next_append_vec_id: Arc<AtomicAccountsFileId>,
    /// Tracks the number of collisions in AccountsFileId
    num_collisions: usize,
    /// Number of storage slots that have been rebuilt so far
    processed_slot_count: usize,
    /// Rebuild from the snapshot files or archives
    snapshot_from: SnapshotFrom,
    /// obsolete accounts for all storages
    obsolete_accounts: HashMap<Slot, SerdeObsoleteAccounts>,
    /// Split files waiting for their matching metadata or data file.
    split_files: HashMap<PathBuf, FileInfo>,
}

impl SnapshotStorageRebuilder {
    /// Rebuild snapshot storages on the current thread by consuming `files`.
    pub(crate) fn rebuild_storages(
        files: impl IntoIterator<Item = FileInfo>,
        next_append_vec_id: Arc<AtomicAccountsFileId>,
        snapshot_from: SnapshotFrom,
        obsolete_accounts: Option<SerdeObsoleteAccountsMap>,
    ) -> Result<AccountStorageMap, SnapshotError> {
        let mut rebuilder = Self {
            storage: AccountStorageMap::default(),
            next_append_vec_id,
            num_collisions: 0,
            processed_slot_count: 0,
            snapshot_from,
            obsolete_accounts: obsolete_accounts
                .map(|map| map.into_hashmap())
                .unwrap_or_default(),
            split_files: HashMap::new(),
        };

        let mut last_log_time = Instant::now();

        for file_info in files {
            rebuilder.process_append_vec_file(file_info)?;
            let now = Instant::now();
            if now.duration_since(last_log_time) >= PROGRESS_LOG_INTERVAL {
                rebuilder.log_progress();
                last_log_time = now;
            }
        }

        if let Some(path) = rebuilder.split_files.keys().next() {
            return Err(SnapshotError::RebuildStorages(format!(
                "missing matching split storage file for '{}'", path.display(),
            )));
        }
        Ok(rebuilder.storage)
    }

    fn log_progress(&self) {
        info!(
            "rebuilt storages for {} slots with {} collisions",
            self.processed_slot_count, self.num_collisions,
        );
    }

    fn process_append_vec_file(&mut self, file_info: FileInfo) -> Result<(), SnapshotError> {
        let filename = file_info.path.file_name().unwrap().to_str().unwrap();
        if let Ok((slot, append_vec_id)) = get_slot_and_append_vec_id(filename) {
            if filename.ends_with(".meta") || filename.ends_with(".data") {
                if self.snapshot_from == SnapshotFrom::Archive {
                    return Err(SnapshotError::RebuildStorages(
                        "snapshot archives must contain AppendVec storages".to_owned(),
                    ));
                }
                let base_path = file_info.path.with_extension("");
                if let Some(other) = self.split_files.remove(&base_path) {
                    let (meta, data) = if filename.ends_with(".meta") {
                        (file_info, other)
                    } else {
                        (other, file_info)
                    };
                    let storage_entry = reconstruct_storage(
                        &slot,
                        append_vec_id as AccountsFileId,
                        self.obsolete_accounts.remove(&slot).map(|accounts| accounts.into_tuple()),
                        || AccountsFile::new_split_for_startup(meta, data),
                    )?;
                    self.insert_storage(slot, storage_entry)?;
                    self.next_append_vec_id
                        .fetch_max((append_vec_id + 1) as AccountsFileId, Ordering::Relaxed);
                    self.processed_slot_count += 1;
                } else {
                    self.split_files.insert(base_path, file_info);
                }
                return Ok(());
            }
            if self.snapshot_from == SnapshotFrom::Dir {
                // Keep track of the highest append_vec_id in the system, so the future append_vecs
                // can be assigned to unique IDs.  This is only needed when loading from a snapshot
                // dir.  When loading from a snapshot archive, the max of the appendvec IDs is
                // updated in remap_append_vec_file(), which is not in the from_dir route.
                self.next_append_vec_id
                    .fetch_max((append_vec_id + 1) as AccountsFileId, Ordering::Relaxed);
            }
            self.process_complete_slot(slot, file_info)?;
            self.processed_slot_count += 1;
        }
        Ok(())
    }

    /// Process a slot that has received all storage entries
    fn process_complete_slot(
        &mut self,
        slot: Slot,
        file_info: FileInfo,
    ) -> Result<(), SnapshotError> {
        let filename = file_info.path.file_name().unwrap().to_str().unwrap();
        let (_, old_append_vec_id) = get_slot_and_append_vec_id(filename)?;

        let storage_entry = match &self.snapshot_from {
            SnapshotFrom::Archive => remap_and_reconstruct_single_storage(
                slot,
                old_append_vec_id,
                file_info,
                &self.next_append_vec_id,
                &mut self.num_collisions,
            )?,
            SnapshotFrom::Dir => reconstruct_single_storage(
                &slot,
                file_info,
                old_append_vec_id as AccountsFileId,
                self.obsolete_accounts
                    .remove(&slot)
                    .map(|accounts| accounts.into_tuple()),
            )?,
        };

        self.insert_storage(slot, storage_entry)
    }

    fn insert_storage(
        &mut self,
        slot: Slot,
        storage_entry: Arc<solana_accounts_db::account_storage_entry::AccountStorageEntry>,
    ) -> Result<(), SnapshotError> {
        let storage_id = storage_entry.id();
        if let Some(other) = self.storage.insert(slot, storage_entry) {
            Err(SnapshotError::RebuildStorages(format!(
                "there must be exactly one storage per slot, but slot {slot} has duplicate \
                 storages: {} vs {storage_id}",
                other.id()
            )))
        } else {
            Ok(())
        }
    }
}

/// Get the slot and storage id from an AppendVec or split storage filename.
pub(crate) fn get_slot_and_append_vec_id(filename: &str) -> Result<(Slot, usize), SnapshotError> {
    let storage_name = filename.strip_suffix(".meta")
        .or_else(|| filename.strip_suffix(".data"))
        .unwrap_or(filename);
    let mut parts = storage_name.splitn(2, '.');
    let slot = parts.next().and_then(|s| Slot::from_str(s).ok());
    let id = parts.next().and_then(|s| usize::from_str(s).ok());

    slot.zip(id)
        .ok_or_else(|| SnapshotError::InvalidAppendVecPath(PathBuf::from(filename)))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::AccountSharedData,
        solana_accounts_db::{
            ObsoleteAccountItem, ObsoleteAccounts,
            account_storage_entry::AccountStorageEntry,
            accounts_file::{AccountsFile, AccountsFileProvider},
        },
        solana_pubkey::Pubkey,
        test_case::test_case,
    };

    #[test]
    fn test_get_slot_and_append_vec_id() {
        let expected_slot = 12345;
        let expected_id = 9987;
        let (slot, id) =
            get_slot_and_append_vec_id(&AccountsFile::file_name(expected_slot, expected_id))
                .unwrap();
        assert_eq!(expected_slot, slot);
        assert_eq!(expected_id as usize, id);
        for suffix in ["meta", "data"] {
            assert_eq!(
                get_slot_and_append_vec_id(&format!("{expected_slot}.{expected_id}.{suffix}")).unwrap(),
                (expected_slot, expected_id as usize),
            );
        }
        assert!(get_slot_and_append_vec_id("12345.9987.other").is_err());
    }

    #[test_case(false)]
    #[test_case(true)]
    fn test_rebuild_mixed_storages(data_first: bool) {
        let dir = tempfile::tempdir().unwrap();
        let slot = 123;
        let id = 456;
        let split = AccountsFileProvider::Split.new_writable(
            dir.path().join(AccountsFile::file_name(slot, id)), 0,
        ).unwrap();
        let owner = Pubkey::new_unique();
        let accounts = vec![
            (Pubkey::new_unique(), AccountSharedData::new(1, 165, &owner)),
            (Pubkey::new_unique(), AccountSharedData::new(2, 8192, &owner)),
        ];
        let offsets = split.write_accounts(&(slot, accounts.as_slice())).unwrap().offsets;
        split.disable_remove_on_drop();
        split.flush().unwrap();
        let obsolete = ObsoleteAccounts {
            accounts: vec![ObsoleteAccountItem {
                offset: offsets[1],
                data_len: 8192,
                slot: slot + 1,
            }],
        };
        let append_vec = Arc::new(AccountStorageEntry::new(
            dir.path(), slot + 1, id + 1, 16384, AccountsFileProvider::AppendVec,
        ));
        append_vec.accounts.write_accounts(&(slot + 1, accounts.as_slice())).unwrap();
        append_vec.flush().unwrap();
        let av = FileInfo::new_from_path(append_vec.path()).unwrap();
        let split = Arc::new(AccountStorageEntry::new_existing(
            slot, id, split, obsolete,
        ));
        let obsolete_accounts = SerdeObsoleteAccountsMap::new_from_storages(
            &[split.clone(), append_vec.clone()], slot + 1,
        );
        let meta = FileInfo::new_from_path(split.path()).unwrap();
        let data = FileInfo::new_from_path(dir.path().join(format!("{slot}.{id}.data"))).unwrap();
        split.disable_remove_on_drop();
        append_vec.disable_remove_on_drop();
        drop(split);
        drop(append_vec);
        let files = if data_first { vec![data, av, meta] } else { vec![meta, av, data] };
        let next_id = Arc::new(AtomicAccountsFileId::new(0));
        let storages = SnapshotStorageRebuilder::rebuild_storages(
            files, next_id.clone(), SnapshotFrom::Dir, Some(obsolete_accounts),
        ).unwrap();
        assert_eq!(storages.len(), 2);
        assert_eq!(next_id.load(Ordering::Relaxed), id + 2);
        let storage = storages.get(&slot).unwrap();
        assert!(matches!(storage.accounts, AccountsFile::Split(_)));
        for (offset, (_, account)) in offsets.iter().zip(&accounts) {
            let loaded = storage.accounts.get_stored_account_callback(*offset, |stored| {
                solana_accounts_db::utils::create_account_shared_data(&stored)
            }).unwrap();
            assert_eq!(&loaded, account);
        }
        let obsolete = storage.obsolete_accounts_for_snapshots(slot + 1);
        assert_eq!(obsolete.accounts.len(), 1);
        assert_eq!(obsolete.accounts[0].offset, offsets[1]);
        assert_eq!(obsolete.accounts[0].data_len, 8192);
        assert!(matches!(storages.get(&(slot + 1)).unwrap().accounts, AccountsFile::AppendVec(_)));
    }

    #[test_case("meta")]
    #[test_case("data")]
    fn test_rebuild_split_missing_file(suffix: &str) {
        let dir = tempfile::tempdir().unwrap();
        let split = AccountsFileProvider::Split.new_writable(dir.path().join("123.456"), 0).unwrap();
        split.flush().unwrap();
        let file = FileInfo::new_from_path(dir.path().join(format!("123.456.{suffix}"))).unwrap();
        let result = SnapshotStorageRebuilder::rebuild_storages(
            [file], Arc::new(AtomicAccountsFileId::new(0)), SnapshotFrom::Dir, None,
        );
        assert!(matches!(result, Err(SnapshotError::RebuildStorages(message))
            if message.contains("missing matching split storage file")));
    }

    #[test]
    fn test_reconstruct_storage_checks_obsolete_id_before_opening() {
        let result = reconstruct_storage(
            &123, 456, Some((ObsoleteAccounts::default(), 457, 0)),
            || panic!("must validate the obsolete storage id before opening files"),
        );
        assert!(matches!(result, Err(SnapshotError::MismatchedAccountsFileId(456, 457))));
    }
}
