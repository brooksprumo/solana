use {
    bytemuck::{Pod, Zeroable},
    clap::{
        crate_description, crate_name, value_t_or_exit, App, AppSettings, Arg, ArgMatches,
        SubCommand,
    },
    memmap2::Mmap,
    solana_accounts_db::{
        parse_cache_hash_data_filename, CacheHashDataFileEntry, CacheHashDataFileHeader,
    },
    std::{
        cmp::Ordering,
        collections::HashMap,
        fs::{self, File},
        io::{self, BufRead as _, BufReader, Read},
        iter,
        mem::size_of,
        num::Saturating,
        path::{Path, PathBuf},
        time::Instant,
    },
};

const CMD_INSPECT: &str = "inspect";
const CMD_DIFF: &str = "diff";
const CMD_DIFF_FILES: &str = "files";
const CMD_DIFF_DIRS: &str = "directories";

fn main() {
    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_version::version!())
        .global_setting(AppSettings::ArgRequiredElseHelp)
        .global_setting(AppSettings::ColoredHelp)
        .global_setting(AppSettings::InferSubcommands)
        .global_setting(AppSettings::UnifiedHelpMessage)
        .global_setting(AppSettings::VersionlessSubcommands)
        .subcommand(
            SubCommand::with_name(CMD_INSPECT)
                .about(
                    "Inspect an accounts hash cache file and display \
                     each account's address, hash, and balance",
                )
                .arg(
                    Arg::with_name("force")
                        .long("force")
                        .takes_value(false)
                        .help("Continue even if sanity checks fail"),
                )
                .arg(
                    Arg::with_name("path")
                        .index(1)
                        .takes_value(true)
                        .value_name("PATH")
                        .help("Accounts hash cache file to inspect"),
                ),
        )
        .subcommand(
            SubCommand::with_name(CMD_DIFF)
                .subcommand(
                    SubCommand::with_name(CMD_DIFF_FILES)
                        .about("Diff two accounts hash cache files")
                        .arg(
                            Arg::with_name("path1")
                                .index(1)
                                .takes_value(true)
                                .value_name("PATH1")
                                .help("Accounts hash cache file 1 to diff"),
                        )
                        .arg(
                            Arg::with_name("path2")
                                .index(2)
                                .takes_value(true)
                                .value_name("PATH2")
                                .help("Accounts hash cache file 2 to diff"),
                        ),
                )
                .subcommand(
                    SubCommand::with_name(CMD_DIFF_DIRS)
                        .about("Diff two accounts hash cache directories")
                        .arg(
                            Arg::with_name("path1")
                                .index(1)
                                .takes_value(true)
                                .value_name("PATH1")
                                .help("Accounts hash cache directory 1 to diff"),
                        )
                        .arg(
                            Arg::with_name("path2")
                                .index(2)
                                .takes_value(true)
                                .value_name("PATH2")
                                .help("Accounts hash cache directory 2 to diff"),
                        ),
                ),
        )
        .subcommand(
            SubCommand::with_name("brooks")
                .arg(
                    Arg::with_name("path1")
                        .index(1)
                        .takes_value(true)
                        .value_name("PATH1")
                        .help("failed index file"),
                )
                .arg(
                    Arg::with_name("path2")
                        .index(2)
                        .takes_value(true)
                        .value_name("PATH2")
                        .help("accounts hash cache dir"),
                ),
        )
        .get_matches();

    let subcommand = matches.subcommand();
    let subcommand_str = subcommand.0;
    match subcommand {
        (CMD_INSPECT, Some(subcommand_matches)) => cmd_inspect(&matches, subcommand_matches),
        (CMD_DIFF, Some(subcommand_matches)) => {
            let diff_subcommand = subcommand_matches.subcommand();
            match diff_subcommand {
                (CMD_DIFF_FILES, Some(diff_subcommand_matches)) => {
                    cmd_diff_files(&matches, diff_subcommand_matches)
                }
                (CMD_DIFF_DIRS, Some(diff_subcommand_matches)) => {
                    cmd_diff_dirs(&matches, diff_subcommand_matches)
                }
                _ => unreachable!(),
            }
        }
        ("brooks", Some(submatches)) => cmd_brooks(&matches, submatches),
        _ => unreachable!(),
    }
    .unwrap_or_else(|err| {
        eprintln!("Error: '{subcommand_str}' failed: {err}");
        std::process::exit(1);
    });
}

fn cmd_inspect(
    _app_matches: &ArgMatches<'_>,
    subcommand_matches: &ArgMatches<'_>,
) -> Result<(), String> {
    let force = subcommand_matches.is_present("force");
    let path = value_t_or_exit!(subcommand_matches, "path", String);
    do_inspect(path, force)
}

fn cmd_diff_files(
    _app_matches: &ArgMatches<'_>,
    subcommand_matches: &ArgMatches<'_>,
) -> Result<(), String> {
    let path1 = value_t_or_exit!(subcommand_matches, "path1", String);
    let path2 = value_t_or_exit!(subcommand_matches, "path2", String);
    do_diff_files(path1, path2)
}

fn cmd_diff_dirs(
    _app_matches: &ArgMatches<'_>,
    subcommand_matches: &ArgMatches<'_>,
) -> Result<(), String> {
    let path1 = value_t_or_exit!(subcommand_matches, "path1", String);
    let path2 = value_t_or_exit!(subcommand_matches, "path2", String);
    do_diff_dirs(path1, path2)
}

fn do_inspect(file: impl AsRef<Path>, force: bool) -> Result<(), String> {
    let (reader, header) = open_file(&file, force).map_err(|err| {
        format!(
            "failed to open accounts hash cache file '{}': {err}",
            file.as_ref().display(),
        )
    })?;
    let count_width = (header.count as f64).log10().ceil() as usize;

    let mut count = Saturating(0);
    scan_file(reader, header.count, |entry| {
        println!(
            "{count:count_width$}: pubkey: {:44}, hash: {:44}, lamports: {}",
            entry.pubkey.to_string(),
            entry.hash.0.to_string(),
            entry.lamports,
        );
        count += 1;
    })?;

    println!("actual entries: {count}, expected: {}", header.count);
    Ok(())
}

fn do_diff_files(file1: impl AsRef<Path>, file2: impl AsRef<Path>) -> Result<(), String> {
    let force = false; // skipping sanity checks is not supported when diffing
    let (mut reader1, header1) = open_file(&file1, force).map_err(|err| {
        format!(
            "failed to open accounts hash cache file 1 '{}': {err}",
            file1.as_ref().display(),
        )
    })?;
    let (mut reader2, header2) = open_file(&file2, force).map_err(|err| {
        format!(
            "failed to open accounts hash cache file 2 '{}': {err}",
            file2.as_ref().display(),
        )
    })?;
    // Note: Purposely open both files before reading either one.  This way, if there's an error
    // opening file 2, we can bail early without having to wait for file 1 to be read completely.

    // extract the entries from both files
    let do_extract = |reader: &mut BufReader<_>, header: &CacheHashDataFileHeader| {
        let mut entries = Vec::new();
        scan_file(reader, header.count, |entry| {
            entries.push(entry);
        })?;

        // entries in the file are sorted by pubkey then slot,
        // so we want to keep the *last* entry (if there are duplicates)
        let entries: HashMap<_, _> = entries
            .into_iter()
            .map(|entry| (entry.pubkey, (entry.hash, entry.lamports)))
            .collect();
        Ok::<_, String>(entries)
    };
    let entries1 = do_extract(&mut reader1, &header1)
        .map_err(|err| format!("failed to extract entries from file 1: {err}"))?;
    let entries2 = do_extract(&mut reader2, &header2)
        .map_err(|err| format!("failed to extract entries from file 2: {err}"))?;

    // compute the differences between the files
    let do_compute = |lhs: &HashMap<_, (_, _)>, rhs: &HashMap<_, (_, _)>| {
        let mut unique_entries = Vec::new();
        let mut mismatch_entries = Vec::new();
        for (lhs_key, lhs_value) in lhs.iter() {
            if let Some(rhs_value) = rhs.get(lhs_key) {
                if lhs_value != rhs_value {
                    mismatch_entries.push((
                        CacheHashDataFileEntry {
                            hash: lhs_value.0,
                            lamports: lhs_value.1,
                            pubkey: *lhs_key,
                        },
                        CacheHashDataFileEntry {
                            hash: rhs_value.0,
                            lamports: rhs_value.1,
                            pubkey: *lhs_key,
                        },
                    ));
                }
            } else {
                unique_entries.push(CacheHashDataFileEntry {
                    hash: lhs_value.0,
                    lamports: lhs_value.1,
                    pubkey: *lhs_key,
                });
            }
        }
        unique_entries.sort_unstable_by(|a, b| a.pubkey.cmp(&b.pubkey));
        mismatch_entries.sort_unstable_by(|a, b| a.0.pubkey.cmp(&b.0.pubkey));
        (unique_entries, mismatch_entries)
    };
    let (unique_entries1, mismatch_entries) = do_compute(&entries1, &entries2);
    let (unique_entries2, _) = do_compute(&entries2, &entries1);

    // display the unique entries in each file
    let do_print = |entries: &[CacheHashDataFileEntry]| {
        let count_width = (entries.len() as f64).log10().ceil() as usize;
        if entries.is_empty() {
            println!("(none)");
        } else {
            let mut total_lamports = Saturating(0);
            for (i, entry) in entries.iter().enumerate() {
                total_lamports += entry.lamports;
                println!(
                    "{i:count_width$}: pubkey: {:44}, hash: {:44}, lamports: {}",
                    entry.pubkey.to_string(),
                    entry.hash.0.to_string(),
                    entry.lamports,
                );
            }
            println!("total lamports: {}", total_lamports.0);
        }
    };
    println!("Unique entries in file 1:");
    do_print(&unique_entries1);
    println!("Unique entries in file 2:");
    do_print(&unique_entries2);

    println!("Mismatch values:");
    let count_width = (mismatch_entries.len() as f64).log10().ceil() as usize;
    if mismatch_entries.is_empty() {
        println!("(none)");
    } else {
        for (i, (lhs, rhs)) in mismatch_entries.iter().enumerate() {
            println!(
                "{i:count_width$}: pubkey: {:44}, hash: {:44}, lamports: {}",
                lhs.pubkey.to_string(),
                lhs.hash.0.to_string(),
                lhs.lamports,
            );
            println!(
                "{i:count_width$}: file 2: {:44}, hash: {:44}, lamports: {}",
                "(same)".to_string(),
                rhs.hash.0.to_string(),
                rhs.lamports,
            );
        }
    }

    Ok(())
}

fn do_diff_dirs(dir1: impl AsRef<Path>, dir2: impl AsRef<Path>) -> Result<(), String> {
    let get_files_in = |dir: &Path| {
        let mut files = Vec::new();
        let entries = fs::read_dir(dir)?;
        for entry in entries {
            let path = entry?.path();
            let meta = fs::metadata(&path)?;
            if meta.is_file() {
                let path = fs::canonicalize(path)?;
                files.push((path, meta));
            }
        }
        Ok::<_, io::Error>(files)
    };
    let parse_files = |files: &[(PathBuf, _)]| {
        files
            .iter()
            .map(|(file, _)| {
                Path::file_name(file)
                    .and_then(parse_cache_hash_data_filename)
                    .ok_or_else(|| format!("failed to parse '{}'", file.display()))
            })
            .collect::<Result<Vec<_>, String>>()
    };
    let parse_and_sort_files_in = |dir: &Path| {
        let files = get_files_in(dir)
            .map_err(|err| format!("failed to get files in '{}': {err}", dir.display()))?;
        let parsed_files = parse_files(&files)?;
        let mut combined: Vec<_> = iter::zip(files, parsed_files).collect();
        combined.sort_unstable_by(|a, b| {
            a.1.slot_range_start
                .cmp(&b.1.slot_range_start)
                .then_with(|| a.1.slot_range_end.cmp(&b.1.slot_range_end))
        });
        Ok::<_, String>(combined)
    };

    let _timer = ElapsedOnDrop {
        message: "diffing directories took ".to_string(),
        start: Instant::now(),
    };

    let files1 = parse_and_sort_files_in(dir1.as_ref())?;
    let files2 = parse_and_sort_files_in(dir2.as_ref())?;

    let mut uniques1 = Vec::new();
    let mut uniques2 = Vec::new();
    let mut mismatches = Vec::new();
    let mut idx1 = Saturating(0);
    let mut idx2 = Saturating(0);
    while idx1.0 < files1.len() && idx2.0 < files2.len() {
        let file1 = &files1[idx1.0];
        let file2 = &files2[idx2.0];
        match file1.1.slot_range_start.cmp(&file2.1.slot_range_start) {
            Ordering::Less => {
                // file1 is an older slot range than file2, so note it and advance idx1
                uniques1.push(file1);
                idx1 += 1;
            }
            Ordering::Greater => {
                // file1 is an newer slot range than file2, so note it and advance idx2
                uniques2.push(file2);
                idx2 += 1;
            }
            Ordering::Equal => {
                match file1.1.slot_range_end.cmp(&file2.1.slot_range_end) {
                    Ordering::Less => {
                        // file1 is a smaller slot range than file2, so note it and advance idx1
                        uniques1.push(file1);
                        idx1 += 1;
                    }
                    Ordering::Greater => {
                        // file1 is a larger slot range than file2, so note it and advance idx2
                        uniques2.push(file2);
                        idx2 += 1;
                    }
                    Ordering::Equal => {
                        // slot ranges match, so compare the files and advance both idx1 and idx2
                        let is_equal = || {
                            // if the files have different sizes, they are not equal
                            if file1.0 .1.len() != file2.0 .1.len() {
                                return false;
                            }

                            // if the parsed file names have different hashes, they are not equal
                            if file1.1.hash != file2.1.hash {
                                return false;
                            }

                            // if the file headers have different entry counts, they are not equal
                            let Ok((mmap1, header1)) = map_file(&file1.0 .0, false) else {
                                return false;
                            };
                            let Ok((mmap2, header2)) = map_file(&file2.0 .0, false) else {
                                return false;
                            };
                            if header1.count != header2.count {
                                return false;
                            }

                            // if the binary data of the files are different, they are not equal
                            let ahash_random_state = ahash::RandomState::new();
                            let hash1 = ahash_random_state.hash_one(mmap1.as_ref());
                            let hash2 = ahash_random_state.hash_one(mmap2.as_ref());
                            if hash1 != hash2 {
                                return false;
                            }

                            // ...otherwise they are equal
                            true
                        };
                        if !is_equal() {
                            mismatches.push((file1, file2));
                        }
                        idx1 += 1;
                        idx2 += 1;
                    }
                }
            }
        }
    }

    for file in files1.iter().skip(idx1.0) {
        uniques1.push(file);
    }
    for file in files2.iter().skip(idx2.0) {
        uniques2.push(file);
    }

    let do_print = |entries: &[&((PathBuf, _), _)]| {
        let count_width = (entries.len() as f64).log10().ceil() as usize;
        if entries.is_empty() {
            println!("(none)");
        } else {
            for (i, entry) in entries.iter().enumerate() {
                println!("{i:count_width$}: '{}'", entry.0 .0.display());
            }
        }
    };
    println!("Unique files in directory 1:");
    do_print(&uniques1);
    println!("Unique files in directory 2:");
    do_print(&uniques2);

    println!("Mismatch files:");
    let count_width = (mismatches.len() as f64).log10().ceil() as usize;
    if mismatches.is_empty() {
        println!("(none)");
    } else {
        for (i, (file1, file2)) in mismatches.iter().enumerate() {
            println!(
                "{i:count_width$}: '{}', '{}'",
                file1.0 .0.display(),
                file2.0 .0.display(),
            );
        }
    }

    Ok(())
}

/// Scan file with `reader` and apply `user_fn` to each entry
///
/// NOTE: `reader`'s cursor must already be at the first entry; i.e. *past* the header.
fn scan_file(
    mut reader: impl Read,
    num_entries_expected: usize,
    mut user_fn: impl FnMut(CacheHashDataFileEntry),
) -> Result<(), String> {
    let mut num_entries_actual = Saturating(0);
    let mut entry = CacheHashDataFileEntry::zeroed();
    loop {
        let result = reader.read_exact(bytemuck::bytes_of_mut(&mut entry));
        match result {
            Ok(()) => {}
            Err(err) => {
                if err.kind() == io::ErrorKind::UnexpectedEof
                    && num_entries_actual.0 == num_entries_expected
                {
                    // we've hit the expected end of the file
                    break;
                } else {
                    return Err(format!(
                        "failed to read file entry {num_entries_actual}, \
                         expected {num_entries_expected} entries: {err}",
                    ));
                }
            }
        };
        user_fn(entry);
        num_entries_actual += 1;
    }
    Ok(())
}

fn map_file(
    path: impl AsRef<Path>,
    force: bool,
) -> Result<(Mmap, CacheHashDataFileHeader), String> {
    let (reader, header) = open_file(&path, force)?;
    let file = reader.into_inner();
    let mmap = unsafe { Mmap::map(&file) }
        .map_err(|err| format!("failed to mmap '{}': {err}", path.as_ref().display()))?;
    Ok((mmap, header))
}

fn open_file(
    path: impl AsRef<Path>,
    force: bool,
) -> Result<(BufReader<File>, CacheHashDataFileHeader), String> {
    let file = File::open(path).map_err(|err| format!("{err}"))?;
    let actual_file_size = file
        .metadata()
        .map_err(|err| format!("failed to query file metadata: {err}"))?
        .len();
    let mut reader = BufReader::new(file);

    let header = {
        let mut header = CacheHashDataFileHeader::zeroed();
        reader
            .read_exact(bytemuck::bytes_of_mut(&mut header))
            .map_err(|err| format!("failed to read header: {err}"))?;
        header
    };

    // Sanity checks -- ensure the actual file size matches the expected file size
    let expected_file_size = size_of::<CacheHashDataFileHeader>()
        .saturating_add(size_of::<CacheHashDataFileEntry>().saturating_mul(header.count));
    if actual_file_size != expected_file_size as u64 {
        let err_msg = format!(
            "failed sanitization: actual file size does not match expected file size! \
             actual: {actual_file_size}, expected: {expected_file_size}",
        );
        if force {
            eprintln!("Warning: {err_msg}\nForced. Continuing... Results may be incorrect.");
        } else {
            return Err(err_msg);
        }
    }

    Ok((reader, header))
}

struct ElapsedOnDrop {
    message: String,
    start: Instant,
}

impl Drop for ElapsedOnDrop {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        println!("{}{elapsed:?}", self.message);
    }
}

fn cmd_brooks(
    _app_matches: &ArgMatches<'_>,
    subcommand_matches: &ArgMatches<'_>,
) -> Result<(), String> {
    let path1 = value_t_or_exit!(subcommand_matches, "path1", String);
    let path2 = value_t_or_exit!(subcommand_matches, "path2", String);

    let failed_index_file_path = Path::new(&path1);
    let accounts_hash_cache_dir_path = Path::new(&path2);

    // brooks TODO:
    // - go through the cache files in REVERSE and build up a map of pubkey => (hash, lamports, slot)
    //     - note: maybe have two maps? one for the newest entry, and one for the older/duplicates?
    //       this way we can see if something got shadowed or not.
    // - stream through the index file and compare with the cache entry
    // - the index will only have one entry per pubkey, so as we go through the index file, REMOVE
    //   the matching entry from the cache map
    // - keep track of any entries in the index MISSING from the cache map, and vice-versa
    // - obviously, track any entries where the hash or lamports is different

    let get_files_in = |dir: &Path| {
        let mut files = Vec::new();
        let entries = fs::read_dir(dir)?;
        for entry in entries {
            let path = entry?.path();
            let meta = fs::metadata(&path)?;
            if meta.is_file() {
                let path = fs::canonicalize(path)?;
                files.push((path, meta));
            }
        }
        Ok::<_, io::Error>(files)
    };
    let parse_files = |files: &[(PathBuf, _)]| {
        files
            .iter()
            .map(|(file, _)| {
                Path::file_name(file)
                    .and_then(parse_cache_hash_data_filename)
                    .ok_or_else(|| format!("failed to parse '{}'", file.display()))
            })
            .collect::<Result<Vec<_>, String>>()
    };
    let parse_and_sort_files_in = |dir: &Path| {
        let files = get_files_in(dir)
            .map_err(|err| format!("failed to get files in '{}': {err}", dir.display()))?;
        let parsed_files = parse_files(&files)?;
        let mut combined: Vec<_> = iter::zip(files, parsed_files).collect();
        combined.sort_unstable_by(|a, b| {
            a.1.slot_range_start
                .cmp(&b.1.slot_range_start)
                .then_with(|| a.1.slot_range_end.cmp(&b.1.slot_range_end))
        });
        Ok::<_, String>(combined)
    };

    eprintln!("brooks DEBUG: parsing and sorting cache files...");
    let cache_files = parse_and_sort_files_in(accounts_hash_cache_dir_path)?;

    // iterate oldest to newest
    // add entries to 'newest' map
    // if there was already an entry, it is older, so add it to the 'older' map
    // at the end, 'newest' will be correct, and the last element of each 'older' vec is the relative newest
    eprintln!("brooks DEBUG: scanning cache files...");
    let mut cache_newest = HashMap::<_, (_, _, _)>::default();
    let mut cache_older = HashMap::<_, Vec<_>>::default();
    for (i, cache_file) in cache_files.iter().enumerate() {
        eprintln!(
            "brooks DEBUG: scanning file {} of {}, '{}'...",
            i + 1,
            cache_files.len(),
            cache_file.0 .0.display(),
        );
        let (reader, header) = open_file(&cache_file.0 .0, false).map_err(|err| {
            format!(
                "failed to open accounts hash cache file '{}': {err}",
                cache_file.0 .0.display(),
            )
        })?;
        scan_file(reader, header.count, |entry| {
            let old_value = cache_newest.insert(
                entry.pubkey,
                (
                    entry.hash,
                    entry.lamports,
                    cache_file.1.slot_range_start..cache_file.1.slot_range_end,
                ),
            );
            if let Some(old_value) = old_value {
                cache_older.entry(entry.pubkey).or_default().push(old_value);
            }
        })?;
    }

    // stream through the index file and compare with the cache entries

    let file = File::open(failed_index_file_path).unwrap();
    let mut reader = BufReader::new(file);

    use solana_accounts_db::accounts_hash::AccountHash;
    use solana_program::{clock::Slot, hash::Hash, pubkey::Pubkey};
    use std::str::FromStr as _;

    #[repr(C)]
    #[derive(Debug, Copy, Clone, Pod, Zeroable)]
    struct FailedIndexFileEntry {
        pubkey: Pubkey,
        lamports: u64,
        hash: AccountHash,
        slot: Slot,
    }

    eprintln!("brooks DEBUG: scanning index file...");
    let mut mismatches = Vec::new();
    let mut missing_from_cache_newest = Vec::new();
    let mut found_in_cache_older = Vec::new();
    let mut count = Saturating(0);
    let mut line = String::with_capacity(1024); // big enough for one line, hopefully
    loop {
        line.truncate(0);
        let Ok(_num_bytes) = reader.read_line(&mut line) else {
            // we've (probably) hit the expected end of the file
            break;
        };

        let get_entry = || {
            let mut line_iter = line.split_whitespace();
            let pubkey_str = line_iter.next()?;
            let lamports_str = line_iter.next()?;
            let hash_str = line_iter.next()?;
            let slot_str = line_iter.next()?;

            Some(FailedIndexFileEntry {
                pubkey: Pubkey::from_str(pubkey_str).ok()?,
                lamports: u64::from_str(lamports_str).ok()?,
                hash: AccountHash(Hash::from_str(hash_str).ok()?),
                slot: Slot::from_str(slot_str).ok()?,
            })
        };
        let Some(entry) = get_entry() else {
            eprintln!("failed to parse line {count}: '{line}'! continuing...");
            continue;
        };

        // brooks TODO: no error, so do the comparisons
        // check if this pubkey is in the 'newest', and has the same values

        if let Some(cache_value) = cache_newest.remove(&entry.pubkey) {
            // this pubkey *was* found in the cache

            if cache_value.0 == entry.hash && cache_value.1 == entry.lamports {
                // match! nothing more to do
            } else {
                // mismatch! check if there's a match in 'oldest'??
                eprintln!("brooks DEBUG: mismatch! index: {entry:?}, cache: {cache_value:?}");
                let older_cache_values = cache_older.get(&entry.pubkey);
                if let Some(older_cache_values) = older_cache_values {
                    for older_cache_value in older_cache_values {
                        if older_cache_value.0 == entry.hash
                            && older_cache_value.1 == entry.lamports
                        {
                            // match in older cache values!
                            // this means the cache picked the wrong version of the account to use??
                            found_in_cache_older.push(entry);
                        } else {
                            // no match; this is what we "expect" (ish?)
                        }
                    }
                }
                mismatches.push((entry, cache_value));
            }
        } else {
            // not in the newest
            // assert not in oldest
            eprintln!("brooks DEBUG: missing! index: {entry:?}");
            assert!(!cache_older.contains_key(&entry.pubkey));
            missing_from_cache_newest.push(entry);
        }

        count += 1;
    }

    // at the end, print out what's left
    // - anything still in cache_newest *was not in the index*! weird!
    // - found in cache older: picked the wrong version of the account to use?? weird!
    // - missing from cache newest: cache didn't have the account at all?? bad!
    // - mismatches: most likely? also bad!

    println!("Done! Total index entries: {count}");

    println!("Entries found in cache_older: {}", cache_older.len());
    for (i, older_cache_entries) in cache_older.iter().enumerate() {
        println!(
            "{i}: len {}, {older_cache_entries:?}",
            older_cache_entries.1.len(),
        );
    }

    println!(
        "Entries still remaining in cache_newest: {}",
        cache_newest.len(),
    );
    for (i, cache_entry) in cache_newest.iter().enumerate() {
        println!("{i}: {cache_entry:?}");
    }

    println!(
        "Entries missing from cache_newest: {}",
        missing_from_cache_newest.len(),
    );
    for (i, missing_entry) in missing_from_cache_newest.into_iter().enumerate() {
        println!("{i}: {missing_entry:?}")
    }

    println!("Mismatched entries: {}", mismatches.len());
    for (i, mismatched_entry) in mismatches.into_iter().enumerate() {
        println!(
            "{i}: index entry: {:?}, cache entry: {:?}",
            mismatched_entry.0, mismatched_entry.1,
        );
    }

    Ok(())
}
