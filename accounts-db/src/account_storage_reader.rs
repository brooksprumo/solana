use {
    crate::{
        account_info::Offset, account_storage::stored_account_info::StoredAccountInfo,
        account_storage_entry::AccountStorageEntry, accounts_file::OpenFileForArchive, u64_align,
    },
    agave_fs::{
        buffered_reader::{self, FileBufRead},
        io_setup::IoSetupState,
    },
    solana_clock::Slot,
    std::{
        collections::HashSet,
        io::{self, Cursor, Read},
    },
};

const LEGACY_APPEND_VEC_STORED_META_SIZE: usize = 0x30;
const LEGACY_APPEND_VEC_ACCOUNT_META_SIZE: usize = 0x38;
const LEGACY_APPEND_VEC_ACCOUNT_HASH_SIZE: usize = 32;
const LEGACY_APPEND_VEC_ACCOUNT_OVERHEAD: usize = LEGACY_APPEND_VEC_STORED_META_SIZE
    + LEGACY_APPEND_VEC_ACCOUNT_META_SIZE
    + LEGACY_APPEND_VEC_ACCOUNT_HASH_SIZE;

// Read-ahead buffer capacity, sized as a multiple of the default io-uring
// reader's read size (1 MiB) and large enough that almost any account storage
// file fits entirely within the buffer.
pub const ACCOUNT_STORAGE_MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024;

#[cfg(not(target_os = "linux"))]
const READER_STACK_BUFFER_SIZE: usize = 64 * 1024;

/// Concrete reader type returned by [`storage_file_buf_reader`].
///
/// The concrete type is exposed (rather than `impl FileBufRead<'a>`) so callers
/// can use inherent methods like `rebind`.
#[cfg(target_os = "linux")]
type StorageFileBufReader<'a> = buffered_reader::SequentialFileReader<'a>;
#[cfg(not(target_os = "linux"))]
type StorageFileBufReader<'a> = buffered_reader::BufferedReader<'a, READER_STACK_BUFFER_SIZE>;

/// When `use_page_cache` is `true`, direct I/O is forced off regardless of
/// `io_setup.use_direct_io` so that reads can hit the kernel's page cache.
/// Otherwise, the `io_setup.use_direct_io` setting is honored.
pub fn storage_file_buf_reader<'a>(
    max_buf_size: usize,
    use_page_cache: bool,
    io_setup: &IoSetupState,
) -> io::Result<StorageFileBufReader<'a>> {
    #[cfg(target_os = "linux")]
    {
        buffered_reader::SequentialFileReaderBuilder::new()
            .shared_sqpoll(io_setup.shared_sqpoll_fd())
            .use_direct_io(io_setup.use_direct_io && !use_page_cache)
            .use_registered_buffers(io_setup.use_registered_io_uring_buffers)
            .build(max_buf_size)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (max_buf_size, use_page_cache, io_setup);
        Ok(StorageFileBufReader::new())
    }
}

/// Lazy iterator yielding a file handle for each storage suitable for
/// archive-style reads matching `use_direct_io` (see [`OpenFileForArchive`]).
pub fn open_storage_files<'s>(
    storages: impl IntoIterator<Item = &'s AccountStorageEntry> + 's,
    use_direct_io: bool,
) -> impl Iterator<Item = io::Result<OpenFileForArchive<'s>>> + 's {
    storages
        .into_iter()
        .map(move |storage| storage.accounts.open_file_for_archive(use_direct_io))
}

/// Should tombstones be included or excluded when reading from storage?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstonesFilter {
    /// tombstones are included when reading from storage
    Include,
    /// tombstones are excluded when reading from storage
    Exclude,
}

/// A wrapper type around `AccountStorageEntry` that implements the `Read` trait.
/// This type skips over the data in accounts contained in the obsolete accounts
/// structure, and optionally over tombstone accounts as well.
///
/// The caller is responsible for activating the storage's file on `file_reader`
/// via `set_file` (typically using a file opened with [`open_storage_files`])
/// before constructing the reader.
pub struct AccountStorageReader<'r, R> {
    inner: AccountStorageReaderInner<'r, R>,
    num_alive_bytes: usize,
}

enum AccountStorageReaderInner<'r, R> {
    Raw {
        sorted_excluded_accounts: Vec<(Offset, usize)>,
        reader: &'r mut R,
        num_total_bytes: usize,
    },
    Bytes(Cursor<Vec<u8>>),
}

impl<'a, 'r, R: FileBufRead<'a>> AccountStorageReader<'r, R> {
    /// Creates a new `AccountStorageReader` from an `AccountStorageEntry`.
    /// The excluded accounts list is sorted during initialization.
    ///
    /// Expects that the caller has already attached the storage's file to
    /// `file_reader` via `set_file`.
    pub fn new(
        storage: &AccountStorageEntry,
        snapshot_slot: Option<Slot>,
        tombstones_filter: TombstonesFilter,
        file_reader: &'r mut R,
    ) -> io::Result<Self> {
        let num_total_bytes = storage.accounts.len();
        let mut sorted_excluded_accounts: Vec<_> = storage
            .obsolete_accounts_read_lock()
            .filter_obsolete_accounts(snapshot_slot)
            .collect();
        let mut excluded_offsets: HashSet<_> = sorted_excluded_accounts
            .iter()
            .map(|(offset, _data_len)| *offset)
            .collect();

        if tombstones_filter == TombstonesFilter::Exclude {
            excluded_offsets.extend(storage.tombstone_offsets_read_lock().iter().copied());
        }

        if !storage.accounts.can_archive_raw() {
            let bytes = append_vec_archive_bytes(storage, &excluded_offsets)?;
            let num_alive_bytes = bytes.len();
            return Ok(Self {
                inner: AccountStorageReaderInner::Bytes(Cursor::new(bytes)),
                num_alive_bytes,
            });
        }

        let mut num_alive_bytes = num_total_bytes - storage.get_obsolete_bytes(snapshot_slot);

        // Convert the length to the size
        sorted_excluded_accounts
            .iter_mut()
            .for_each(|(_offset, len)| {
                *len = storage.accounts.calculate_stored_size(*len);
            });

        if tombstones_filter == TombstonesFilter::Exclude {
            // Tombstones are zero-lamport accounts, which store no data, so every
            // tombstone record has the fixed stored size of a data-less account.
            let tombstone_stored_size = storage.accounts.calculate_stored_size(0);
            let tombstone_offsets = storage.tombstone_offsets_read_lock();
            num_alive_bytes -= tombstone_offsets.len() * tombstone_stored_size;
            sorted_excluded_accounts.extend(
                tombstone_offsets
                    .iter()
                    .map(|offset| (*offset, tombstone_stored_size)),
            );
        }

        sorted_excluded_accounts
            .sort_unstable_by(|(a_offset, _), (b_offset, _)| b_offset.cmp(a_offset));

        Ok(Self {
            inner: AccountStorageReaderInner::Raw {
                sorted_excluded_accounts,
                reader: file_reader,
                num_total_bytes,
            },
            num_alive_bytes,
        })
    }

    pub fn len(&self) -> usize {
        self.num_alive_bytes
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<'a, R: FileBufRead<'a>> Read for AccountStorageReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (sorted_excluded_accounts, reader, num_total_bytes) = match &mut self.inner {
            AccountStorageReaderInner::Raw {
                sorted_excluded_accounts,
                reader,
                num_total_bytes,
            } => (sorted_excluded_accounts, reader, num_total_bytes),
            AccountStorageReaderInner::Bytes(reader) => return reader.read(buf),
        };

        let mut total_read = 0;
        let buf_len = buf.len();

        while total_read < buf_len {
            let next_excluded_account = sorted_excluded_accounts.last();
            let file_offset = reader.get_file_offset() as usize;
            if let Some(&(excluded_start, excluded_size)) = next_excluded_account
                && file_offset == excluded_start
            {
                let skip_len = excluded_size.min(*num_total_bytes - excluded_start);
                reader.consume_or_skip(skip_len);
                sorted_excluded_accounts.pop();
                continue;
            }

            // Cannot read beyond the end of the buffer
            let bytes_left_in_buffer = buf_len.saturating_sub(total_read);

            // Cannot read beyond the next excluded account or the end of the file
            let bytes_to_read_from_file = if let Some((excluded_start, _)) = next_excluded_account {
                excluded_start.saturating_sub(file_offset)
            } else {
                num_total_bytes.saturating_sub(file_offset)
            };

            let bytes_to_read = bytes_left_in_buffer.min(bytes_to_read_from_file);

            let read_size = reader.read(&mut buf[total_read..][..bytes_to_read])?;

            if read_size == 0 {
                break; // EOF
            }

            total_read += read_size;
        }

        Ok(total_read)
    }
}

fn append_vec_archive_bytes(
    storage: &AccountStorageEntry,
    excluded_offsets: &HashSet<Offset>,
) -> io::Result<Vec<u8>> {
    let mut live_offsets = Vec::new();
    storage
        .accounts
        .scan_accounts_without_data(|offset, _account| {
            if !excluded_offsets.contains(&offset) {
                live_offsets.push(offset);
            }
        })
        .map_err(accounts_file_error_to_io)?;

    let mut bytes = Vec::new();
    for offset in live_offsets {
        storage
            .accounts
            .get_stored_account_callback(offset, |account| {
                append_legacy_account_bytes(&mut bytes, &account);
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing account while building snapshot archive bytes",
                )
            })?;
    }
    Ok(bytes)
}

fn append_legacy_account_bytes(bytes: &mut Vec<u8>, account: &StoredAccountInfo<'_>) {
    let start = bytes.len();
    let record_len = LEGACY_APPEND_VEC_ACCOUNT_OVERHEAD + account.data.len();
    bytes.resize(start + record_len, 0);
    let record = &mut bytes[start..start + record_len];

    record[0x00..0x08].copy_from_slice(&0u64.to_le_bytes());
    record[0x08..0x10].copy_from_slice(&(account.data.len() as u64).to_le_bytes());
    record[0x10..0x30].copy_from_slice(account.pubkey.as_ref());

    let account_meta = LEGACY_APPEND_VEC_STORED_META_SIZE;
    record[account_meta..account_meta + 8].copy_from_slice(&account.lamports.to_le_bytes());
    record[account_meta + 8..account_meta + 16].copy_from_slice(&account.rent_epoch.to_le_bytes());
    record[account_meta + 16..account_meta + 48].copy_from_slice(account.owner.as_ref());
    record[account_meta + 48] = account.executable.into();

    let data_start = LEGACY_APPEND_VEC_ACCOUNT_OVERHEAD;
    record[data_start..data_start + account.data.len()].copy_from_slice(account.data);
    bytes.resize(u64_align!(bytes.len()), 0);
}

fn accounts_file_error_to_io(err: crate::accounts_file::AccountsFileError) -> io::Error {
    io::Error::other(err.to_string())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            ObsoleteAccounts,
            account_storage_entry::AccountStorageEntry,
            accounts_db::get_temp_accounts_paths,
            accounts_file::{AccountsFile, AccountsFileProvider},
            append_vec::new_scan_accounts_reader,
            u64_align,
            utils::create_account_shared_data,
        },
        agave_fs::io_setup::IoSetupState,
        log::*,
        rand::{
            SeedableRng,
            rngs::StdRng,
            seq::{IndexedMutRandom as _, IndexedRandom},
        },
        solana_account::{AccountSharedData, ReadableAccount, accounts_equal},
        solana_pubkey::Pubkey,
        std::{fs::File, iter},
        test_case::test_case,
    };

    fn create_storage_for_storage_reader(
        slot: Slot,
        provider: AccountsFileProvider,
    ) -> (AccountStorageEntry, Vec<tempfile::TempDir>) {
        let id = 0;
        let (temp_dirs, paths) = get_temp_accounts_paths(1).unwrap();
        let file_size = 1024 * 1024;
        (
            AccountStorageEntry::new(&paths[0], slot, id, file_size, provider),
            temp_dirs,
        )
    }

    #[test_case(AccountsFileProvider::AppendVec)]
    fn test_account_storage_reader_no_obsolete_accounts(provider: AccountsFileProvider) {
        let (storage, _temp_dirs) = create_storage_for_storage_reader(0, provider);

        let account = AccountSharedData::new(1, 10, &Pubkey::default());
        let account2 = AccountSharedData::new(1, 10, &Pubkey::default());
        let slot = 0;

        let accounts = [
            (&Pubkey::new_unique(), &account),
            (&Pubkey::new_unique(), &account2),
        ];

        storage.accounts.write_accounts(&(slot, &accounts[..]));

        let files = open_storage_files(iter::once(&storage), false)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut buf_reader = storage_file_buf_reader(
            ACCOUNT_STORAGE_MAX_BUFFER_SIZE,
            false,
            &IoSetupState::default(),
        )
        .unwrap();
        buf_reader
            .set_file(files[0].as_ref(), storage.accounts.len() as u64)
            .unwrap();
        let reader =
            AccountStorageReader::new(&storage, None, TombstonesFilter::Include, &mut buf_reader)
                .unwrap();
        assert_eq!(reader.len(), storage.accounts.len());
    }

    #[test_case(0, 0, 0, TombstonesFilter::Include)]
    #[test_case(1, 0, 0, TombstonesFilter::Include)]
    #[test_case(1, 1, 0, TombstonesFilter::Include)]
    #[test_case(1, 1, 0, TombstonesFilter::Exclude)]
    #[test_case(1, 0, 1, TombstonesFilter::Include)]
    #[test_case(100, 0, 0, TombstonesFilter::Include)]
    #[test_case(100, 0, 10, TombstonesFilter::Include)]
    #[test_case(100, 0, 100, TombstonesFilter::Include)]
    #[test_case(100, 10, 0, TombstonesFilter::Include)]
    #[test_case(100, 10, 0, TombstonesFilter::Exclude)]
    #[test_case(100, 100, 0, TombstonesFilter::Include)]
    #[test_case(100, 100, 0, TombstonesFilter::Exclude)]
    #[test_case(100, 10, 10, TombstonesFilter::Include)]
    #[test_case(100, 10, 10, TombstonesFilter::Exclude)]
    fn test_account_storage_reader_with_excluded_accounts(
        total_accounts: usize,
        num_tombstones: usize,
        num_obsolete: usize,
        tombstones_filter: TombstonesFilter,
    ) {
        let (storage, _temp_dirs) =
            create_storage_for_storage_reader(0, AccountsFileProvider::AppendVec);

        let slot = 0;

        // Generate a seed from entropy and log the original seed
        let seed: u64 = rand::random();
        dbg!("Generated seed: {seed}");

        // Use a seedable RNG with the generated seed for reproducibility
        let mut rng = StdRng::seed_from_u64(seed);

        // Choose disjoint random index sets for the tombstone and obsolete accounts.
        // Tombstones must be chosen before writing because they are written as
        // zero-lamport, data-less accounts.
        let chosen_indexes = (0..total_accounts)
            .collect::<Vec<_>>()
            .choose_multiple(&mut rng, num_tombstones + num_obsolete)
            .cloned()
            .collect::<Vec<_>>();
        let (tombstone_indexes, obsolete_indexes) = chosen_indexes.split_at(num_tombstones);

        // Create a bunch of accounts and add them to the storage
        let accounts: Vec<_> = (0..total_accounts)
            .map(|index| {
                if tombstone_indexes.contains(&index) {
                    AccountSharedData::new(0, 0, &Pubkey::default())
                } else {
                    AccountSharedData::new(1, 10, &Pubkey::default())
                }
            })
            .collect();

        let accounts_to_append: Vec<_> = accounts
            .into_iter()
            .map(|account| (Pubkey::new_unique(), account))
            .collect();

        let offsets = storage
            .accounts
            .write_accounts(&(slot, &accounts_to_append[..]))
            .map(|stored_accounts_info| stored_accounts_info.offsets)
            .unwrap_or_default();

        let tombstone_offsets: Vec<_> = tombstone_indexes
            .iter()
            .map(|index| offsets[*index])
            .collect();
        let obsolete_offsets: Vec<_> = obsolete_indexes
            .iter()
            .map(|index| offsets[*index])
            .collect();

        storage.batch_insert_tombstone_offsets(tombstone_offsets);

        // Mark the obsolete accounts in storage
        let data_lens = storage.accounts.get_account_data_lens(&obsolete_offsets);
        storage
            .obsolete_accounts()
            .write()
            .unwrap()
            .mark_accounts_obsolete(obsolete_offsets.iter().copied().zip(data_lens), 0);

        let storage = storage.reopen_as_readonly().unwrap_or(storage);

        // Create the reader and check the length
        let files = open_storage_files(iter::once(&storage), false)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut file_reader = storage_file_buf_reader(
            ACCOUNT_STORAGE_MAX_BUFFER_SIZE,
            false,
            &IoSetupState::default(),
        )
        .unwrap();
        file_reader
            .set_file(files[0].as_ref(), storage.accounts.len() as u64)
            .unwrap();
        let mut reader =
            AccountStorageReader::new(&storage, None, tombstones_filter, &mut file_reader).unwrap();
        let mut number_of_accounts_to_remove = num_obsolete;
        let mut current_len = storage.accounts.len() - storage.get_obsolete_bytes(None);
        if tombstones_filter == TombstonesFilter::Exclude {
            number_of_accounts_to_remove += num_tombstones;
            current_len -= num_tombstones * storage.accounts.calculate_stored_size(0);
        }
        assert_eq!(reader.len(), current_len);

        // Create a temporary directory and a file within it
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file_path = temp_dir.path().join("output_file");
        let mut output_file = File::create(&temp_file_path).unwrap();

        let bytes_written = io::copy(&mut reader, &mut output_file).unwrap();
        assert_eq!(bytes_written as usize, reader.len());

        // Close the file
        drop(output_file);

        // If the number of accounts left is not zero, create a new AccountsFile from the output file
        // and verify that the number of accounts in the new file is correct
        if (total_accounts - number_of_accounts_to_remove) != 0 {
            let (accounts_file, num_accounts) =
                AccountsFile::new_from_file(temp_file_path, current_len).unwrap();

            // Verify that the correct number of accounts were found in the file
            assert_eq!(
                num_accounts,
                (total_accounts - number_of_accounts_to_remove)
            );

            // Create a new AccountStorageEntry from the output file
            let new_storage = AccountStorageEntry::new_existing(
                slot,
                0,
                accounts_file,
                ObsoleteAccounts::default(),
            );

            // Verify that the new storage has the same length as the reader
            assert_eq!(new_storage.accounts.len(), reader.len());
        }
    }

    #[test]
    fn test_account_storage_reader_split_archives_as_append_vec() {
        let slot = 0;
        let id = 0;
        let (temp_dirs, paths) = get_temp_accounts_paths(1).unwrap();
        let small_key = Pubkey::new_unique();
        let large_key = Pubkey::new_unique();
        let small_account = AccountSharedData::new(1, 16, &Pubkey::new_unique());
        let large_account = AccountSharedData::new(2, 8 * 1024, &Pubkey::new_unique());
        let accounts = [(&small_key, &small_account), (&large_key, &large_account)];

        let storage = AccountStorageEntry::new(
            &paths[0],
            slot,
            id,
            1024 * 1024,
            AccountsFileProvider::SplitStorage,
        );
        let stored_info = storage
            .accounts
            .write_accounts(&(slot, &accounts[..]))
            .unwrap();
        storage.add_accounts(stored_info.offsets.len(), stored_info.size);

        let obsolete_offset = stored_info.offsets[0];
        let obsolete_data_len = storage.accounts.get_account_data_lens(&[obsolete_offset])[0];
        storage
            .obsolete_accounts()
            .write()
            .unwrap()
            .mark_accounts_obsolete(iter::once((obsolete_offset, obsolete_data_len)), 0);

        let files = open_storage_files(iter::once(&storage), false)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut file_reader = storage_file_buf_reader(
            ACCOUNT_STORAGE_MAX_BUFFER_SIZE,
            false,
            &IoSetupState::default(),
        )
        .unwrap();
        file_reader
            .set_file(files[0].as_ref(), storage.accounts.len() as u64)
            .unwrap();
        let mut reader =
            AccountStorageReader::new(&storage, None, TombstonesFilter::Include, &mut file_reader)
                .unwrap();
        assert_eq!(
            reader.len(),
            u64_align!(LEGACY_APPEND_VEC_ACCOUNT_OVERHEAD + large_account.data().len())
        );

        let temp_dir = tempfile::tempdir().unwrap();
        let temp_file_path = temp_dir.path().join("output_file");
        let mut output_file = File::create(&temp_file_path).unwrap();
        let bytes_written = io::copy(&mut reader, &mut output_file).unwrap();
        assert_eq!(bytes_written as usize, reader.len());
        drop(output_file);

        let (accounts_file, num_accounts) =
            AccountsFile::new_from_file(temp_file_path, reader.len()).unwrap();
        assert_eq!(num_accounts, 1);

        let mut scan_reader = new_scan_accounts_reader();
        let mut loaded = Vec::new();
        accounts_file
            .scan_accounts(&mut scan_reader, |_offset, account| {
                loaded.push((*account.pubkey(), create_account_shared_data(&account)));
            })
            .unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, large_key);
        assert!(accounts_equal(&loaded[0].1, &large_account));

        drop(temp_dirs);
    }

    #[test]
    fn test_account_storage_reader_filter_by_slot() {
        let (storage, _temp_dirs) =
            create_storage_for_storage_reader(10, AccountsFileProvider::AppendVec);
        let total_accounts = 30;

        let slot = 0;

        // Create a bunch of accounts and add them to the storage
        let accounts: Vec<_> =
            iter::repeat_with(|| AccountSharedData::new(1, 10, &Pubkey::default()))
                .take(total_accounts)
                .collect();

        let accounts_to_append: Vec<_> = accounts
            .into_iter()
            .map(|account| (Pubkey::new_unique(), account))
            .collect();

        let offsets = storage
            .accounts
            .write_accounts(&(slot, &accounts_to_append[..]));

        // Generate a seed from entropy and log the original seed
        let seed: u64 = rand::random();
        info!("Generated seed: {seed}");

        // Use a seedable RNG with the generated seed for reproducibility
        let mut rng = StdRng::seed_from_u64(seed);

        let max_offset = offsets
            .as_ref()
            .and_then(|offsets| offsets.offsets.iter().max().cloned())
            .unwrap();

        let mut obsolete_account_offset = offsets
            .map(|offsets| {
                offsets
                    .offsets
                    .choose_multiple(&mut rng, total_accounts - 1)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Ensure that the last entry will be marked obsolete at some point
        if !obsolete_account_offset.contains(&max_offset) {
            // Replace a random obsolete account with the max offset
            if let Some(random_index) = obsolete_account_offset.choose_mut(&mut rng) {
                *random_index = max_offset;
            }
        }

        // Mark the obsolete accounts in storage at different slots
        let mut slot_marked_dead = 0;
        obsolete_account_offset.into_iter().for_each(|offset| {
            let mut size = storage.accounts.get_account_data_lens(&[offset]);
            storage
                .obsolete_accounts()
                .write()
                .unwrap()
                .mark_accounts_obsolete(
                    vec![(offset, size.pop().unwrap())].into_iter(),
                    slot_marked_dead,
                );
            slot_marked_dead += 1;
        });

        // Create a temporary directory
        let temp_dir = tempfile::tempdir().unwrap();

        // Now iterate through all the possible snapshot slots and verify correctness
        let files = open_storage_files(iter::once(&storage), false)
            .collect::<io::Result<Vec<_>>>()
            .unwrap();
        let mut file_reader = storage_file_buf_reader(
            ACCOUNT_STORAGE_MAX_BUFFER_SIZE,
            false,
            &IoSetupState::default(),
        )
        .unwrap();
        for snapshot_slot in 0..slot_marked_dead {
            file_reader
                .set_file(files[0].as_ref(), storage.accounts.len() as u64)
                .unwrap();
            let mut reader = AccountStorageReader::new(
                &storage,
                Some(snapshot_slot),
                TombstonesFilter::Include,
                &mut file_reader,
            )
            .unwrap();
            let current_len =
                storage.accounts.len() - storage.get_obsolete_bytes(Some(snapshot_slot));
            assert_eq!(reader.len(), current_len);

            // Create a file to write the reader's output. It will get deleted by AccountsFile::drop() every
            // iteration so it does not need a unique name
            let temp_file_path = temp_dir.path().join("output_file");
            let mut output_file = File::create(&temp_file_path).unwrap();

            let bytes_written = io::copy(&mut reader, &mut output_file).unwrap();
            assert_eq!(bytes_written as usize, reader.len());

            // Close the file
            drop(output_file);

            let (accounts_file, _num_accounts) =
                AccountsFile::new_from_file(temp_file_path, current_len).unwrap();

            // Create a new AccountStorageEntry from the output file
            let new_storage = AccountStorageEntry::new_existing(
                slot,
                0,
                accounts_file,
                ObsoleteAccounts::default(),
            );

            // Verify that the new storage has the same length as the reader
            assert_eq!(new_storage.accounts.len(), reader.len());
        }
    }
}
