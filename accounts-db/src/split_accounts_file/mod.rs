mod data;
mod meta;
mod utils;

use {
    self::{
        data::{
            DATA_HEADER_SIZE, calculate_data_stored_size, read_account_data, write_data_entry,
            write_data_header, write_data_header_used_len,
        },
        meta::{
            DataLen, DataRef, ExternalDataOffset, META_ALIGNMENT, META_HEADER_SIZE,
            calculate_meta_stored_size, read_account_meta, should_store_internal, write_meta_entry,
            write_meta_header, write_meta_header_used_len,
        },
        utils::{
            align_usize, create_split_file, data_path_for_base, meta_path_for_base,
            parse_slot_and_id, usize_to_file_size,
        },
    },
    crate::{
        account_info::Offset,
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        accounts_file::StoredAccountsInfo,
        storable_accounts::StorableAccounts,
        utils::create_account_shared_data,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_pubkey::Pubkey,
    std::{
        convert::TryFrom,
        fs::{File, remove_file},
        io,
        path::{Path, PathBuf},
        sync::{
            Mutex, OnceLock,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    },
};

/// Account storage backed by split metadata and data files.
///
/// Account index offsets point into the meta file. Small account data is stored
/// inline in the meta entry; larger data is stored in the sibling data file.
#[derive(Debug)]
pub struct SplitAccountsFile {
    slot: u64,
    id: u32,
    meta_path: PathBuf,
    data_path: PathBuf,
    meta_file: File,
    data_file: OnceLock<File>,
    write_lock: Mutex<()>,
    meta_len: AtomicUsize,
    data_len: AtomicUsize,
    logical_len: AtomicUsize,
    remove_file_on_drop: AtomicBool,
}

impl Drop for SplitAccountsFile {
    fn drop(&mut self) {
        if self.remove_file_on_drop.load(Ordering::Acquire) {
            if let Err(err) = remove_file(&self.meta_path) {
                log::warn!(
                    "failed to remove {} on drop: {err}",
                    self.meta_path.display(),
                );
            }
            if let Err(err) = remove_file(&self.data_path)
                && err.kind() != io::ErrorKind::NotFound
            {
                log::warn!(
                    "failed to remove {} on drop: {err}",
                    self.data_path.display(),
                );
            }
        }
    }
}

impl SplitAccountsFile {
    // brooks TODO: add a fn to open() existing split file

    // brooks TODO: doc new file for writing
    pub fn new(base_path: impl Into<PathBuf>) -> io::Result<Self> {
        let base_path = base_path.into();
        let (slot, id) = parse_slot_and_id(&base_path).unwrap_or_default();
        let meta_path = meta_path_for_base(&base_path);
        let data_path = data_path_for_base(&base_path);

        let _ = remove_file(&meta_path);
        let _ = remove_file(&data_path);

        let mut meta_file = create_split_file(&meta_path)?;
        write_meta_header(&mut meta_file, slot, id, META_HEADER_SIZE)?;

        Ok(Self {
            slot,
            id,
            meta_path,
            data_path,
            meta_file,
            data_file: OnceLock::new(),
            write_lock: Mutex::new(()),
            meta_len: AtomicUsize::new(META_HEADER_SIZE),
            data_len: AtomicUsize::new(DATA_HEADER_SIZE),
            logical_len: AtomicUsize::new(0),
            remove_file_on_drop: AtomicBool::new(true),
        })
    }

    pub fn write_accounts<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
    ) -> io::Result<StoredAccountsInfo> {
        let _write_guard = self.write_lock.lock().unwrap();
        let mut meta_offset = self.meta_len.load(Ordering::Acquire);
        let mut data_offset = self.data_len.load(Ordering::Acquire);
        let mut offsets = Vec::with_capacity(accounts.len());
        let mut stored_size = 0usize;
        let mut wrote_external_data = false;

        for i in 0..accounts.len() {
            accounts.account_default_if_zero_lamport(i, |account| -> io::Result<()> {
                let data_len = account.data().len();
                let aligned_meta_offset = align_usize(meta_offset, META_ALIGNMENT);
                let meta_stored_size = calculate_meta_stored_size(data_len);
                let data_ref = if data_len == 0 {
                    DataRef::NoData
                } else if should_store_internal(data_len) {
                    DataRef::Inline {
                        len: DataLen(u64::try_from(data_len).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "split data length too large",
                            )
                        })?),
                    }
                } else {
                    if self.data_file.get().is_none() {
                        let mut file = create_split_file(&self.data_path)?;
                        write_data_header(&mut file, self.slot, self.id, DATA_HEADER_SIZE)?;
                        self.data_file.set(file).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                "split data file already initialized",
                            )
                        })?;
                    }
                    let aligned_data_offset = align_usize(data_offset, data::DATA_ALIGNMENT);
                    let data_stored_size = calculate_data_stored_size(data_len);
                    write_data_entry(
                        self.data_file.get().expect("data file exists"),
                        aligned_data_offset,
                        account.pubkey(),
                        account.data(),
                    )?;
                    data_offset = aligned_data_offset.saturating_add(data_stored_size);
                    stored_size = stored_size.saturating_add(data_stored_size);
                    wrote_external_data = true;
                    DataRef::External {
                        len: DataLen(u64::try_from(data_len).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "split data length too large",
                            )
                        })?),
                        offset: ExternalDataOffset(u64::try_from(aligned_data_offset).map_err(
                            |_| {
                                io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "split data offset too large",
                                )
                            },
                        )?),
                    }
                };

                write_meta_entry(&self.meta_file, aligned_meta_offset, &account, data_ref)?;
                meta_offset = aligned_meta_offset.saturating_add(meta_stored_size);
                offsets.push(aligned_meta_offset);
                stored_size = stored_size.saturating_add(meta_stored_size);
                Ok(())
            })?;
        }

        self.meta_file.set_len(usize_to_file_size(meta_offset)?)?;
        write_meta_header_used_len(&self.meta_file, meta_offset)?;
        self.meta_file.sync_all()?;

        if wrote_external_data {
            let file = self.data_file.get().expect("data file exists");
            file.set_len(usize_to_file_size(data_offset)?)?;
            write_data_header_used_len(file, data_offset)?;
            file.sync_all()?;
        }

        self.data_len.store(data_offset, Ordering::Release);
        self.logical_len.fetch_add(stored_size, Ordering::Release);
        self.meta_len.store(meta_offset, Ordering::Release);

        Ok(StoredAccountsInfo {
            offsets,
            size: stored_size,
        })
    }

    pub fn reopen_as_readonly_file_io(&self) -> Option<Self> {
        None
    }

    pub fn disable_remove_on_drop(&self) {
        self.remove_file_on_drop.store(false, Ordering::Release);
    }

    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.logical_len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn dead_bytes_due_to_zero_lamport_single_ref(&self, count: usize) -> usize {
        Self::calculate_stored_size(0) * count
    }

    pub fn path(&self) -> &Path {
        &self.meta_path
    }

    pub fn open_file_for_archive(&self) -> &File {
        &self.meta_file
    }

    pub fn get_account_without_data<Ret>(
        &self,
        offset: usize,
        mut callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>) -> Ret,
    ) -> Option<Ret> {
        let meta_len = self.meta_len.load(Ordering::Acquire);
        let account = read_account_meta(&self.meta_file, meta_len, offset)?;
        Some(callback(StoredAccountInfoWithoutData {
            pubkey: &account.pubkey,
            lamports: account.lamports,
            owner: &account.owner,
            data_len: account.data_len,
            executable: account.executable,
            rent_epoch: account.rent_epoch,
        }))
    }

    pub fn get_account<Ret>(
        &self,
        offset: usize,
        mut callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> Option<Ret> {
        let meta_len = self.meta_len.load(Ordering::Acquire);
        let data_len = self.data_len.load(Ordering::Acquire);
        let account = read_account_meta(&self.meta_file, meta_len, offset)?;
        let data = read_account_data(
            &self.meta_file,
            meta_len,
            self.data_file.get(),
            data_len,
            &account,
        )
        .ok()?;
        Some(callback(StoredAccountInfo {
            pubkey: &account.pubkey,
            lamports: account.lamports,
            owner: &account.owner,
            data: &data,
            executable: account.executable,
            rent_epoch: account.rent_epoch,
        }))
    }

    pub fn get_account_shared_data(&self, offset: usize) -> Option<AccountSharedData> {
        self.get_account(offset, |account| create_account_shared_data(&account))
    }

    pub fn scan_pubkeys(&self, mut callback: impl FnMut(&Pubkey)) -> io::Result<()> {
        self.scan_accounts_without_data(|_offset, account| callback(account.pubkey()))
    }

    pub fn scan_accounts_without_data(
        &self,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfoWithoutData<'local>),
    ) -> io::Result<()> {
        let meta_len = self.meta_len.load(Ordering::Acquire);
        let mut offset = META_HEADER_SIZE;
        while offset < meta_len {
            let Some(account) = read_account_meta(&self.meta_file, meta_len, offset) else {
                break;
            };
            let next_offset = offset.saturating_add(account.stored_size);
            callback(
                offset,
                StoredAccountInfoWithoutData {
                    pubkey: &account.pubkey,
                    lamports: account.lamports,
                    owner: &account.owner,
                    data_len: account.data_len,
                    executable: account.executable,
                    rent_epoch: account.rent_epoch,
                },
            );
            offset = next_offset;
        }
        Ok(())
    }

    pub fn scan_accounts<'a>(
        &'a self,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> io::Result<()> {
        let meta_len = self.meta_len.load(Ordering::Acquire);
        let data_len = self.data_len.load(Ordering::Acquire);
        let mut offset = META_HEADER_SIZE;
        while offset < meta_len {
            let Some(account) = read_account_meta(&self.meta_file, meta_len, offset) else {
                break;
            };
            let next_offset = offset.saturating_add(account.stored_size);
            let data = read_account_data(
                &self.meta_file,
                meta_len,
                self.data_file.get(),
                data_len,
                &account,
            )?;
            callback(
                offset,
                StoredAccountInfo {
                    pubkey: &account.pubkey,
                    lamports: account.lamports,
                    owner: &account.owner,
                    data: &data,
                    executable: account.executable,
                    rent_epoch: account.rent_epoch,
                },
            );
            offset = next_offset;
        }
        Ok(())
    }

    pub fn calculate_stored_size(data_len: usize) -> usize {
        let meta_size = calculate_meta_stored_size(data_len);
        if should_store_internal(data_len) {
            meta_size
        } else {
            meta_size.saturating_add(calculate_data_stored_size(data_len))
        }
    }

    pub fn get_account_data_lens(&self, sorted_offsets: &[usize]) -> Vec<usize> {
        let meta_len = self.meta_len.load(Ordering::Acquire);
        let mut data_lens = Vec::with_capacity(sorted_offsets.len());
        for &offset in sorted_offsets {
            let Some(account) = read_account_meta(&self.meta_file, meta_len, offset) else {
                break;
            };
            data_lens.push(account.data_len);
        }
        data_lens
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        super::{
            data::{DATA_ALIGNMENT, DATA_ENTRY_FIXED_SIZE, DATA_LEN_OFFSET},
            meta::{
                DATA_REF_EXTERNAL, DATA_REF_INTERNAL, MAX_META_ENTRY_SIZE, META_DATA_LEN_OFFSET,
                META_DATA_OFFSET_OFFSET, META_DATA_REF_KIND_OFFSET, META_ENTRY_FIXED_SIZE,
            },
            utils::{read_array, read_exact_at},
        },
        solana_account::accounts_equal,
        tempfile::TempDir,
    };

    fn new_account(lamports: u64, data_len: usize) -> (Pubkey, AccountSharedData) {
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let mut account = AccountSharedData::new(lamports, data_len, &owner);
        account.set_data((0..data_len).map(|i| (i % 251) as u8).collect());
        (pubkey, account)
    }

    #[test]
    fn test_split_accounts_file_roundtrip_internal_and_external() {
        let temp_dir = TempDir::new().unwrap();
        let small = new_account(11, 16);
        let large = new_account(22, MAX_META_ENTRY_SIZE);
        let accounts = [(&small.0, &small.1), (&large.0, &large.1)];

        let split = SplitAccountsFile::new(temp_dir.path().join("42.7")).unwrap();
        let stored = split.write_accounts(&(42, &accounts[..])).unwrap();
        assert_eq!(stored.offsets.len(), 2);
        assert_eq!(stored.offsets[0], META_HEADER_SIZE);
        assert_eq!(stored.offsets[0] % META_ALIGNMENT, 0);
        assert_eq!(stored.offsets[1] % META_ALIGNMENT, 0);
        assert!(split.data_file.get().is_some());

        let meta_len = split.meta_len.load(Ordering::Acquire);

        let mut small_meta = [0; META_ENTRY_FIXED_SIZE];
        read_exact_at(
            &split.meta_file,
            meta_len,
            stored.offsets[0],
            &mut small_meta,
        )
        .unwrap();
        assert_eq!(small_meta[META_DATA_REF_KIND_OFFSET], DATA_REF_INTERNAL);
        assert_eq!(
            u64::from_le_bytes(read_array(&small_meta, META_DATA_LEN_OFFSET).unwrap()),
            u64::try_from(small.1.data().len()).unwrap(),
        );

        let mut large_meta = [0; META_ENTRY_FIXED_SIZE];
        read_exact_at(
            &split.meta_file,
            meta_len,
            stored.offsets[1],
            &mut large_meta,
        )
        .unwrap();
        assert_eq!(large_meta[META_DATA_REF_KIND_OFFSET], DATA_REF_EXTERNAL);
        assert_eq!(
            u64::from_le_bytes(read_array(&large_meta, META_DATA_LEN_OFFSET).unwrap()),
            u64::try_from(large.1.data().len()).unwrap(),
        );
        let external_offset =
            u64::from_le_bytes(read_array(&large_meta, META_DATA_OFFSET_OFFSET).unwrap());
        assert_eq!(external_offset % u64::try_from(DATA_ALIGNMENT).unwrap(), 0);

        let data_len = split.data_len.load(Ordering::Acquire);
        let mut large_data = [0; DATA_ENTRY_FIXED_SIZE];
        read_exact_at(
            split.data_file.get().unwrap(),
            data_len,
            usize::try_from(external_offset).unwrap(),
            &mut large_data,
        )
        .unwrap();
        assert_eq!(
            u64::from_le_bytes(read_array(&large_data, DATA_LEN_OFFSET).unwrap()),
            u64::try_from(large.1.data().len()).unwrap(),
        );
        for (offset, expected) in stored.offsets.iter().zip([&small, &large]) {
            split
                .get_account_without_data(*offset, |account| {
                    assert_eq!(account.pubkey(), &expected.0);
                    assert_eq!(account.lamports, expected.1.lamports());
                    assert_eq!(account.owner, expected.1.owner());
                    assert_eq!(account.data_len, expected.1.data().len());
                    assert_eq!(account.rent_epoch, expected.1.rent_epoch());
                })
                .unwrap();

            let loaded = split.get_account_shared_data(*offset).unwrap();
            assert!(accounts_equal(&loaded, &expected.1));
        }
    }

    #[test]
    fn test_split_accounts_file_multiple_writes() {
        let temp_dir = TempDir::new().unwrap();
        let first = new_account(11, MAX_META_ENTRY_SIZE);
        let second = new_account(22, MAX_META_ENTRY_SIZE + 1);
        let split = SplitAccountsFile::new(temp_dir.path().join("42.7")).unwrap();

        let first_stored = split
            .write_accounts(&(42, &[(&first.0, &first.1)][..]))
            .unwrap();
        let len_after_first_write = split.len();
        let second_stored = split
            .write_accounts(&(42, &[(&second.0, &second.1)][..]))
            .unwrap();

        assert_eq!(first_stored.offsets.len(), 1);
        assert_eq!(second_stored.offsets.len(), 1);
        assert!(second_stored.offsets[0] > first_stored.offsets[0]);
        assert_eq!(split.len(), len_after_first_write + second_stored.size,);
        assert!(accounts_equal(
            &split
                .get_account_shared_data(first_stored.offsets[0])
                .unwrap(),
            &first.1,
        ));
        assert!(accounts_equal(
            &split
                .get_account_shared_data(second_stored.offsets[0])
                .unwrap(),
            &second.1,
        ));
    }

    #[test]
    fn test_read_external_data_does_not_take_write_lock() {
        let temp_dir = TempDir::new().unwrap();
        let expected = new_account(11, MAX_META_ENTRY_SIZE);
        let split =
            std::sync::Arc::new(SplitAccountsFile::new(temp_dir.path().join("42.7")).unwrap());
        let stored = split
            .write_accounts(&(42, &[(&expected.0, &expected.1)][..]))
            .unwrap();

        let write_guard = split.write_lock.lock().unwrap();
        let split_for_read = std::sync::Arc::clone(&split);
        let offset = stored.offsets[0];
        let (sender, receiver) = std::sync::mpsc::channel();
        let read_thread = std::thread::spawn(move || {
            sender
                .send(split_for_read.get_account_shared_data(offset))
                .unwrap();
        });

        let loaded = receiver.recv_timeout(std::time::Duration::from_secs(10));
        drop(write_guard);
        read_thread.join().unwrap();
        assert!(accounts_equal(
            &loaded
                .expect("external data read must not wait for the write lock")
                .unwrap(),
            &expected.1,
        ));
    }
}
