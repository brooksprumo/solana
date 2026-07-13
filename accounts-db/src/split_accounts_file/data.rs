use {
    super::{
        meta::{DataLen, DataRef, SplitStoredAccount},
        utils::{align_usize, read_array, read_exact_at, usize_to_file_size},
    },
    agave_fs::file_io::write_buffer_to_file,
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{
        convert::TryFrom,
        fs::File,
        io::{self, Write},
    },
};

pub const DATA_HEADER_SIZE: usize = 4 * 1024;
const DATA_FORMAT_VERSION: u16 = 1;
const DATA_MAGIC: &[u8; 8] = b"AGDATA0\0";
const DATA_HEADER_ALIGNMENT_LOG2: u8 = 12;

const DATA_HEADER_MAGIC_OFFSET: usize = 0;
const DATA_HEADER_VERSION_OFFSET: usize = 8;
const DATA_HEADER_HEADER_LEN_OFFSET: usize = 10;
const DATA_HEADER_ALIGNMENT_LOG2_OFFSET: usize = 12;
const DATA_HEADER_SLOT_OFFSET: usize = 16;
const DATA_HEADER_ID_OFFSET: usize = 24;
const DATA_HEADER_USED_LEN_OFFSET: usize = 32;

pub const DATA_ALIGNMENT: usize = 4 * 1024;

pub const DATA_ADDRESS_OFFSET: usize = 0;
pub const DATA_LEN_OFFSET: usize = 32;
pub const DATA_ENTRY_FIXED_SIZE: usize = 40;

pub fn write_data_header(file: &mut File, slot: Slot, id: u32, used_len: usize) -> io::Result<()> {
    let mut header = [0u8; DATA_HEADER_SIZE];
    header[DATA_HEADER_MAGIC_OFFSET..DATA_HEADER_MAGIC_OFFSET + 8].copy_from_slice(DATA_MAGIC);
    header[DATA_HEADER_VERSION_OFFSET..DATA_HEADER_VERSION_OFFSET + 2]
        .copy_from_slice(&DATA_FORMAT_VERSION.to_le_bytes());
    header[DATA_HEADER_HEADER_LEN_OFFSET..DATA_HEADER_HEADER_LEN_OFFSET + 2]
        .copy_from_slice(&u16::try_from(DATA_HEADER_SIZE).unwrap().to_le_bytes());
    header[DATA_HEADER_ALIGNMENT_LOG2_OFFSET] = DATA_HEADER_ALIGNMENT_LOG2;
    header[DATA_HEADER_SLOT_OFFSET..DATA_HEADER_SLOT_OFFSET + 8]
        .copy_from_slice(&slot.to_le_bytes());
    header[DATA_HEADER_ID_OFFSET..DATA_HEADER_ID_OFFSET + 4].copy_from_slice(&id.to_le_bytes());
    header[DATA_HEADER_USED_LEN_OFFSET..DATA_HEADER_USED_LEN_OFFSET + 8].copy_from_slice(
        &u64::try_from(used_len)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "split data used length too large",
                )
            })?
            .to_le_bytes(),
    );
    file.write_all(&header)
}

pub fn write_data_header_used_len(file: &File, used_len: usize) -> io::Result<()> {
    let used_len = u64::try_from(used_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "split data used length too large",
        )
    })?;
    write_buffer_to_file(
        file,
        &used_len.to_le_bytes(),
        usize_to_file_size(DATA_HEADER_USED_LEN_OFFSET)?,
    )
}

#[allow(dead_code)]
pub fn read_data_header(file: &File) -> io::Result<usize> {
    let physical_len = usize::try_from(file.metadata()?.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "split data physical file length too large",
        )
    })?;
    let mut header = [0u8; DATA_HEADER_SIZE];
    read_exact_at(file, physical_len, 0, &mut header)?;
    if &header[DATA_HEADER_MAGIC_OFFSET..DATA_HEADER_MAGIC_OFFSET + 8] != DATA_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "split data file magic mismatch",
        ));
    }
    let version = u16::from_le_bytes(read_array(&header, DATA_HEADER_VERSION_OFFSET).unwrap());
    let header_len =
        u16::from_le_bytes(read_array(&header, DATA_HEADER_HEADER_LEN_OFFSET).unwrap());
    let alignment_log2 = header[DATA_HEADER_ALIGNMENT_LOG2_OFFSET];
    if version != DATA_FORMAT_VERSION
        || usize::from(header_len) != DATA_HEADER_SIZE
        || alignment_log2 != DATA_HEADER_ALIGNMENT_LOG2
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported split data file header",
        ));
    }
    let used_len = u64::from_le_bytes(read_array(&header, DATA_HEADER_USED_LEN_OFFSET).unwrap());
    let used_len = usize::try_from(used_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "split data used length too large",
        )
    })?;
    if used_len < DATA_HEADER_SIZE || used_len > physical_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid split data used length",
        ));
    }
    Ok(used_len)
}

pub fn calculate_data_stored_size(data_len: usize) -> usize {
    align_usize(
        DATA_ENTRY_FIXED_SIZE.saturating_add(data_len),
        DATA_ALIGNMENT,
    )
}

pub fn write_data_entry(
    file: &File,
    offset: usize,
    pubkey: &Pubkey,
    data: &[u8],
) -> io::Result<()> {
    debug_assert_eq!(offset % DATA_ALIGNMENT, 0);
    let len =
        DataLen(u64::try_from(data.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "split data length too large")
        })?);
    let mut fixed = [0u8; DATA_ENTRY_FIXED_SIZE];
    fixed[DATA_ADDRESS_OFFSET..DATA_ADDRESS_OFFSET + 32].copy_from_slice(pubkey.as_ref());
    fixed[DATA_LEN_OFFSET..DATA_LEN_OFFSET + 8].copy_from_slice(&len.0.to_le_bytes());
    write_buffer_to_file(file, &fixed, usize_to_file_size(offset)?)?;
    write_buffer_to_file(
        file,
        data,
        usize_to_file_size(offset + DATA_ENTRY_FIXED_SIZE)?,
    )
}

pub fn read_account_data(
    meta_file: &File,
    meta_len: usize,
    data_file: Option<&File>,
    data_len: usize,
    account: &SplitStoredAccount,
) -> io::Result<Vec<u8>> {
    match account.data_ref {
        DataRef::NoData => Ok(Vec::new()),
        DataRef::Inline { len } => {
            let len = usize::try_from(len.0).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "split data length too large")
            })?;
            let mut data = vec![0; len];
            read_exact_at(
                meta_file,
                meta_len,
                account.offset + super::meta::META_ENTRY_FIXED_SIZE,
                &mut data,
            )?;
            Ok(data)
        }
        DataRef::External { len, offset } => {
            let data_file = data_file.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "split data file missing")
            })?;
            read_data_entry_from_file(
                data_file,
                data_len,
                usize::try_from(offset.0).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "split data offset too large")
                })?,
                &account.pubkey,
                len,
            )
        }
    }
}

fn read_data_entry_from_file(
    file: &File,
    file_len: usize,
    offset: usize,
    expected_pubkey: &Pubkey,
    expected_len: DataLen,
) -> io::Result<Vec<u8>> {
    let expected_len_usize = usize::try_from(expected_len.0)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "split data length too large"))?;
    let stored_size = calculate_data_stored_size(expected_len_usize);
    if offset
        .checked_add(stored_size)
        .is_none_or(|end| end > file_len)
    {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "split data entry exceeds data file length",
        ));
    }

    let mut fixed = [0u8; DATA_ENTRY_FIXED_SIZE];
    read_exact_at(file, file_len, offset, &mut fixed)?;
    let pubkey = Pubkey::new_from_array(read_array(&fixed, DATA_ADDRESS_OFFSET).unwrap());
    let len = DataLen(u64::from_le_bytes(
        read_array(&fixed, DATA_LEN_OFFSET).unwrap(),
    ));
    if &pubkey != expected_pubkey || len != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "split data entry metadata mismatch",
        ));
    }

    let mut data = vec![0; expected_len_usize];
    read_exact_at(file, file_len, offset + DATA_ENTRY_FIXED_SIZE, &mut data)?;
    Ok(data)
}
