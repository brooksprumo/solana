use {
    super::utils::{align_usize, read_array, read_exact_at, usize_to_file_size},
    crate::{account_info::Offset, storable_accounts::AccountForStorage},
    agave_fs::file_io::write_buffer_to_file,
    solana_account::ReadableAccount,
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    std::{
        convert::TryFrom,
        fs::File,
        io::{self, Write},
    },
};

pub const META_HEADER_SIZE: usize = 4 * 1024;
const META_FORMAT_VERSION: u16 = 1;
const META_MAGIC: &[u8; 8] = b"AGMETA0\0";
const META_HEADER_ALIGNMENT_LOG2: u8 = 3;

const META_HEADER_MAGIC_OFFSET: usize = 0;
const META_HEADER_VERSION_OFFSET: usize = 8;
const META_HEADER_HEADER_LEN_OFFSET: usize = 10;
const META_HEADER_ALIGNMENT_LOG2_OFFSET: usize = 12;
const META_HEADER_SLOT_OFFSET: usize = 16;
const META_HEADER_ID_OFFSET: usize = 24;
const META_HEADER_USED_LEN_OFFSET: usize = 32;

pub const META_ALIGNMENT: usize = 8;
pub const MAX_META_ENTRY_SIZE: usize = 4 * 1024;

pub const META_ADDRESS_OFFSET: usize = 0;
pub const META_OWNER_OFFSET: usize = 32;
pub const META_LAMPORTS_OFFSET: usize = 64;
pub const META_RENT_EPOCH_OFFSET: usize = 72;
pub const META_EXECUTABLE_OFFSET: usize = 80;
pub const META_DATA_REF_KIND_OFFSET: usize = 81;
pub const META_DATA_LEN_OFFSET: usize = 88;
pub const META_DATA_OFFSET_OFFSET: usize = 96;
pub const META_ENTRY_FIXED_SIZE: usize = 104;

pub const DATA_REF_NONE: u8 = 0;
pub const DATA_REF_INTERNAL: u8 = 1;
pub const DATA_REF_EXTERNAL: u8 = 2;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct DataLen(pub u64);

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct ExternalDataOffset(pub u64);

#[derive(Debug, Copy, Clone)]
pub enum DataRef {
    NoData,
    Inline {
        len: DataLen,
    },
    External {
        len: DataLen,
        offset: ExternalDataOffset,
    },
}

#[derive(Debug)]
pub struct SplitStoredAccount {
    pub offset: Offset,
    pub pubkey: Pubkey,
    pub owner: Pubkey,
    pub lamports: u64,
    pub rent_epoch: Epoch,
    pub executable: bool,
    pub data_ref: DataRef,
    pub data_len: usize,
    pub stored_size: usize,
}

pub fn write_meta_header(file: &mut File, slot: Slot, id: u32, used_len: usize) -> io::Result<()> {
    let mut header = [0u8; META_HEADER_SIZE];
    header[META_HEADER_MAGIC_OFFSET..META_HEADER_MAGIC_OFFSET + 8].copy_from_slice(META_MAGIC);
    header[META_HEADER_VERSION_OFFSET..META_HEADER_VERSION_OFFSET + 2]
        .copy_from_slice(&META_FORMAT_VERSION.to_le_bytes());
    header[META_HEADER_HEADER_LEN_OFFSET..META_HEADER_HEADER_LEN_OFFSET + 2]
        .copy_from_slice(&u16::try_from(META_HEADER_SIZE).unwrap().to_le_bytes());
    header[META_HEADER_ALIGNMENT_LOG2_OFFSET] = META_HEADER_ALIGNMENT_LOG2;
    header[META_HEADER_SLOT_OFFSET..META_HEADER_SLOT_OFFSET + 8]
        .copy_from_slice(&slot.to_le_bytes());
    header[META_HEADER_ID_OFFSET..META_HEADER_ID_OFFSET + 4].copy_from_slice(&id.to_le_bytes());
    header[META_HEADER_USED_LEN_OFFSET..META_HEADER_USED_LEN_OFFSET + 8].copy_from_slice(
        &u64::try_from(used_len)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "split meta used length too large",
                )
            })?
            .to_le_bytes(),
    );
    file.write_all(&header)
}

pub fn write_meta_header_used_len(file: &File, used_len: usize) -> io::Result<()> {
    let used_len = u64::try_from(used_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "split meta used length too large",
        )
    })?;
    write_buffer_to_file(
        file,
        &used_len.to_le_bytes(),
        usize_to_file_size(META_HEADER_USED_LEN_OFFSET)?,
    )
}

#[allow(dead_code)]
pub fn read_meta_header(file: &File) -> io::Result<usize> {
    let physical_len = usize::try_from(file.metadata()?.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "split meta physical file length too large",
        )
    })?;
    let mut header = [0u8; META_HEADER_SIZE];
    read_exact_at(file, physical_len, 0, &mut header)?;
    if &header[META_HEADER_MAGIC_OFFSET..META_HEADER_MAGIC_OFFSET + 8] != META_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "split meta file magic mismatch",
        ));
    }
    let version = u16::from_le_bytes(read_array(&header, META_HEADER_VERSION_OFFSET).unwrap());
    let header_len =
        u16::from_le_bytes(read_array(&header, META_HEADER_HEADER_LEN_OFFSET).unwrap());
    let alignment_log2 = header[META_HEADER_ALIGNMENT_LOG2_OFFSET];
    if version != META_FORMAT_VERSION
        || usize::from(header_len) != META_HEADER_SIZE
        || alignment_log2 != META_HEADER_ALIGNMENT_LOG2
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported split meta file header",
        ));
    }
    let used_len = u64::from_le_bytes(read_array(&header, META_HEADER_USED_LEN_OFFSET).unwrap());
    let used_len = usize::try_from(used_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "split meta used length too large",
        )
    })?;
    if used_len < META_HEADER_SIZE || used_len > physical_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid split meta used length",
        ));
    }
    Ok(used_len)
}

impl DataRef {
    pub fn len(&self) -> Option<usize> {
        match self {
            DataRef::NoData => Some(0),
            DataRef::Inline { len } | DataRef::External { len, .. } => usize::try_from(len.0).ok(),
        }
    }
}

pub fn calculate_meta_stored_size(data_len: usize) -> usize {
    if should_store_internal(data_len) {
        align_usize(
            META_ENTRY_FIXED_SIZE.saturating_add(data_len),
            META_ALIGNMENT,
        )
    } else {
        align_usize(META_ENTRY_FIXED_SIZE, META_ALIGNMENT)
    }
}

pub fn should_store_internal(data_len: usize) -> bool {
    data_len == 0
        || (u64::try_from(data_len).is_ok()
            && align_usize(
                META_ENTRY_FIXED_SIZE.saturating_add(data_len),
                META_ALIGNMENT,
            ) <= MAX_META_ENTRY_SIZE)
}

pub fn read_account_meta(
    meta_file: &File,
    meta_len: usize,
    offset: usize,
) -> Option<SplitStoredAccount> {
    if offset.checked_add(META_ENTRY_FIXED_SIZE)? > meta_len {
        return None;
    }

    let mut fixed = [0u8; META_ENTRY_FIXED_SIZE];
    read_exact_at(meta_file, meta_len, offset, &mut fixed).ok()?;

    let pubkey = Pubkey::new_from_array(read_array(&fixed, META_ADDRESS_OFFSET)?);
    let owner = Pubkey::new_from_array(read_array(&fixed, META_OWNER_OFFSET)?);
    let lamports = u64::from_le_bytes(read_array(&fixed, META_LAMPORTS_OFFSET)?);
    let rent_epoch = u64::from_le_bytes(read_array(&fixed, META_RENT_EPOCH_OFFSET)?);
    let executable = match *fixed.get(META_EXECUTABLE_OFFSET)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let data_ref = match *fixed.get(META_DATA_REF_KIND_OFFSET)? {
        DATA_REF_NONE => {
            let data_len = DataLen(u64::from_le_bytes(read_array(
                &fixed,
                META_DATA_LEN_OFFSET,
            )?));
            if data_len != DataLen(0) {
                return None;
            }
            DataRef::NoData
        }
        DATA_REF_INTERNAL => DataRef::Inline {
            len: DataLen(u64::from_le_bytes(read_array(
                &fixed,
                META_DATA_LEN_OFFSET,
            )?)),
        },
        DATA_REF_EXTERNAL => DataRef::External {
            len: DataLen(u64::from_le_bytes(read_array(
                &fixed,
                META_DATA_LEN_OFFSET,
            )?)),
            offset: ExternalDataOffset(u64::from_le_bytes(read_array(
                &fixed,
                META_DATA_OFFSET_OFFSET,
            )?)),
        },
        _ => return None,
    };
    let data_len = data_ref.len()?;
    let stored_size = calculate_meta_stored_size(data_len);
    if stored_size > MAX_META_ENTRY_SIZE || offset.checked_add(stored_size)? > meta_len {
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
        data_len,
        stored_size,
    })
}

pub fn write_meta_entry(
    file: &File,
    offset: usize,
    account: &impl ReadableAccountPubkey,
    data_ref: DataRef,
) -> io::Result<()> {
    debug_assert_eq!(offset % META_ALIGNMENT, 0);
    let data_len = data_ref.len().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "account data length exceeds split meta entry limit",
        )
    })?;
    if data_len != account.data().len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "split meta data ref length does not match account data length",
        ));
    }
    let mut fixed = [0u8; META_ENTRY_FIXED_SIZE];
    fixed[META_ADDRESS_OFFSET..META_ADDRESS_OFFSET + 32].copy_from_slice(account.pubkey().as_ref());
    fixed[META_OWNER_OFFSET..META_OWNER_OFFSET + 32].copy_from_slice(account.owner().as_ref());
    fixed[META_LAMPORTS_OFFSET..META_LAMPORTS_OFFSET + 8]
        .copy_from_slice(&account.lamports().to_le_bytes());
    fixed[META_RENT_EPOCH_OFFSET..META_RENT_EPOCH_OFFSET + 8]
        .copy_from_slice(&account.rent_epoch().to_le_bytes());
    fixed[META_EXECUTABLE_OFFSET] = account.executable().into();

    match data_ref {
        DataRef::NoData => fixed[META_DATA_REF_KIND_OFFSET] = DATA_REF_NONE,
        DataRef::Inline { len } => {
            fixed[META_DATA_REF_KIND_OFFSET] = DATA_REF_INTERNAL;
            fixed[META_DATA_LEN_OFFSET..META_DATA_LEN_OFFSET + 8]
                .copy_from_slice(&len.0.to_le_bytes());
        }
        DataRef::External { len, offset } => {
            fixed[META_DATA_REF_KIND_OFFSET] = DATA_REF_EXTERNAL;
            fixed[META_DATA_LEN_OFFSET..META_DATA_LEN_OFFSET + 8]
                .copy_from_slice(&len.0.to_le_bytes());
            fixed[META_DATA_OFFSET_OFFSET..META_DATA_OFFSET_OFFSET + 8]
                .copy_from_slice(&offset.0.to_le_bytes());
        }
    }

    write_buffer_to_file(file, &fixed, usize_to_file_size(offset)?)?;
    if matches!(data_ref, DataRef::Inline { .. }) {
        write_buffer_to_file(
            file,
            account.data(),
            usize_to_file_size(offset + META_ENTRY_FIXED_SIZE)?,
        )?;
    }
    Ok(())
}

pub trait ReadableAccountPubkey: ReadableAccount {
    fn pubkey(&self) -> &Pubkey;
}

impl ReadableAccountPubkey for AccountForStorage<'_> {
    fn pubkey(&self) -> &Pubkey {
        self.pubkey()
    }
}
