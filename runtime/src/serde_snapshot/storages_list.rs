use {
    solana_accounts_db::{
        account_storage_entry::AccountStorageEntry, accounts_db::AccountsFileId,
        accounts_file::AccountsFile,
    },
    solana_clock::Slot,
    std::{collections::HashMap, io, path::PathBuf, sync::Arc},
    wincode::{SchemaRead, SchemaWrite},
};

/// Identifies a storage belonging to a bank snapshot and the files required to restore it.
///
/// The file is local-only (never archived) and gated behind `SNAPSHOT_FASTBOOT_VERSION`, so the
/// on-disk encoding can use `AccountsFileId` (`u32`) directly without worrying about
/// forward-compat with potential future widening.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub enum StorageListItem {
    AppendVec {
        id: AccountsFileId,
        slot: Slot,
    },
    Split {
        has_data_file: bool,
        id: AccountsFileId,
        slot: Slot,
    },
}

impl StorageListItem {
    pub fn slot_and_id(&self) -> (Slot, AccountsFileId) {
        match *self {
            Self::AppendVec { slot, id } | Self::Split { slot, id, .. } => (slot, id),
        }
    }

    /// Checks the file suffix; callers must separately match the slot and ID.
    pub fn matches_filename(&self, filename: &str) -> bool {
        match self {
            Self::AppendVec { .. } => !filename.ends_with(".meta") && !filename.ends_with(".data"),
            Self::Split { has_data_file, .. } => {
                filename.ends_with(".meta") || (*has_data_file && filename.ends_with(".data"))
            }
        }
    }
}

/// On-disk format of the storages list file.
///
/// Written next to the bank snapshot on graceful exit to record which storages in the account
/// run dirs make up the snapshot; read on startup to prune anything else before fastboot.
#[derive(Debug, SchemaRead, SchemaWrite)]
pub struct StoragesList {
    list: Vec<StorageListItem>,
}

impl StoragesList {
    pub fn new_from_storages(snapshot_storages: &[Arc<AccountStorageEntry>]) -> Self {
        Self::from_items(
            snapshot_storages
                .iter()
                .map(|storage| {
                    let slot = storage.slot();
                    let id = storage.id();
                    match &storage.accounts {
                        AccountsFile::AppendVec(_) => StorageListItem::AppendVec { slot, id },
                        AccountsFile::Split(split) => StorageListItem::Split {
                            slot,
                            id,
                            has_data_file: split.data_file().is_some(),
                        },
                    }
                })
                .collect(),
        )
    }

    /// Build a `StoragesList` from an existing `Vec` of items.
    pub fn from_items(list: Vec<StorageListItem>) -> Self {
        Self { list }
    }

    pub fn into_map(self) -> HashMap<(Slot, AccountsFileId), StorageListItem> {
        self.list
            .into_iter()
            .map(|item| (item.slot_and_id(), item))
            .collect()
    }
}

/// Slot/id-only format used before fastboot v5.
#[repr(C)]
#[derive(Debug, SchemaRead, SchemaWrite)]
pub struct LegacyStorageListItem {
    pub slot: Slot,
    pub id: AccountsFileId,
}

#[derive(Debug, SchemaRead, SchemaWrite)]
pub struct LegacyStoragesList {
    pub list: Vec<LegacyStorageListItem>,
}

impl LegacyStoragesList {
    /// Fastboot files already exist, so infer their format without rewriting the manifest.
    pub fn into_current(self, account_paths: &[PathBuf]) -> io::Result<StoragesList> {
        let mut list = Vec::with_capacity(self.list.len());
        for LegacyStorageListItem { slot, id } in self.list {
            let name = AccountsFile::file_name(slot, id);
            let mut item = None;
            for account_path in account_paths {
                let path = account_path.join(&name);
                if path.try_exists()? {
                    item = Some(StorageListItem::AppendVec { slot, id });
                    break;
                }
                if path.with_added_extension("meta").try_exists()? {
                    item = Some(StorageListItem::Split {
                        slot,
                        id,
                        has_data_file: path.with_added_extension("data").try_exists()?,
                    });
                    break;
                }
            }
            list.push(item.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing storage '{name}' from legacy storages list"),
                )
            })?);
        }
        Ok(StoragesList::from_items(list))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::serde_snapshot::{deserialize_wincode_from, serialize_into},
        std::io::Cursor,
    };

    #[test]
    fn test_roundtrip_storages_list() {
        let list = StoragesList {
            list: vec![
                StorageListItem::AppendVec { slot: 7, id: 42 },
                StorageListItem::AppendVec {
                    slot: 11,
                    id: 13,
                },
                StorageListItem::AppendVec {
                    slot: Slot::MAX,
                    id: AccountsFileId::MAX,
                },
            ],
        };
        let expected = list.list.clone();

        let mut buf = Vec::new();
        serialize_into(Cursor::new(&mut buf), &list).unwrap();

        let decoded: StoragesList = deserialize_wincode_from(Cursor::new(&buf)).unwrap();
        assert_eq!(decoded.list, expected);
    }
}
