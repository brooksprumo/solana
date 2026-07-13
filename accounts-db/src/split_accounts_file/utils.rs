use {
    agave_fs::{FileSize, file_io::read_into_buffer},
    solana_clock::Slot,
    std::{
        convert::TryFrom,
        fs::{File, OpenOptions},
        io,
        path::{Path, PathBuf},
    },
};

pub fn create_split_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

pub fn read_exact_at(
    file: &File,
    valid_file_len: usize,
    start_offset: usize,
    buffer: &mut [u8],
) -> io::Result<()> {
    let bytes_read = read_into_buffer(
        file,
        usize_to_file_size(valid_file_len)?,
        usize_to_file_size(start_offset)?,
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

pub fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    bytes.get(offset..offset + N)?.try_into().ok()
}

pub fn usize_to_file_size(value: usize) -> io::Result<FileSize> {
    FileSize::try_from(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "split file offset too large"))
}

pub fn align_usize(value: usize, alignment: usize) -> usize {
    value.saturating_add(alignment - 1) & !(alignment - 1)
}

pub fn meta_path_for_base(base_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta", base_path.display()))
}

pub fn data_path_for_base(base_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.data", base_path.display()))
}

pub fn parse_slot_and_id(base_path: &Path) -> Option<(Slot, u32)> {
    let name = base_path.file_name()?.to_str()?;
    let mut parts = name.split('.');
    let slot = parts.next()?.parse().ok()?;
    let id = parts.next()?.parse().ok()?;
    Some((slot, id))
}
