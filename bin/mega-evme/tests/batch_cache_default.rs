//! Integration tests for the batch-mode on-disk cache default.
//!
//! Batch replay (`--tx-file` / `--block`) engages the on-disk RPC cache only
//! when the invocation asks for it explicitly. The clean-exit persist
//! re-reads, merges, and atomically rewrites the whole per-chain cache file
//! under a cross-process lock, so its cost grows with the file and serializes
//! across concurrent processes — while a linear history scan gets almost no
//! cache hits in return. With the default in place a batch exit performs zero
//! disk-cache work, making its cost independent of any cache a machine has
//! accumulated. Single-transaction replay keeps the previous default.
//!
//! `--rpc.clear-cache` opts a batch run back in the same way `--rpc.cache-dir`
//! does: deleting the cache file is a request that only means something while
//! the disk cache is engaged, so forcing it off would make the documented
//! recovery flag a no-op and leave the polluted file for the next run.
//!
//! The tests point the child's platform cache directory into a temp dir via
//! `HOME` / `XDG_CACHE_HOME`, so the real user cache is never touched.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::TempDir;

mod common;
use common::MockRpcServer;

/// Any syntactically valid transaction hash; every lookup fails at the mock.
const TX: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

/// A fake home directory the child process resolves its platform cache dir in.
struct FakeHome {
    dir: TempDir,
}

impl FakeHome {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("tempdir") }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where the child's default per-chain cache file lands for chain 4326.
    ///
    /// Mirrors `dirs::cache_dir()` under the overridden environment: macOS
    /// resolves `$HOME/Library/Caches`, other unixes `$XDG_CACHE_HOME` (which
    /// the tests always set).
    fn default_cache_file(&self) -> PathBuf {
        let base = if cfg!(target_os = "macos") {
            self.path().join("Library/Caches")
        } else {
            self.path().join("xdg-cache")
        };
        base.join("mega-evme/rpc/rpc-cache-4326.json")
    }

    /// Every `rpc-cache-*.json` anywhere under the fake home.
    fn cache_files(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, hits: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, hits);
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("rpc-cache-") && name.ends_with(".json") {
                        hits.push(path);
                    }
                }
            }
        }
        let mut hits = Vec::new();
        walk(self.path(), &mut hits);
        hits
    }
}

/// Run `mega-evme replay` with the platform cache dir redirected into `home`.
fn replay(home: &FakeHome, args: &[&str]) {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .arg("replay")
        .args(args)
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("xdg-cache"))
        .output()
        .expect("failed to run mega-evme");
    // Every scenario here replays a transaction the mock cannot answer, so the
    // run itself fails; the assertions are about the cache file side effects.
    assert!(output.status.code().is_some(), "mega-evme must exit, not die on a signal: {output:?}");
}

/// A mock whose chain id resolves (4326) and whose every other request fails
/// without triggering the retry layer.
async fn failing_mock() -> MockRpcServer {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    server.respond_jsonrpc_error(-32601, "no such method", 2).await;
    server
}

/// Write a `--tx-file` under the fake home and return its path.
fn tx_file(home: &FakeHome) -> PathBuf {
    let path = home.path().join("targets.txt");
    std::fs::write(&path, format!("{TX}\n")).expect("write tx file");
    path
}

/// A default-flag batch run must neither read nor write any on-disk cache:
/// a pre-existing default cache file survives byte-identical (even though its
/// content is garbage a load would have rejected), and no new cache file
/// appears anywhere. The exit therefore does no work proportional to the
/// cache a machine has accumulated, no matter how many targets were replayed.
#[tokio::test(flavor = "multi_thread")]
async fn test_batch_default_leaves_disk_cache_untouched() {
    let home = FakeHome::new();
    let seeded = home.default_cache_file();
    std::fs::create_dir_all(seeded.parent().expect("cache file has a parent")).expect("mkdir");
    std::fs::write(&seeded, b"not even json").expect("seed cache file");

    let server = failing_mock().await;
    let targets = tx_file(&home);

    replay(
        &home,
        &[
            "--tx-file",
            targets.to_str().expect("utf-8"),
            "--rpc",
            &server.uri(),
            "--rpc.max-retries",
            "0",
            "--rpc.backoff-ms",
            "1",
            "--json",
        ],
    );

    let bytes = std::fs::read(&seeded).expect("seeded file must still exist");
    assert_eq!(bytes, b"not even json", "the seeded default cache file must stay byte-identical");
    assert_eq!(
        home.cache_files(),
        vec![seeded],
        "no other cache file may appear anywhere under the fake home",
    );
}

/// An explicit `--rpc.clear-cache` opts a batch run back into the disk cache at
/// the default path: the seeded file is deleted before the run and a fresh cache
/// file is persisted on exit. Forcing the cache off instead would make the flag
/// parse and do nothing, leaving the polluted file for the next non-batch run.
#[tokio::test(flavor = "multi_thread")]
async fn test_batch_clear_cache_clears_and_repersists_default_path() {
    let home = FakeHome::new();
    let seeded = home.default_cache_file();
    std::fs::create_dir_all(seeded.parent().expect("cache file has a parent")).expect("mkdir");
    std::fs::write(&seeded, b"not even json").expect("seed cache file");

    let server = failing_mock().await;
    let targets = tx_file(&home);

    replay(
        &home,
        &[
            "--tx-file",
            targets.to_str().expect("utf-8"),
            "--rpc",
            &server.uri(),
            "--rpc.clear-cache",
            "--rpc.max-retries",
            "0",
            "--rpc.backoff-ms",
            "1",
            "--json",
        ],
    );

    let bytes = std::fs::read(&seeded).expect("a fresh cache file must exist after the run");
    assert_ne!(
        bytes, b"not even json",
        "the seeded cache file must have been cleared, not carried forward",
    );
    // The clear only happened because the disk cache was engaged, so the exit
    // persist must have written a well-formed cache envelope in its place.
    let doc: serde_json::Value =
        serde_json::from_slice(&bytes).expect("the persisted cache file must be valid JSON");
    assert_eq!(doc["version"], serde_json::json!(1), "doc={doc}");
    assert_eq!(doc["kind"], serde_json::json!("cache"), "doc={doc}");
    assert_eq!(
        home.cache_files(),
        vec![seeded],
        "the run must not create a cache file anywhere else under the fake home",
    );
}

/// `--rpc.no-cache-file` wins over `--rpc.clear-cache`: with no cache file in
/// play there is nothing to delete, load, or persist, so a seeded file survives
/// byte-identical. Batch mode passes the pair through unchanged, so both target
/// forms behave the same way.
#[tokio::test(flavor = "multi_thread")]
async fn test_no_cache_file_wins_over_clear_cache_in_both_modes() {
    for batch in [false, true] {
        let home = FakeHome::new();
        let seeded = home.default_cache_file();
        std::fs::create_dir_all(seeded.parent().expect("cache file has a parent")).expect("mkdir");
        std::fs::write(&seeded, b"not even json").expect("seed cache file");

        let server = failing_mock().await;
        let uri = server.uri();
        let targets = tx_file(&home);

        let mut args =
            if batch { vec!["--tx-file", targets.to_str().expect("utf-8")] } else { vec![TX] };
        args.extend_from_slice(&[
            "--rpc",
            &uri,
            "--rpc.no-cache-file",
            "--rpc.clear-cache",
            "--rpc.max-retries",
            "0",
            "--rpc.backoff-ms",
            "1",
            "--json",
        ]);
        replay(&home, &args);

        let bytes = std::fs::read(&seeded).expect("the seeded file must still exist");
        assert_eq!(
            bytes, b"not even json",
            "--rpc.no-cache-file must keep the disk cache out of play (batch = {batch})",
        );
        assert_eq!(
            home.cache_files(),
            vec![seeded],
            "no cache file may be written anywhere (batch = {batch})",
        );
    }
}

/// An explicit `--rpc.cache-dir` opts a batch run back into persistence: the
/// per-chain cache file is written on exit.
#[tokio::test(flavor = "multi_thread")]
async fn test_batch_explicit_cache_dir_still_persists() {
    let home = FakeHome::new();
    let cache_dir = home.path().join("explicit-cache");

    let server = failing_mock().await;
    let targets = tx_file(&home);

    replay(
        &home,
        &[
            "--tx-file",
            targets.to_str().expect("utf-8"),
            "--rpc",
            &server.uri(),
            "--rpc.cache-dir",
            cache_dir.to_str().expect("utf-8"),
            "--rpc.max-retries",
            "0",
            "--rpc.backoff-ms",
            "1",
            "--json",
        ],
    );

    assert!(
        cache_dir.join("rpc-cache-4326.json").exists(),
        "an explicit --rpc.cache-dir must persist the cache file on exit",
    );
}

/// Single-transaction replay keeps the previous default: the per-chain cache
/// file is persisted into the platform cache directory.
#[tokio::test(flavor = "multi_thread")]
async fn test_single_replay_still_persists_by_default() {
    let home = FakeHome::new();

    let server = failing_mock().await;

    replay(
        &home,
        &[TX, "--rpc", &server.uri(), "--rpc.max-retries", "0", "--rpc.backoff-ms", "1", "--json"],
    );

    assert!(
        home.default_cache_file().exists(),
        "single-transaction replay must keep persisting the default cache file",
    );
}
