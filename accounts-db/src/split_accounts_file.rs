use {
    crate::{
        account_info::Offset,
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        accounts_file::StoredAccountsInfo,
        storable_accounts::StorableAccounts,
        u64_align,
        utils::create_account_shared_data,
    },
    agave_fs::{
        FileInfo, FileSize,
        buffered_reader::RequiredLenBufFileRead,
        file_io::{read_into_buffer, write_buffer_to_file},
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    std::{
        convert::TryFrom,
        collections::HashSet,
        fs::{File, OpenOptions, remove_file},
        io::{self, Seek, SeekFrom, Write},
        mem::MaybeUninit,
        path::{Path, PathBuf},
        slice,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    },
};

const HEADER_SIZE: usize = 4 * 1024;
const FORMAT_VERSION: u16 = 1;
const META_MAGIC: &[u8; 8] = b"AGMETA0\0";
const DATA_MAGIC: &[u8; 8] = b"AGDATA0\0";
const META_ALIGNMENT: usize = 8;
const DATA_ALIGNMENT: usize = 4 * 1024;
const DATA_OFFSET_SHIFT: usize = 12;

const META_ADDRESS_OFFSET: usize = 0;
const META_OWNER_OFFSET: usize = 32;
const META_LAMPORTS_OFFSET: usize = 64;
const META_RENT_EPOCH_OFFSET: usize = 72;
const META_EXECUTABLE_OFFSET: usize = 80;
const META_DATA_REF_KIND_OFFSET: usize = 81;
const META_DATA_LEN_OFFSET: usize = 88;
const META_DATA_SLOT_OFFSET: usize = 96;
const META_DATA_ID_OFFSET: usize = 104;
const META_DATA_OFFSET_REDUCED_OFFSET: usize = 108;
const META_ENTRY_FIXED_SIZE: usize = 112;

const DATA_ADDRESS_OFFSET: usize = 0;
const DATA_LEN_OFFSET: usize = 32;
const DATA_ENTRY_FIXED_SIZE: usize = 40;

const DATA_REF_NONE: u8 = 0;
const DATA_REF_INTERNAL: u8 = 1;
const DATA_REF_EXTERNAL: u8 = 2;

const HEADER_MAGIC_OFFSET: usize = 0;
const HEADER_VERSION_OFFSET: usize = 8;
const HEADER_HEADER_LEN_OFFSET: usize = 10;
const HEADER_ALIGNMENT_LOG2_OFFSET: usize = 12;
const HEADER_SLOT_OFFSET: usize = 16;
const HEADER_ID_OFFSET: usize = 24;
const HEADER_USED_LEN_OFFSET: usize = 32;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct DataLocation {
    pub(crate) slot: Slot,
    pub(crate) id: u32,
    pub(crate) offset: Offset,
}

#[derive(Debug, Clone, Copy)]
enum SplitDataRef {
    None,
    Internal { len: usize },
    External { len: usize, location: DataLocation },
}

#[derive(Debug)]
struct SplitStoredAccount {
    offset: Offset,
    pubkey: Pubkey,
    owner: Pubkey,
    lamports: u64,
    rent_epoch: Epoch,
    executable: bool,
    data_ref: SplitDataRef,
    stored_size: usize,
}

/// Account storage backed by separate metadata and data files.
///
/// The account index points at offsets in the meta file. Data entries can be
/// referenced by newer metadata entries in sibling split stores. A reusable
/// reference is only created after validating that the old data entry matches
/// the new account bytes.
#[derive(Debug)]
pub struct SplitAccountsFile {
    base_path: PathBuf,
    meta_path: PathBuf,
    data_path: PathBuf,
    meta_file: File,
    data_file: File,
    append_lock: Mutex<()>,
    allow_writes: bool,
    meta_current_len: AtomicUsize,
    data_current_len: AtomicUsize,
    meta_file_size: usize,
    data_file_size: usize,
    remove_file_on_drop: AtomicBool,
    is_dirty: AtomicBool,
    slot: Slot,
    id: u32,
}

impl Drop for SplitAccountsFile {
    fn drop(&mut self) {
        if self.remove_file_on_drop.load(Ordering::Acquire) {
            if let Err(err) = remove_file(&self.meta_path) {
                log::warn!(
                    "SplitAccountsFile failed to remove {}: {err}",
                    self.meta_path.display()
                );
            }
            if let Err(err) = remove_file(&self.data_path) {
                log::warn!(
                    "SplitAccountsFile failed to remove {}: {err}",
                    self.data_path.display()
                );
            }
        }
    }
}

impl SplitAccountsFile {
    pub(crate) fn new(base_path: impl Into<PathBuf>, create: bool, payload_size: usize) -> Self {
        let base_path = base_path.into();
        let (slot, id) = parse_slot_and_id(&base_path).unwrap_or_default();
        let meta_path = meta_path_for_base(&base_path);
        let data_path = data_path_for_base(&base_path);
        let payload_size = payload_size.max(1);
        let meta_file_size = HEADER_SIZE + payload_size;
        let data_file_size = HEADER_SIZE + payload_size;

        if create {
            let _ = remove_file(&meta_path);
            let _ = remove_file(&data_path);
        }

        let mut meta_file = open_sized_file(&meta_path, create, meta_file_size)
            .unwrap_or_else(|err| panic!("Unable to open meta file {}: {err}", meta_path.display()));
        let mut data_file = open_sized_file(&data_path, create, data_file_size)
            .unwrap_or_else(|err| panic!("Unable to open data file {}: {err}", data_path.display()));

        if create {
            write_header(&mut meta_file, META_MAGIC, 3, slot, id, HEADER_SIZE)
                .expect("must write split meta header");
            write_header(&mut data_file, DATA_MAGIC, 12, slot, id, HEADER_SIZE)
                .expect("must write split data header");
        }

        Self {
            base_path,
            meta_path,
            data_path,
            meta_file,
            data_file,
            append_lock: Mutex::default(),
            allow_writes: true,
            meta_current_len: AtomicUsize::new(HEADER_SIZE),
            data_current_len: AtomicUsize::new(HEADER_SIZE),
            meta_file_size,
            data_file_size,
            remove_file_on_drop: AtomicBool::new(true),
            is_dirty: AtomicBool::new(create),
            slot,
            id,
        }
    }

    pub(crate) fn new_for_startup(meta_file_info: FileInfo) -> io::Result<Self> {
        let meta_path = meta_file_info.path;
        let base_path = base_path_for_meta_path(&meta_path);
        let data_path = data_path_for_base(&base_path);
        let data_file_info = FileInfo::new_from_path(&data_path)?;

        let (slot, id) = parse_slot_and_id(&base_path).unwrap_or_default();
        let meta_used_len = read_header(&meta_file_info.file, META_MAGIC)?;
        let data_used_len = read_header(&data_file_info.file, DATA_MAGIC)?;

        Ok(Self {
            base_path,
            meta_path,
            data_path,
            meta_file: meta_file_info.file,
            data_file: data_file_info.file,
            append_lock: Mutex::default(),
            allow_writes: false,
            meta_current_len: AtomicUsize::new(meta_used_len),
            data_current_len: AtomicUsize::new(data_used_len),
            meta_file_size: meta_file_info.size as usize,
            data_file_size: data_file_info.size as usize,
            remove_file_on_drop: AtomicBool::new(true),
            is_dirty: AtomicBool::new(false),
            slot,
            id,
        })
    }

    pub(crate) fn reopen_as_readonly_file_io(&self) -> Option<Self> {
        if !self.allow_writes {
            return None;
        }

        self.remove_file_on_drop.store(false, Ordering::Release);
        let meta_file_info = FileInfo::new_from_path(&self.meta_path).ok()?;
        let data_file_info = FileInfo::new_from_path(&self.data_path).ok()?;

        let mut new = Self {
            base_path: self.base_path.clone(),
            meta_path: self.meta_path.clone(),
            data_path: self.data_path.clone(),
            meta_file: meta_file_info.file,
            data_file: data_file_info.file,
            append_lock: Mutex::default(),
            allow_writes: false,
            meta_current_len: AtomicUsize::new(self.meta_len()),
            data_current_len: AtomicUsize::new(self.data_len()),
            meta_file_size: meta_file_info.size as usize,
            data_file_size: data_file_info.size as usize,
            remove_file_on_drop: AtomicBool::new(true),
            is_dirty: AtomicBool::new(false),
            slot: self.slot,
            id: self.id,
        };
        if self.is_dirty.swap(false, Ordering::AcqRel) {
            *new.is_dirty.get_mut() = true;
        }
        Some(new)
    }

    pub(crate) fn disable_remove_on_drop(&self) {
        self.remove_file_on_drop.store(false, Ordering::Release);
    }

    pub(crate) fn flush(&self) -> io::Result<()> {
        if self.is_dirty.swap(false, Ordering::AcqRel) {
            write_header_used_len(&self.meta_file, self.meta_len())?;
            write_header_used_len(&self.data_file, self.data_len())?;
            self.meta_file.sync_all()?;
            self.data_file.sync_all()?;
        }
        Ok(())
    }

    pub(crate) fn remaining_bytes(&self) -> u64 {
        let meta_remaining = self.meta_file_size.saturating_sub(u64_align!(self.meta_len()));
        let data_remaining = self
            .data_file_size
            .saturating_sub(align_usize(self.data_len(), DATA_ALIGNMENT));
        // `write_accounts_to_storage()` compares an AppendVec-style
        // `STORE_META_OVERHEAD + data_len` requirement against this value after
        // an append fails.  External split data entries are page-aligned, so be
        // conservative here; otherwise the caller can spin on a store that is
        // large enough for the old layout but too small for one page-aligned
        // data entry.
        meta_remaining
            .min(data_remaining.saturating_sub(DATA_ALIGNMENT))
            as u64
    }

    pub(crate) fn len(&self) -> usize {
        self.meta_len().saturating_add(self.data_len())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.meta_len() == HEADER_SIZE
    }

    pub(crate) fn capacity(&self) -> u64 {
        self.meta_file_size.saturating_add(self.data_file_size) as u64
    }

    pub(crate) fn dead_bytes_due_to_zero_lamport_single_ref(&self, count: usize) -> usize {
        Self::calculate_stored_size(0) * count
    }

    pub(crate) fn path(&self) -> &Path {
        &self.meta_path
    }

    pub(crate) fn can_reference_data_from(&self, other: &Self) -> bool {
        fn parent_or_empty(path: &Path) -> &Path {
            path.parent().unwrap_or_else(|| Path::new(""))
        }

        parent_or_empty(&self.data_path) == parent_or_empty(&other.data_path)
    }

    pub(crate) fn get_stored_account_without_data_callback<Ret>(
        &self,
        offset: usize,
        mut callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>) -> Ret,
    ) -> Option<Ret> {
        let account = self.read_meta_account(offset)?;
        Some(callback(StoredAccountInfoWithoutData {
            pubkey: &account.pubkey,
            lamports: account.lamports,
            owner: &account.owner,
            data_len: account.data_ref.len(),
            executable: account.executable,
            rent_epoch: account.rent_epoch,
        }))
    }

    pub(crate) fn get_stored_account_callback<Ret>(
        &self,
        offset: usize,
        mut callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> Option<Ret> {
        let account = self.read_meta_account(offset)?;
        let data = self.read_account_data(&account).ok()?;
        Some(callback(StoredAccountInfo {
            pubkey: &account.pubkey,
            lamports: account.lamports,
            owner: &account.owner,
            data: &data,
            executable: account.executable,
            rent_epoch: account.rent_epoch,
        }))
    }

    pub(crate) fn get_account_shared_data(&self, offset: usize) -> Option<AccountSharedData> {
        self.get_stored_account_callback(offset, |account| create_account_shared_data(&account))
    }

    pub(crate) fn scan_accounts_without_data(
        &self,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfoWithoutData<'local>),
    ) -> io::Result<()> {
        let mut offset = HEADER_SIZE;
        let meta_len = self.meta_len();
        while offset < meta_len {
            let Some(account) = self.read_meta_account(offset) else {
                break;
            };
            let next_offset = offset.saturating_add(account.stored_size);
            callback(
                offset,
                StoredAccountInfoWithoutData {
                    pubkey: &account.pubkey,
                    lamports: account.lamports,
                    owner: &account.owner,
                    data_len: account.data_ref.len(),
                    executable: account.executable,
                    rent_epoch: account.rent_epoch,
                },
            );
            offset = next_offset;
        }
        Ok(())
    }

    pub(crate) fn scan_accounts<'a>(
        &'a self,
        _reader: &mut impl RequiredLenBufFileRead<'a>,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> io::Result<()> {
        let mut offset = HEADER_SIZE;
        let meta_len = self.meta_len();
        while offset < meta_len {
            let Some(account) = self.read_meta_account(offset) else {
                break;
            };
            let next_offset = offset.saturating_add(account.stored_size);
            let data = self.read_account_data(&account)?;
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

    pub(crate) fn calculate_stored_size(data_len: usize) -> usize {
        let meta_size = Self::calculate_meta_stored_size(data_len);
        if should_store_internal(data_len) {
            meta_size
        } else {
            meta_size.saturating_add(Self::calculate_data_stored_size(data_len))
        }
    }

    pub(crate) fn get_account_data_lens(&self, sorted_offsets: &[usize]) -> Vec<usize> {
        let mut data_lens = Vec::with_capacity(sorted_offsets.len());
        for &offset in sorted_offsets {
            let Some(account) = self.read_meta_account(offset) else {
                break;
            };
            data_lens.push(account.data_ref.len());
        }
        data_lens
    }

    pub(crate) fn get_account_stored_sizes(&self, sorted_offsets: &[usize]) -> Vec<usize> {
        let mut stored_sizes = Vec::with_capacity(sorted_offsets.len());
        for &offset in sorted_offsets {
            let Some(account) = self.read_meta_account(offset) else {
                break;
            };
            stored_sizes.push(self.account_stored_size(&account));
        }
        stored_sizes
    }

    pub(crate) fn scan_pubkeys(&self, mut callback: impl FnMut(&Pubkey)) -> io::Result<()> {
        self.scan_accounts_without_data(|_offset, account| callback(account.pubkey()))
    }

    pub(crate) fn reusable_external_data_location(
        &self,
        offset: Offset,
        expected_pubkey: &Pubkey,
        expected_data: &[u8],
    ) -> Option<DataLocation> {
        let account = self.read_meta_account(offset)?;
        if &account.pubkey != expected_pubkey {
            return None;
        }

        let SplitDataRef::External { len, location } = account.data_ref else {
            return None;
        };
        if len != expected_data.len() {
            return None;
        }

        let data = self.read_account_data(&account).ok()?;
        (data == expected_data).then_some(location)
    }

    pub(crate) fn external_data_location(&self, offset: Offset) -> Option<DataLocation> {
        let account = self.read_meta_account(offset)?;
        let SplitDataRef::External { location, .. } = account.data_ref else {
            return None;
        };
        (!self.owns_data_location(location)).then_some(location)
    }

    pub(crate) fn write_accounts<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        skip: usize,
    ) -> Option<StoredAccountsInfo> {
        self.write_accounts_internal(accounts, skip, None)
    }

    pub(crate) fn write_accounts_with_reusable_data_refs<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        skip: usize,
        reusable_data_refs: &[Option<DataLocation>],
    ) -> Option<StoredAccountsInfo> {
        assert_eq!(accounts.len(), reusable_data_refs.len());
        self.write_accounts_internal(accounts, skip, Some(reusable_data_refs))
    }

    fn write_accounts_internal<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        skip: usize,
        reusable_data_refs: Option<&[Option<DataLocation>]>,
    ) -> Option<StoredAccountsInfo> {
        assert!(self.allow_writes, "append not allowed in read-only state");
        let _lock = self.append_lock.lock().unwrap();

        let len = accounts.len();
        let mut meta_offset = self.meta_len();
        let mut data_offset = self.data_len();
        let mut offsets = Vec::with_capacity(len.saturating_sub(skip));
        let mut stored_size = 0usize;
        let mut stop = false;

        for i in skip..len {
            if stop {
                break;
            }
            accounts.account_default_if_zero_lamport(i, |account| {
                let data_len = account.data().len();
                let meta_stored_size = Self::calculate_meta_stored_size(data_len);
                let reusable_data_ref = reusable_data_refs
                    .and_then(|refs| refs.get(i))
                    .copied()
                    .flatten()
                    .filter(|_| data_len != 0 && !should_store_internal(data_len));
                let data_stored_size = if should_store_internal(data_len)
                    || reusable_data_ref.is_some()
                {
                    0
                } else {
                    Self::calculate_data_stored_size(data_len)
                };
                let aligned_meta_offset = u64_align!(meta_offset);
                let aligned_data_offset = align_usize(data_offset, DATA_ALIGNMENT);
                let next_meta_offset = aligned_meta_offset.saturating_add(meta_stored_size);
                let next_data_offset = aligned_data_offset.saturating_add(data_stored_size);

                if next_meta_offset > self.meta_file_size || next_data_offset > self.data_file_size
                {
                    stop = true;
                    return;
                }

                let data_ref = if data_len == 0 {
                    SplitDataRef::None
                } else if should_store_internal(data_len) {
                    SplitDataRef::Internal { len: data_len }
                } else if let Some(location) = reusable_data_ref {
                    SplitDataRef::External {
                        len: data_len,
                        location,
                    }
                } else {
                    self.write_data_entry(aligned_data_offset, account.pubkey(), account.data())
                        .expect("must append split data entry");
                    data_offset = next_data_offset;
                    SplitDataRef::External {
                        len: data_len,
                        location: DataLocation {
                            slot: accounts.target_slot(),
                            id: self.id,
                            offset: aligned_data_offset,
                        },
                    }
                };

                self.write_meta_entry(aligned_meta_offset, &account, data_ref)
                    .expect("must append split meta entry");
                meta_offset = next_meta_offset;
                offsets.push(aligned_meta_offset);
                stored_size = stored_size
                    .saturating_add(meta_stored_size)
                    .saturating_add(data_stored_size);
            });
        }

        if offsets.is_empty() {
            return None;
        }

        self.meta_current_len.store(meta_offset, Ordering::Release);
        self.data_current_len.store(data_offset, Ordering::Release);
        self.is_dirty.store(true, Ordering::Release);

        Some(StoredAccountsInfo {
            offsets,
            size: stored_size,
        })
    }

    pub(crate) fn append_vec_archive_bytes(
        &self,
        obsolete_offsets: &HashSet<Offset>,
    ) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut offset = HEADER_SIZE;
        let meta_len = self.meta_len();
        while offset < meta_len {
            let Some(account) = self.read_meta_account(offset) else {
                break;
            };
            let next_offset = offset.saturating_add(account.stored_size);
            if !obsolete_offsets.contains(&offset) {
                let data = self.read_account_data(&account)?;
                let account = StoredAccountInfo {
                    pubkey: &account.pubkey,
                    lamports: account.lamports,
                    owner: &account.owner,
                    data: &data,
                    executable: account.executable,
                    rent_epoch: account.rent_epoch,
                };
                append_legacy_account_bytes(&mut bytes, &account);
            }
            offset = next_offset;
        }
        Ok(bytes)
    }

    pub(crate) fn meta_len(&self) -> usize {
        self.meta_current_len.load(Ordering::Acquire)
    }

    pub(crate) fn data_len(&self) -> usize {
        self.data_current_len.load(Ordering::Acquire)
    }

    fn calculate_meta_stored_size(data_len: usize) -> usize {
        if should_store_internal(data_len) {
            u64_align!(META_ENTRY_FIXED_SIZE.saturating_add(data_len))
        } else {
            u64_align!(META_ENTRY_FIXED_SIZE)
        }
    }

    fn calculate_data_stored_size(data_len: usize) -> usize {
        align_usize(DATA_ENTRY_FIXED_SIZE.saturating_add(data_len), DATA_ALIGNMENT)
    }

    fn account_stored_size(&self, account: &SplitStoredAccount) -> usize {
        let data_stored_size = match account.data_ref {
            SplitDataRef::External { len, location } if self.owns_data_location(location) => {
                Self::calculate_data_stored_size(len)
            }
            SplitDataRef::None | SplitDataRef::Internal { .. } | SplitDataRef::External { .. } => {
                0
            }
        };
        account.stored_size.saturating_add(data_stored_size)
    }

    fn read_meta_account(&self, offset: usize) -> Option<SplitStoredAccount> {
        let meta_len = self.meta_len();
        if offset.checked_add(META_ENTRY_FIXED_SIZE)? > meta_len {
            return None;
        }

        let mut fixed = [MaybeUninit::<u8>::uninit(); META_ENTRY_FIXED_SIZE];
        let bytes_read = read_into_buffer(
            &self.meta_file,
            meta_len as FileSize,
            offset as FileSize,
            unsafe {
                slice::from_raw_parts_mut(fixed.as_mut_ptr() as *mut u8, META_ENTRY_FIXED_SIZE)
            },
        )
        .ok()?;
        if bytes_read < META_ENTRY_FIXED_SIZE {
            return None;
        }
        let fixed =
            unsafe { slice::from_raw_parts(fixed.as_ptr() as *const u8, META_ENTRY_FIXED_SIZE) };

        let pubkey = Pubkey::new_from_array(read_array(fixed, META_ADDRESS_OFFSET)?);
        let owner = Pubkey::new_from_array(read_array(fixed, META_OWNER_OFFSET)?);
        let lamports = u64::from_le_bytes(read_array(fixed, META_LAMPORTS_OFFSET)?);
        let rent_epoch = u64::from_le_bytes(read_array(fixed, META_RENT_EPOCH_OFFSET)?);
        let executable = match *fixed.get(META_EXECUTABLE_OFFSET)? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let data_len = u32::from_le_bytes(read_array(fixed, META_DATA_LEN_OFFSET)?) as usize;
        let data_ref = match *fixed.get(META_DATA_REF_KIND_OFFSET)? {
            DATA_REF_NONE => {
                if data_len != 0 {
                    return None;
                }
                SplitDataRef::None
            }
            DATA_REF_INTERNAL => SplitDataRef::Internal { len: data_len },
            DATA_REF_EXTERNAL => {
                let slot = u64::from_le_bytes(read_array(fixed, META_DATA_SLOT_OFFSET)?);
                let id = u32::from_le_bytes(read_array(fixed, META_DATA_ID_OFFSET)?);
                let offset_reduced =
                    u32::from_le_bytes(read_array(fixed, META_DATA_OFFSET_REDUCED_OFFSET)?);
                SplitDataRef::External {
                    len: data_len,
                    location: DataLocation {
                        slot,
                        id,
                        offset: (offset_reduced as usize) << DATA_OFFSET_SHIFT,
                    },
                }
            }
            _ => return None,
        };
        let stored_size = Self::calculate_meta_stored_size(data_len);
        if offset.checked_add(stored_size)? > meta_len {
            return None;
        }

        Some(SplitStoredAccount {
            offset,
            pubkey,
            owner,
            lamports,
            rent_epoch,
            executable,
            data_ref,
            stored_size,
        })
    }

    fn read_account_data(&self, account: &SplitStoredAccount) -> io::Result<Vec<u8>> {
        match account.data_ref {
            SplitDataRef::None => Ok(Vec::new()),
            SplitDataRef::Internal { len } => {
                let mut data = vec![0; len];
                let data_offset = account.offset + META_ENTRY_FIXED_SIZE;
                read_exact_at(&self.meta_file, self.meta_len(), data_offset, &mut data)?;
                Ok(data)
            }
            SplitDataRef::External { len, location } => {
                if self.owns_data_location(location) {
                    return Self::read_data_entry_from_file(
                        &self.data_file,
                        self.data_len(),
                        location.offset,
                        &account.pubkey,
                        len,
                    );
                }

                let data_path = data_path_for_base(&self.base_path_for_location(location));
                let data_file_info = FileInfo::new_from_path(&data_path)?;
                read_header(&data_file_info.file, DATA_MAGIC)?;
                Self::read_data_entry_from_file(
                    &data_file_info.file,
                    data_file_info.size as usize,
                    location.offset,
                    &account.pubkey,
                    len,
                )
            }
        }
    }

    fn read_data_entry_from_file(
        data_file: &File,
        data_len: usize,
        offset: usize,
        expected_pubkey: &Pubkey,
        expected_len: usize,
    ) -> io::Result<Vec<u8>> {
        let stored_size = Self::calculate_data_stored_size(expected_len);
        if offset
            .checked_add(stored_size)
            .is_none_or(|end| end > data_len)
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "split data entry exceeds data file length",
            ));
        }

        let mut fixed = [0u8; DATA_ENTRY_FIXED_SIZE];
        read_exact_at(data_file, data_len, offset, &mut fixed)?;
        let pubkey = Pubkey::new_from_array(
            fixed[DATA_ADDRESS_OFFSET..DATA_ADDRESS_OFFSET + 32]
                .try_into()
                .expect("slice has correct size"),
        );
        let len = u32::from_le_bytes(
            fixed[DATA_LEN_OFFSET..DATA_LEN_OFFSET + 4]
                .try_into()
                .expect("slice has correct size"),
        ) as usize;
        if &pubkey != expected_pubkey || len != expected_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "split data entry metadata mismatch",
            ));
        }

        let mut data = vec![0; expected_len];
        read_exact_at(data_file, data_len, offset + DATA_ENTRY_FIXED_SIZE, &mut data)?;
        Ok(data)
    }

    fn owns_data_location(&self, location: DataLocation) -> bool {
        location.slot == self.slot && location.id == self.id
    }

    fn base_path_for_location(&self, location: DataLocation) -> PathBuf {
        self.base_path
            .with_file_name(format!("{}.{}", location.slot, location.id))
    }

    fn write_data_entry(&self, offset: usize, pubkey: &Pubkey, data: &[u8]) -> io::Result<()> {
        debug_assert_eq!(offset % DATA_ALIGNMENT, 0);
        let len = u32::try_from(data.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "account data length exceeds split data entry limit",
            )
        })?;
        let mut fixed = [0u8; DATA_ENTRY_FIXED_SIZE];
        fixed[DATA_ADDRESS_OFFSET..DATA_ADDRESS_OFFSET + 32].copy_from_slice(pubkey.as_ref());
        fixed[DATA_LEN_OFFSET..DATA_LEN_OFFSET + 4].copy_from_slice(&len.to_le_bytes());
        write_buffer_to_file(&self.data_file, &fixed, offset as u64)?;
        write_buffer_to_file(
            &self.data_file,
            data,
            (offset + DATA_ENTRY_FIXED_SIZE) as u64,
        )
    }

    fn write_meta_entry(
        &self,
        offset: usize,
        account: &impl ReadableAccountPubkey,
        data_ref: SplitDataRef,
    ) -> io::Result<()> {
        debug_assert_eq!(offset % META_ALIGNMENT, 0);
        let data_len = data_ref.len();
        let data_len_u32 = u32::try_from(data_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "account data length exceeds split meta entry limit",
            )
        })?;
        let mut fixed = [0u8; META_ENTRY_FIXED_SIZE];
        fixed[META_ADDRESS_OFFSET..META_ADDRESS_OFFSET + 32]
            .copy_from_slice(account.pubkey().as_ref());
        fixed[META_OWNER_OFFSET..META_OWNER_OFFSET + 32].copy_from_slice(account.owner().as_ref());
        fixed[META_LAMPORTS_OFFSET..META_LAMPORTS_OFFSET + 8]
            .copy_from_slice(&account.lamports().to_le_bytes());
        fixed[META_RENT_EPOCH_OFFSET..META_RENT_EPOCH_OFFSET + 8]
            .copy_from_slice(&account.rent_epoch().to_le_bytes());
        fixed[META_EXECUTABLE_OFFSET] = account.executable().into();
        fixed[META_DATA_LEN_OFFSET..META_DATA_LEN_OFFSET + 4]
            .copy_from_slice(&data_len_u32.to_le_bytes());

        match data_ref {
            SplitDataRef::None => fixed[META_DATA_REF_KIND_OFFSET] = DATA_REF_NONE,
            SplitDataRef::Internal { .. } => fixed[META_DATA_REF_KIND_OFFSET] = DATA_REF_INTERNAL,
            SplitDataRef::External { location, .. } => {
                fixed[META_DATA_REF_KIND_OFFSET] = DATA_REF_EXTERNAL;
                fixed[META_DATA_SLOT_OFFSET..META_DATA_SLOT_OFFSET + 8]
                    .copy_from_slice(&location.slot.to_le_bytes());
                fixed[META_DATA_ID_OFFSET..META_DATA_ID_OFFSET + 4]
                    .copy_from_slice(&location.id.to_le_bytes());
                let reduced_offset = u32::try_from(location.offset >> DATA_OFFSET_SHIFT).map_err(
                    |_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "split data offset exceeds reduced offset limit",
                        )
                    },
                )?;
                fixed[META_DATA_OFFSET_REDUCED_OFFSET..META_DATA_OFFSET_REDUCED_OFFSET + 4]
                    .copy_from_slice(&reduced_offset.to_le_bytes());
            }
        }

        write_buffer_to_file(&self.meta_file, &fixed, offset as u64)?;
        if matches!(data_ref, SplitDataRef::Internal { .. }) {
            write_buffer_to_file(
                &self.meta_file,
                account.data(),
                (offset + META_ENTRY_FIXED_SIZE) as u64,
            )?;
        }
        Ok(())
    }
}

trait ReadableAccountPubkey: ReadableAccount {
    fn pubkey(&self) -> &Pubkey;
}

impl ReadableAccountPubkey for crate::storable_accounts::AccountForStorage<'_> {
    fn pubkey(&self) -> &Pubkey {
        self.pubkey()
    }
}

impl SplitDataRef {
    fn len(&self) -> usize {
        match self {
            SplitDataRef::None => 0,
            SplitDataRef::Internal { len } | SplitDataRef::External { len, .. } => *len,
        }
    }
}

fn open_sized_file(path: &Path, create: bool, size: usize) -> io::Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .open(path)?;
    file.seek(SeekFrom::Start((size - 1) as u64))?;
    file.write_all(&[0])?;
    file.rewind()?;
    file.flush()?;
    Ok(file)
}

fn write_header(
    file: &mut File,
    magic: &[u8; 8],
    alignment_log2: u8,
    slot: Slot,
    id: u32,
    used_len: usize,
) -> io::Result<()> {
    let mut header = [0u8; HEADER_SIZE];
    header[HEADER_MAGIC_OFFSET..HEADER_MAGIC_OFFSET + 8].copy_from_slice(magic);
    header[HEADER_VERSION_OFFSET..HEADER_VERSION_OFFSET + 2]
        .copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[HEADER_HEADER_LEN_OFFSET..HEADER_HEADER_LEN_OFFSET + 2]
        .copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    header[HEADER_ALIGNMENT_LOG2_OFFSET] = alignment_log2;
    header[HEADER_SLOT_OFFSET..HEADER_SLOT_OFFSET + 8].copy_from_slice(&slot.to_le_bytes());
    header[HEADER_ID_OFFSET..HEADER_ID_OFFSET + 4].copy_from_slice(&id.to_le_bytes());
    header[HEADER_USED_LEN_OFFSET..HEADER_USED_LEN_OFFSET + 8]
        .copy_from_slice(&(used_len as u64).to_le_bytes());
    file.rewind()?;
    file.write_all(&header)?;
    file.flush()
}

fn read_header(file: &File, expected_magic: &[u8; 8]) -> io::Result<usize> {
    let mut header = [0u8; HEADER_SIZE];
    read_exact_at(file, HEADER_SIZE, 0, &mut header)?;
    if &header[HEADER_MAGIC_OFFSET..HEADER_MAGIC_OFFSET + 8] != expected_magic {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "split accounts file magic mismatch",
        ));
    }
    let version = u16::from_le_bytes(
        header[HEADER_VERSION_OFFSET..HEADER_VERSION_OFFSET + 2]
            .try_into()
            .expect("slice has correct size"),
    );
    let header_len = u16::from_le_bytes(
        header[HEADER_HEADER_LEN_OFFSET..HEADER_HEADER_LEN_OFFSET + 2]
            .try_into()
            .expect("slice has correct size"),
    );
    if version != FORMAT_VERSION || header_len as usize != HEADER_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported split accounts file header",
        ));
    }
    let used_len = u64::from_le_bytes(
        header[HEADER_USED_LEN_OFFSET..HEADER_USED_LEN_OFFSET + 8]
            .try_into()
            .expect("slice has correct size"),
    );
    usize::try_from(used_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "split used length too large"))
}

fn write_header_used_len(file: &File, used_len: usize) -> io::Result<()> {
    write_buffer_to_file(
        file,
        &(used_len as u64).to_le_bytes(),
        HEADER_USED_LEN_OFFSET as u64,
    )
}

fn read_exact_at(
    file: &File,
    valid_file_len: usize,
    start_offset: usize,
    buffer: &mut [u8],
) -> io::Result<()> {
    let bytes_read = read_into_buffer(
        file,
        valid_file_len as FileSize,
        start_offset as FileSize,
        buffer,
    )?;
    if bytes_read == buffer.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "short split accounts file read",
        ))
    }
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset + N)?.try_into().ok()
}

fn align_usize(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn should_store_internal(data_len: usize) -> bool {
    data_len == 0 || u64_align!(META_ENTRY_FIXED_SIZE.saturating_add(data_len)) <= HEADER_SIZE
}

fn meta_path_for_base(base_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta", base_path.display()))
}

fn data_path_for_base(base_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.data", base_path.display()))
}

fn base_path_for_meta_path(meta_path: &Path) -> PathBuf {
    let Some(name) = meta_path.file_name().and_then(|name| name.to_str()) else {
        return meta_path.to_path_buf();
    };
    let Some(base_name) = name.strip_suffix(".meta") else {
        return meta_path.to_path_buf();
    };
    meta_path.with_file_name(base_name)
}

fn parse_slot_and_id(base_path: &Path) -> Option<(Slot, u32)> {
    let filename = base_path.file_name()?.to_str()?;
    let (slot, id) = filename.split_once('.')?;
    Some((slot.parse().ok()?, id.parse().ok()?))
}

fn append_legacy_account_bytes(bytes: &mut Vec<u8>, account: &StoredAccountInfo<'_>) {
    bytes.resize(u64_align!(bytes.len()), 0);
    bytes.extend_from_slice(&0u64.to_ne_bytes());
    bytes.extend_from_slice(&(account.data.len() as u64).to_ne_bytes());
    bytes.extend_from_slice(account.pubkey.as_ref());
    bytes.extend_from_slice(&account.lamports.to_ne_bytes());
    bytes.extend_from_slice(&account.rent_epoch.to_ne_bytes());
    bytes.extend_from_slice(account.owner.as_ref());
    bytes.push(account.executable.into());
    bytes.resize(bytes.len() + 7, 0);
    bytes.extend_from_slice(&[0u8; 32]);
    bytes.extend_from_slice(account.data);
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::append_vec::{AppendVec, new_scan_accounts_reader},
        solana_account::{WritableAccount, accounts_equal},
        tempfile::TempDir,
    };

    fn new_test_split_file(payload_size: usize) -> (TempDir, SplitAccountsFile) {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("42.7");
        let split = SplitAccountsFile::new(path, true, payload_size);
        (temp_dir, split)
    }

    fn new_account(lamports: u64, data_len: usize) -> (Pubkey, AccountSharedData) {
        let pubkey = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let mut account = AccountSharedData::new(lamports, data_len, &owner);
        account.set_data((0..data_len).map(|i| (i % 251) as u8).collect());
        (pubkey, account)
    }

    #[test]
    fn test_split_accounts_file_roundtrip_internal_and_external() {
        let (_temp_dir, split) = new_test_split_file(32 * 1024);
        let small = new_account(11, 16);
        let large = new_account(22, HEADER_SIZE);
        let accounts = [(&small.0, &small.1), (&large.0, &large.1)];

        let stored = split.write_accounts(&(42, &accounts[..]), 0).unwrap();
        assert_eq!(stored.offsets.len(), 2);
        assert_eq!(stored.offsets[0], HEADER_SIZE);
        assert!(stored.size >= SplitAccountsFile::calculate_stored_size(large.1.data().len()));

        for (offset, expected) in stored.offsets.iter().zip([&small, &large]) {
            split
                .get_stored_account_without_data_callback(*offset, |account| {
                    assert_eq!(account.pubkey(), &expected.0);
                    assert_eq!(account.lamports, expected.1.lamports());
                    assert_eq!(account.owner, expected.1.owner());
                    assert_eq!(account.data_len, expected.1.data().len());
                })
                .unwrap();

            let loaded = split.get_account_shared_data(*offset).unwrap();
            assert!(accounts_equal(&loaded, &expected.1));
        }

        let mut scanned = Vec::new();
        split
            .scan_accounts_without_data(|offset, account| {
                scanned.push((offset, *account.pubkey(), account.data_len));
            })
            .unwrap();
        assert_eq!(
            scanned,
            vec![
                (stored.offsets[0], small.0, small.1.data().len()),
                (stored.offsets[1], large.0, large.1.data().len()),
            ]
        );
    }

    #[test]
    fn test_split_accounts_file_reuses_external_data_from_sibling_file() {
        let temp_dir = TempDir::new().unwrap();
        let old_split = SplitAccountsFile::new(temp_dir.path().join("42.7"), true, 32 * 1024);
        let new_split = SplitAccountsFile::new(temp_dir.path().join("43.8"), true, 32 * 1024);
        assert!(new_split.can_reference_data_from(&old_split));

        let (pubkey, account) = new_account(22, HEADER_SIZE);
        let accounts = [(&pubkey, &account)];
        let old_offset = old_split
            .write_accounts(&(42, &accounts[..]), 0)
            .unwrap()
            .offsets[0];
        old_split.flush().unwrap();
        assert!(old_split.data_len() > HEADER_SIZE);

        let location = old_split
            .reusable_external_data_location(old_offset, &pubkey, account.data())
            .unwrap();
        let mut updated_account = account.clone();
        updated_account.set_lamports(account.lamports() + 1);
        let updated_accounts = [(&pubkey, &updated_account)];
        let new_stored = new_split
            .write_accounts_with_reusable_data_refs(
                &(43, &updated_accounts[..]),
                0,
                &[Some(location)],
            )
            .unwrap();

        assert_eq!(new_split.data_len(), HEADER_SIZE);
        assert_eq!(
            new_split.get_account_stored_sizes(&new_stored.offsets),
            vec![SplitAccountsFile::calculate_meta_stored_size(account.data().len())],
        );
        let loaded = new_split
            .get_account_shared_data(new_stored.offsets[0])
            .unwrap();
        assert!(accounts_equal(&loaded, &updated_account));
    }

    #[test]
    fn test_split_accounts_file_writes_headers_and_reopens() {
        let (temp_dir, split) = new_test_split_file(16 * 1024);
        let account = new_account(33, 128);
        let accounts = [(&account.0, &account.1)];
        let offset = split.write_accounts(&(42, &accounts[..]), 0).unwrap().offsets[0];
        split.flush().unwrap();

        let meta_path = temp_dir.path().join("42.7.meta");
        let data_path = temp_dir.path().join("42.7.data");
        assert!(meta_path.exists());
        assert!(data_path.exists());

        let reopened = SplitAccountsFile::new_for_startup(FileInfo::new_from_path(meta_path).unwrap())
            .unwrap();
        let loaded = reopened.get_account_shared_data(offset).unwrap();
        assert!(accounts_equal(&loaded, &account.1));
    }

    #[test]
    fn test_split_accounts_file_synthesizes_legacy_append_vec_bytes() {
        let (temp_dir, split) = new_test_split_file(32 * 1024);
        let first = new_account(44, 64);
        let second = new_account(55, HEADER_SIZE);
        let accounts = [(&first.0, &first.1), (&second.0, &second.1)];
        let stored = split.write_accounts(&(42, &accounts[..]), 0).unwrap();

        let archive_bytes = split.append_vec_archive_bytes(&HashSet::new()).unwrap();
        let legacy_path = temp_dir.path().join("legacy-append-vec");
        std::fs::write(&legacy_path, &archive_bytes).unwrap();
        let legacy = AppendVec::new_from_file(&legacy_path, archive_bytes.len())
            .unwrap()
            .0;

        let mut reader = new_scan_accounts_reader();
        let mut loaded = Vec::new();
        legacy
            .scan_accounts(&mut reader, |_offset, account| {
                loaded.push((*account.pubkey, create_account_shared_data(&account)));
            })
            .unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].0, first.0);
        assert!(accounts_equal(&loaded[0].1, &first.1));
        assert_eq!(loaded[1].0, second.0);
        assert!(accounts_equal(&loaded[1].1, &second.1));

        let archive_bytes = split
            .append_vec_archive_bytes(&HashSet::from([stored.offsets[0]]))
            .unwrap();
        let legacy_path = temp_dir.path().join("legacy-append-vec-with-obsolete");
        std::fs::write(&legacy_path, &archive_bytes).unwrap();
        let legacy = AppendVec::new_from_file(&legacy_path, archive_bytes.len())
            .unwrap()
            .0;

        let mut reader = new_scan_accounts_reader();
        let mut loaded = Vec::new();
        legacy
            .scan_accounts(&mut reader, |_offset, account| {
                loaded.push((*account.pubkey, create_account_shared_data(&account)));
            })
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, second.0);
        assert!(accounts_equal(&loaded[0].1, &second.1));
    }
}
