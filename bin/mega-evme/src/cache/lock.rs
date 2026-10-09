//! Advisory locking for cache files shared by concurrent processes.
//!
//! Every writer of a cache file — clean-exit persist and the offline
//! `cache merge` subcommand alike — takes the exclusive lock on that file's
//! sidecar before it re-reads the file, merges, and renames the result into
//! place. A writer that skips the lock can only be correct by luck: two
//! read-modify-write cycles that interleave lose whichever side renamed first.
//!
//! The lock lives on a sidecar (`<target>.lock`) rather than the cache file
//! itself because the target is replaced by rename on every write, and a lock
//! held on the replaced inode protects nothing.

use std::{
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
};

/// Path of the advisory lock sidecar for `target` (`<target>.lock`).
pub(crate) fn lock_sidecar_path(target: &Path) -> PathBuf {
    let mut os = target.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

/// RAII exclusive lock on the sidecar file for a cache target.
///
/// The lock is released when this guard is dropped (file handle closed).
/// The sidecar file itself is left on disk.
#[derive(Debug)]
pub(crate) struct ExclusiveFileLock {
    _file: fs::File,
}

/// Acquire an exclusive advisory lock on `<target>.lock`, blocking until held.
///
/// The sidecar is created if missing and left in place after unlock.
///
/// Callers must fail closed on `Err`: an unlocked write is exactly the
/// lost-update race the lock exists to prevent, so a failed acquisition means
/// "do not write", never "write anyway".
pub(crate) fn acquire_exclusive_lock(target: &Path) -> std::io::Result<ExclusiveFileLock> {
    let lock_path = lock_sidecar_path(target);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // truncate(false): the sidecar is only a flock target; keep any existing bytes.
    let file =
        OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&lock_path)?;
    // Blocking exclusive advisory lock.
    file.lock()?;
    Ok(ExclusiveFileLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_sidecar_path_suffix() {
        let p = Path::new("/tmp/rpc-cache-1.json");
        assert_eq!(lock_sidecar_path(p), PathBuf::from("/tmp/rpc-cache-1.json.lock"));
    }

    /// The sidecar is created on acquisition and left in place after unlock.
    #[test]
    fn test_acquire_exclusive_lock_creates_and_keeps_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("rpc-cache-1.json");
        let sidecar = lock_sidecar_path(&target);
        assert!(!sidecar.exists());

        let guard = acquire_exclusive_lock(&target).expect("acquire");
        assert!(sidecar.exists(), "sidecar created while held");
        drop(guard);
        assert!(sidecar.exists(), "sidecar left in place after unlock");
    }

    /// An un-openable sidecar path surfaces as an error rather than a silent
    /// unlocked write.
    #[test]
    fn test_acquire_exclusive_lock_reports_unopenable_sidecar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("rpc-cache-1.json");
        // A directory in the sidecar's place cannot be opened as a file.
        fs::create_dir(lock_sidecar_path(&target)).expect("occupy sidecar path");

        acquire_exclusive_lock(&target).expect_err("un-openable sidecar must not silently succeed");
    }
}
