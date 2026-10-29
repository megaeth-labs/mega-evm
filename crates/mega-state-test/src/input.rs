//! Bounds on fixture discovery, input bytes and the state materialized by a test.

use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use walkdir::WalkDir;

/// The largest state-test fixture file the runner reads: 64 MiB.
const MAX_FIXTURE_BYTES: u64 = 64 * 1024 * 1024;
/// The largest blockchain-test fixture file: 256 MiB, accommodating bundled block sequences.
const MAX_BLOCKCHAIN_FIXTURE_BYTES: u64 = 256 * 1024 * 1024;
/// The largest collection of JSON files one discovery accepts, across all input paths.
const MAX_FIXTURE_FILES: usize = 100_000;
/// The deepest entry discovery accepts, relative to each input directory.
const MAX_FIXTURE_DEPTH: usize = 64;
/// The largest state a fixture may materialize, in accounts.
const MAX_STATE_ACCOUNTS: usize = 100_000;
/// The largest state a fixture may materialize, in storage slots across all accounts.
const MAX_STATE_SLOTS: usize = 1_000_000;

/// Reads a UTF-8 fixture, refusing an oversized file before allocating its contents.
pub(crate) fn read_fixture(path: &Path) -> io::Result<String> {
    read_with_limit(File::open(path)?, MAX_FIXTURE_BYTES)
}

/// Reads a blockchain fixture under its larger bound, using the same guarded reader.
pub(crate) fn read_blockchain_fixture(path: &Path) -> io::Result<String> {
    read_with_limit(File::open(path)?, MAX_BLOCKCHAIN_FIXTURE_BYTES)
}

fn read_with_limit(file: File, limit: u64) -> io::Result<String> {
    if file.metadata()?.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("fixture exceeds {limit} bytes"),
        ));
    }
    read_bounded(file, limit)
}

fn read_bounded(reader: impl Read, limit: u64) -> io::Result<String> {
    // The same opened file may grow after metadata was read. Never read it past this sentinel.
    let mut json = String::new();
    reader.take(limit + 1).read_to_string(&mut json)?;
    if json.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("fixture exceeds {limit} bytes"),
        ));
    }
    Ok(json)
}

/// Every JSON file under `path`, or `path` itself when it is a file, in path order.
/// Walking errors and exceeded limits fail discovery rather than dropping any input.
pub fn find_json_files(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    discover(path, &mut files, MAX_FIXTURE_DEPTH, MAX_FIXTURE_FILES)?;
    files.sort();
    Ok(files)
}

/// Discovers all `paths` under one file-count budget; an empty input directory is an error.
pub fn find_json_files_in_paths(paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        let before = files.len();
        discover(path, &mut files, MAX_FIXTURE_DEPTH, MAX_FIXTURE_FILES)?;
        if files.len() == before {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no JSON fixtures under {}", path.display()),
            ));
        }
    }
    files.sort();
    Ok(files)
}

fn discover(path: &Path, files: &mut Vec<PathBuf>, depth: usize, limit: usize) -> io::Result<()> {
    let metadata = path
        .metadata()
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    if metadata.is_file() {
        return add_file(path.to_path_buf(), files, limit);
    }
    for entry in WalkDir::new(path) {
        let entry = entry?;
        // WalkDir::max_depth would silently omit descendants, so inspect the first excess entry.
        if entry.depth() > depth {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("fixture directory depth exceeds {depth}: {}", entry.path().display()),
            ));
        }
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            add_file(entry.into_path(), files, limit)?;
        }
    }
    Ok(())
}

fn add_file(path: PathBuf, files: &mut Vec<PathBuf>, limit: usize) -> io::Result<()> {
    if files.len() >= limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fixture file count exceeds {limit}: {}", path.display()),
        ));
    }
    files.push(path);
    Ok(())
}

/// Checks account and slot cardinality before copying fixture state or building its tries.
pub(crate) fn check_state_size(slot_counts: impl IntoIterator<Item = usize>) -> Result<(), String> {
    let mut total_slots = 0usize;
    for (index, slots) in slot_counts.into_iter().enumerate() {
        if index >= MAX_STATE_ACCOUNTS {
            return Err(format!("state account count exceeds {MAX_STATE_ACCOUNTS}"));
        }
        total_slots = total_slots
            .checked_add(slots)
            .filter(|total| *total <= MAX_STATE_SLOTS)
            .ok_or_else(|| format!("state storage exceeds {MAX_STATE_SLOTS} total slots"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, Write};

    use super::*;

    #[test]
    fn test_discovery_fails_instead_of_truncating_at_depth_limit() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("a");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("one.json"), "{}").unwrap();
        assert!(discover(root.path(), &mut Vec::new(), 2, 1).is_ok());
        std::fs::create_dir_all(nested.join("b").join("c")).unwrap();
        let error = discover(root.path(), &mut Vec::new(), 2, 1).unwrap_err();
        assert!(error.to_string().contains("depth exceeds 2"));
    }

    #[test]
    fn test_discovery_file_budget_is_shared_between_roots() {
        let root = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for name in ["one.json", "two.json"] {
            let path = root.path().join(name);
            std::fs::write(&path, "{}").unwrap();
            discover(&path, &mut files, 1, 2).unwrap();
        }
        let error = discover(&root.path().join("one.json"), &mut files, 1, 2).unwrap_err();
        assert!(error.to_string().contains("file count exceeds 2"));
    }

    #[test]
    fn test_discovery_propagates_missing_root() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("absent");
        let error = find_json_files(&missing).unwrap_err();
        assert!(error.to_string().contains(&missing.display().to_string()));
        assert!(find_json_files_in_paths(&[root.path().to_path_buf()]).is_err());
    }

    #[test]
    fn test_read_limit_accepts_exact_size_and_rejects_one_more_byte() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"{}\n").unwrap();
        file.rewind().unwrap();
        assert_eq!(read_with_limit(file.try_clone().unwrap(), 3).unwrap(), "{}\n");
        file.rewind().unwrap();
        assert!(read_with_limit(file, 2).unwrap_err().to_string().contains("exceeds 2 bytes"));
    }

    #[test]
    fn test_reader_refuses_growth_past_the_metadata_bound() {
        let contents = std::io::Cursor::new(b"{}\nmore bytes");
        let error = read_bounded(contents, 3).unwrap_err();
        assert!(error.to_string().contains("exceeds 3 bytes"));
    }

    #[test]
    fn test_state_limits_accept_boundaries_and_reject_each_dimension() {
        assert!(check_state_size(std::iter::repeat_n(0, MAX_STATE_ACCOUNTS)).is_ok());
        let error = check_state_size(std::iter::repeat_n(0, MAX_STATE_ACCOUNTS + 1)).unwrap_err();
        assert!(error.contains("account count"));
        assert!(check_state_size([MAX_STATE_SLOTS / 2, MAX_STATE_SLOTS / 2]).is_ok());
        assert!(check_state_size([MAX_STATE_SLOTS, 1]).unwrap_err().contains("total slots"));
        assert!(check_state_size([1, usize::MAX]).is_err());
    }
}
