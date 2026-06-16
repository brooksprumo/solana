use {
    crate::{
        account_info::Offset,
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        accounts_db::AccountsFileId,
        append_vec::{AppendVec, AppendVecError},
        split_accounts_file::{DataLocation, SplitAccountsFile},
        storable_accounts::StorableAccounts,
    },
    agave_fs::{FileInfo, buffered_reader::RequiredLenBufFileRead, file_io::open_for_reading},
    solana_account::AccountSharedData,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
        fs::File,
        io, mem,
        path::{Path, PathBuf},
    },
    thiserror::Error,
};

// Data placement should be aligned at the next boundary. Without alignment accessing the memory may
// crash on some architectures.
pub const ALIGN_BOUNDARY_OFFSET: usize = mem::size_of::<u64>();
#[macro_export]
macro_rules! u64_align {
    ($addr: expr) => {
        ($addr + ($crate::accounts_file::ALIGN_BOUNDARY_OFFSET - 1))
            & !($crate::accounts_file::ALIGN_BOUNDARY_OFFSET - 1)
    };
}

pub type Result<T> = std::result::Result<T, AccountsFileError>;

/// An enum for AccountsFile related errors.
#[derive(Error, Debug)]
pub enum AccountsFileError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("AppendVecError: {0}")]
    AppendVecError(#[from] AppendVecError),
}

#[derive(Debug)]
/// An enum for accessing an accounts file which can be implemented
/// under different formats.
pub enum AccountsFile {
    AppendVec(AppendVec),
    Split(SplitAccountsFile),
}

impl AccountsFile {
    /// Create an AccountsFile instance from the specified path.
    ///
    /// The second element of the returned tuple is the number of accounts in the
    /// accounts file.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_from_file(path: impl Into<PathBuf>, current_len: usize) -> Result<(Self, usize)> {
        let (av, num_accounts) = AppendVec::new_from_file(path, current_len)?;
        Ok((Self::AppendVec(av), num_accounts))
    }

    /// Creates a new AccountsFile for the underlying storage at `file_info`
    ///
    /// This version of `new()` may only be called when reconstructing storages as part of startup.
    /// The storage length is taken to be the full file size; this is trusted and relies on later
    /// index generation or accounts verification to ensure it is valid.
    pub fn new_for_startup(file_info: FileInfo) -> Result<Self> {
        if file_info
            .path
            .extension()
            .is_some_and(|extension| extension == "meta")
        {
            Ok(Self::Split(SplitAccountsFile::new_for_startup(file_info)?))
        } else {
            let av = AppendVec::new_for_startup(file_info)?;
            Ok(Self::AppendVec(av))
        }
    }

    /// if storage is not readonly, reopen another instance that is read only
    pub(crate) fn reopen_as_readonly(&self) -> Option<Self> {
        match self {
            Self::AppendVec(av) => av.reopen_as_readonly_file_io().map(Self::AppendVec),
            Self::Split(split) => split.reopen_as_readonly_file_io().map(Self::Split),
        }
    }

    /// Detach the on-disk file from this storage's lifetime; see
    /// [`AppendVec::disable_remove_on_drop`].
    pub fn disable_remove_on_drop(&self) {
        match self {
            Self::AppendVec(av) => av.disable_remove_on_drop(),
            Self::Split(split) => split.disable_remove_on_drop(),
        }
    }

    /// Return the total number of bytes of the zero lamport single ref accounts in the storage.
    /// Those bytes are "dead" and can be shrunk away.
    pub(crate) fn dead_bytes_due_to_zero_lamport_single_ref(&self, count: usize) -> usize {
        match self {
            Self::AppendVec(av) => av.dead_bytes_due_to_zero_lamport_single_ref(count),
            Self::Split(split) => split.dead_bytes_due_to_zero_lamport_single_ref(count),
        }
    }

    /// Flushes contents to disk
    pub fn flush(&self) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.flush()?,
            Self::Split(split) => split.flush()?,
        }
        Ok(())
    }

    pub fn remaining_bytes(&self) -> u64 {
        match self {
            Self::AppendVec(av) => av.remaining_bytes(),
            Self::Split(split) => split.remaining_bytes(),
        }
    }

    /// Returns the number of bytes, *not accounts*, used in the AccountsFile
    pub fn len(&self) -> usize {
        match self {
            Self::AppendVec(av) => av.len(),
            Self::Split(split) => split.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::AppendVec(av) => av.is_empty(),
            Self::Split(split) => split.is_empty(),
        }
    }

    /// Returns the total number of bytes, *not accounts*, the AccountsFile can hold
    pub fn capacity(&self) -> u64 {
        match self {
            Self::AppendVec(av) => av.capacity(),
            Self::Split(split) => split.capacity(),
        }
    }

    pub fn file_name(slot: Slot, id: AccountsFileId) -> String {
        format!("{slot}.{id}")
    }

    /// Calls `callback` with the stored account at `offset`.
    ///
    /// Returns `None` if there is no account at `offset`, otherwise returns the result of
    /// `callback` in `Some`.
    ///
    /// This fn does *not* load the account's data, just the data length.  If the data is needed,
    /// use `get_stored_account_callback()` instead.  However, prefer this fn when possible.
    pub fn get_stored_account_without_data_callback<Ret>(
        &self,
        offset: usize,
        callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>) -> Ret,
    ) -> Option<Ret> {
        match self {
            Self::AppendVec(av) => av.get_stored_account_without_data_callback(offset, callback),
            Self::Split(split) => split.get_stored_account_without_data_callback(offset, callback),
        }
    }

    /// Calls `callback` with the stored account at `offset`.
    ///
    /// Returns `None` if there is no account at `offset`, otherwise returns the result of
    /// `callback` in `Some`.
    ///
    /// This fn *does* load the account's data.  If the data is not needed,
    /// use `get_stored_account_without_data_callback()` instead.
    pub fn get_stored_account_callback<Ret>(
        &self,
        offset: usize,
        callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> Option<Ret> {
        match self {
            Self::AppendVec(av) => av.get_stored_account_callback(offset, callback),
            Self::Split(split) => split.get_stored_account_callback(offset, callback),
        }
    }

    /// return an `AccountSharedData` for an account at `offset`, if any.  Otherwise return None.
    pub(crate) fn get_account_shared_data(&self, offset: usize) -> Option<AccountSharedData> {
        match self {
            Self::AppendVec(av) => av.get_account_shared_data(offset),
            Self::Split(split) => split.get_account_shared_data(offset),
        }
    }

    /// Return the path of the underlying account file.
    pub fn path(&self) -> &Path {
        match self {
            Self::AppendVec(av) => av.path(),
            Self::Split(split) => split.path(),
        }
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * Offset: the offset within the file of this account
    /// * StoredAccountInfoWithoutData: the account itself, without account data
    ///
    /// Note that account data is not read/passed to the callback.
    pub fn scan_accounts_without_data(
        &self,
        callback: impl for<'local> FnMut(Offset, StoredAccountInfoWithoutData<'local>),
    ) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_accounts_without_data(callback)?,
            Self::Split(split) => split.scan_accounts_without_data(callback)?,
        }
        Ok(())
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * Offset: the offset within the file of this account
    /// * StoredAccountInfo: the account itself, with account data
    ///
    /// Prefer scan_accounts_without_data() when account data is not needed,
    /// as it can potentially read less and be faster.
    pub(crate) fn scan_accounts<'a>(
        &'a self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_accounts(reader, callback)?,
            Self::Split(split) => split.scan_accounts(reader, callback)?,
        }
        Ok(())
    }

    /// Calculate the amount of storage required for an account with the passed
    /// in data_len
    pub(crate) fn calculate_stored_size(&self, data_len: usize) -> usize {
        match self {
            Self::AppendVec(_) => AppendVec::calculate_stored_size(data_len),
            Self::Split(_) => SplitAccountsFile::calculate_stored_size(data_len),
        }
    }

    /// for each offset in `sorted_offsets`, get the data size
    pub(crate) fn get_account_data_lens(&self, sorted_offsets: &[usize]) -> Vec<usize> {
        match self {
            Self::AppendVec(av) => av.get_account_data_lens(sorted_offsets),
            Self::Split(split) => split.get_account_data_lens(sorted_offsets),
        }
    }

    pub(crate) fn get_account_stored_sizes(&self, sorted_offsets: &[usize]) -> Vec<usize> {
        match self {
            Self::AppendVec(av) => {
                let file_len = av.len();
                av.get_account_data_lens(sorted_offsets)
                    .into_iter()
                    .zip(sorted_offsets)
                    .map(|(data_len, offset)| {
                        AppendVec::calculate_stored_size(data_len)
                            .min(file_len.saturating_sub(*offset))
                    })
                    .collect()
            }
            Self::Split(split) => split.get_account_stored_sizes(sorted_offsets),
        }
    }

    /// iterate over all pubkeys
    pub fn scan_pubkeys(&self, callback: impl FnMut(&Pubkey)) -> Result<()> {
        match self {
            Self::AppendVec(av) => av.scan_pubkeys(callback)?,
            Self::Split(split) => split.scan_pubkeys(callback)?,
        }
        Ok(())
    }

    /// Copy each account metadata, account and hash to the internal buffer.
    /// If there is no room to write the first entry, None is returned.
    /// Otherwise, returns the starting offset of each account metadata.
    /// Plus, the final return value is the offset where the next entry would be appended.
    /// So, return.len() is 1 + (number of accounts written)
    /// After each account is appended, the internal `current_len` is updated
    /// and will be available to other threads.
    pub fn write_accounts<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        skip: usize,
    ) -> Option<StoredAccountsInfo> {
        match self {
            Self::AppendVec(av) => av.append_accounts(accounts, skip),
            Self::Split(split) => split.write_accounts(accounts, skip),
        }
    }

    pub fn is_split(&self) -> bool {
        matches!(self, Self::Split(_))
    }

    pub(crate) fn can_reuse_split_data_from(&self, old: &Self) -> bool {
        match (self, old) {
            (Self::Split(new), Self::Split(old)) => new.can_reference_data_from(old),
            _ => false,
        }
    }

    pub(crate) fn reusable_split_data_location(
        &self,
        offset: Offset,
        expected_pubkey: &Pubkey,
        expected_data: &[u8],
    ) -> Option<DataLocation> {
        match self {
            Self::Split(split) => {
                split.reusable_external_data_location(offset, expected_pubkey, expected_data)
            }
            Self::AppendVec(_) => None,
        }
    }

    pub(crate) fn split_external_data_location(&self, offset: Offset) -> Option<DataLocation> {
        match self {
            Self::Split(split) => split.external_data_location(offset),
            Self::AppendVec(_) => None,
        }
    }

    pub(crate) fn split_external_data_locations(
        &self,
        sorted_offsets: &[Offset],
    ) -> Vec<DataLocation> {
        sorted_offsets
            .iter()
            .filter_map(|&offset| self.split_external_data_location(offset))
            .collect()
    }

    pub(crate) fn write_accounts_with_reusable_split_data_refs<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        skip: usize,
        reusable_data_refs: &[Option<DataLocation>],
    ) -> Option<StoredAccountsInfo> {
        match self {
            Self::Split(split) => {
                split.write_accounts_with_reusable_data_refs(accounts, skip, reusable_data_refs)
            }
            Self::AppendVec(_) => self.write_accounts(accounts, skip),
        }
    }

    #[cfg(test)]
    pub(crate) fn split_data_len_for_tests(&self) -> Option<usize> {
        match self {
            Self::Split(split) => Some(split.data_len()),
            Self::AppendVec(_) => None,
        }
    }

    pub fn append_vec_archive_bytes(
        &self,
        obsolete_offsets: &std::collections::HashSet<Offset>,
    ) -> io::Result<Vec<u8>> {
        match self {
            Self::AppendVec(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "append-vec archive byte synthesis is only needed for split accounts files",
            )),
            Self::Split(split) => split.append_vec_archive_bytes(obsolete_offsets),
        }
    }

    /// Returns a file handle suitable for archive-style reads. With
    /// `use_direct_io = true` a fresh fd is opened with `O_DIRECT`; otherwise
    /// the `AccountsFile`'s existing fd is borrowed, saving one fd per storage.
    pub fn open_file_for_archive(&self, use_direct_io: bool) -> io::Result<OpenFileForArchive<'_>> {
        if self.is_split() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "split accounts files require synthesized archive bytes",
            ));
        }
        if use_direct_io {
            open_for_reading(self.path(), true).map(OpenFileForArchive::Owned)
        } else {
            Ok(match self {
                Self::AppendVec(av) => av.open_file_for_archive(),
                Self::Split(_) => unreachable!("split files returned above"),
            })
        }
    }
}

/// An enum that creates AccountsFile instance with the specified format.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub enum AccountsFileProvider {
    #[default]
    AppendVec,
    Split,
}

impl AccountsFileProvider {
    pub fn calculate_stored_size(&self, data_len: usize) -> usize {
        match self {
            Self::AppendVec => AppendVec::calculate_stored_size(data_len),
            Self::Split => SplitAccountsFile::calculate_stored_size(data_len),
        }
    }

    pub fn new_writable(&self, path: impl Into<PathBuf>, file_size: u64) -> AccountsFile {
        match self {
            Self::AppendVec => {
                AccountsFile::AppendVec(AppendVec::new(path, true, file_size as usize))
            }
            Self::Split => AccountsFile::Split(SplitAccountsFile::new(
                path,
                true,
                file_size as usize,
            )),
        }
    }
}

/// The access method to use when archiving an AccountsFile
#[derive(Debug)]
pub enum OpenFileForArchive<'a> {
    /// Borrowed `AccountsFile` fd; lacks `O_DIRECT`, so reads go through the
    /// kernel page cache (incompatible with direct-I/O reads).
    Borrowed(&'a File),
    /// Freshly opened fd, typically with `O_DIRECT` on Linux.
    Owned(File),
}

impl AsRef<File> for OpenFileForArchive<'_> {
    fn as_ref(&self) -> &File {
        match self {
            Self::Borrowed(f) => f,
            Self::Owned(f) => f,
        }
    }
}

/// Information after storing accounts
#[derive(Debug)]
pub struct StoredAccountsInfo {
    /// offset in the storage where each account was stored
    pub offsets: Vec<usize>,
    /// total size of all the stored accounts
    pub size: usize,
}
