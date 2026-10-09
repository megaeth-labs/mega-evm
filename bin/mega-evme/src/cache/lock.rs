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
//!
//! The target is the file a path really names ([`canonical_target`]), not the
//! path as spelled. Two processes reaching one cache file by different names —
//! a symlinked directory and its real path, or a symlink to the file — must
//! take one sidecar, or both "exclusive" locks succeed and the read-merge-rename
//! cycles lose each other's entries; and the rename must land on the file, or it
//! replaces the symlink instead of updating what it points to. Messages keep
//! naming the path the user gave.

use std::{
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
};

/// Symlinks followed while resolving a path whose file does not exist, before
/// giving up: the bound the kernel applies to path resolution.
const MAX_SYMLINKS: usize = 40;

/// The file `path` really names: the one every cache-file writer locks and
/// replaces, however the path is spelled.
///
/// An existing file is canonicalized, following every symlink. A dangling
/// symlink names the file it points to, so a file removed behind a link is
/// recreated where the link expects it. A file that does not exist yet is named
/// under the canonical form of its nearest existing ancestor. When nothing
/// resolves, the path is used as given.
pub(crate) fn canonical_target(path: &Path) -> PathBuf {
    resolve_target(path).unwrap_or_else(|| path.to_path_buf())
}

/// [`canonical_target`], or `None` when the path does not resolve.
pub(crate) fn resolve_target(path: &Path) -> Option<PathBuf> {
    resolve(path, 0)
}

/// Resolve `path` after following `links` symlinks on the way to it.
fn resolve(path: &Path, links: usize) -> Option<PathBuf> {
    if let Ok(real) = fs::canonicalize(path) {
        return Some(real);
    }
    if let Ok(target) = fs::read_link(path) {
        if links >= MAX_SYMLINKS {
            return None;
        }
        let target = match path.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
        return resolve(&target, links + 1);
    }
    let name = path.file_name()?;
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    Some(resolve(parent.unwrap_or_else(|| Path::new(".")), links)?.join(name))
}

/// Path of the advisory lock sidecar for `target` (`<target>.lock`).
///
/// A pure spelling: [`acquire_exclusive_lock`] applies it to the
/// [`canonical_target`], and messages apply it to the path the user gave.
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

/// Acquire an exclusive advisory lock on the sidecar of the file `target`
/// names (`<canonical target>.lock`), blocking until held.
///
/// The sidecar is created if missing and left in place after unlock.
///
/// Callers must fail closed on `Err`: an unlocked write is exactly the
/// lost-update race the lock exists to prevent, so a failed acquisition means
/// "do not write", never "write anyway".
pub(crate) fn acquire_exclusive_lock(target: &Path) -> std::io::Result<ExclusiveFileLock> {
    let lock_path = lock_sidecar_path(&canonical_target(target));
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

    /// Every spelling of one cache file resolves to the one real file: a path
    /// through a symlinked directory, a symlink to the file, a dangling symlink,
    /// and a file that does not exist yet under a symlinked directory (or below
    /// directories that do not exist yet either).
    #[cfg(unix)]
    #[test]
    fn test_canonical_target_resolves_every_alias_of_one_file() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real dir");
        let alias = dir.path().join("alias");
        symlink(&real, &alias).expect("symlink the dir");
        let file = real.join("rpc-cache-1.json");
        fs::write(&file, "{}").expect("write the cache file");
        let real_dir = fs::canonicalize(&real).expect("canonical dir");
        let real_file = real_dir.join("rpc-cache-1.json");

        assert_eq!(canonical_target(&file), real_file);
        assert_eq!(canonical_target(&alias.join("rpc-cache-1.json")), real_file);

        let link = dir.path().join("link.json");
        symlink(&file, &link).expect("symlink the file");
        assert_eq!(canonical_target(&link), real_file, "a symlink names its target");

        let dangling = dir.path().join("dangling.json");
        symlink(real.join("missing.json"), &dangling).expect("dangling symlink");
        assert_eq!(
            canonical_target(&dangling),
            real_dir.join("missing.json"),
            "a dangling symlink names the file it points to"
        );

        assert_eq!(
            canonical_target(&alias.join("new.json")),
            real_dir.join("new.json"),
            "a new file under a symlinked directory resolves through it"
        );
        assert_eq!(
            canonical_target(&alias.join("sub").join("deeper").join("new.json")),
            real_dir.join("sub").join("deeper").join("new.json"),
            "missing directories are named under the nearest real ancestor"
        );
    }

    /// Every alias of one cache file takes one lock: a holder through one
    /// spelling blocks a locker through another until it lets go. Covered for a
    /// symlinked cache directory (the file not existing yet, so both resolve
    /// through the directory) and for a symlink to the file itself, whose
    /// spelled sidecar would be a different file from the target's.
    #[cfg(unix)]
    #[test]
    fn test_aliases_of_one_cache_file_serialize_on_one_lock() {
        use std::{os::unix::fs::symlink, sync::mpsc, thread, time::Duration};

        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("real dir");
        let alias = dir.path().join("alias");
        symlink(&real, &alias).expect("symlink the dir");
        let existing = real.join("rpc-cache-2.json");
        fs::write(&existing, "{}").expect("write the cache file");
        let link = dir.path().join("link.json");
        symlink(&existing, &link).expect("symlink the file");

        for (holder, locker) in [
            (alias.join("rpc-cache-1.json"), real.join("rpc-cache-1.json")),
            (link.clone(), existing),
        ] {
            assert_eq!(
                lock_sidecar_path(&canonical_target(&holder)),
                lock_sidecar_path(&canonical_target(&locker)),
                "{} and {} lock one sidecar",
                holder.display(),
                locker.display()
            );

            let held = acquire_exclusive_lock(&holder).expect("hold through one spelling");
            let (acquired, waiting) = mpsc::channel();
            let thread = thread::spawn(move || {
                let guard = acquire_exclusive_lock(&locker).expect("lock through the other");
                acquired.send(()).expect("report the acquisition");
                drop(guard);
            });
            assert!(
                waiting.recv_timeout(Duration::from_millis(300)).is_err(),
                "{}: the other spelling must wait while one holds the lock",
                holder.display()
            );
            drop(held);
            waiting
                .recv_timeout(Duration::from_secs(10))
                .expect("the other spelling gets the lock once the first lets go");
            thread.join().expect("locker thread");
        }
        assert!(!lock_sidecar_path(&link).exists(), "no sidecar is taken beside the link");
    }
}
