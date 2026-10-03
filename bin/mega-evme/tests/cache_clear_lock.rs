//! Cross-process serialization for `--rpc.clear-cache` against the sidecar lock.
//!
//! Clear and persist share the exclusive advisory lock on `<cache>.lock`. A
//! clear that unlinks without that lock can race a writer mid re-read-merge-
//! rename: the writer's rename lands after the clear (undoing it), or the clear
//! deletes the file the locked writer just re-read. These tests hold the lock
//! in the test process, spawn a real `mega-evme` that takes the clear-cache
//! path, show the clear does not complete while the lock is held, then release
//! so the clear finishes.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

mod common;
use common::MockRpcServer;

/// How long the lock is held while the spawned clear must make no progress.
const HOLD: Duration = Duration::from_secs(2);

/// Upper bound on how long clear may take after the lock is released.
const COMPLETION_DEADLINE: Duration = Duration::from_secs(60);

/// The advisory lock sidecar the binary locks for `target`.
fn sidecar(target: &Path) -> PathBuf {
    let mut os = target.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

/// Take the exclusive lock on the cache file's sidecar and keep it until the
/// returned handle is dropped.
fn hold_cache_lock(target: &Path) -> File {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(sidecar(target))
        .expect("open the cache sidecar");
    file.lock().expect("hold the cache lock");
    file
}

/// Spawn a single-tx online `replay` that hits `build_provider` with
/// `--rpc.clear-cache`. After the clear the lookup fails; we only care that
/// clear ran under the lock.
fn spawn_clear_cache(rpc: &str, cache_dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args([
            "replay",
            "--rpc",
            rpc,
            "--rpc.cache-dir",
            cache_dir.to_str().expect("utf-8 cache dir"),
            "--rpc.clear-cache",
            "--rpc.max-retries",
            "0",
            "--rpc.backoff-ms",
            "1",
            // Dummy hash: chain-id is enough for clear; the later tx fetch fails.
            "0x0000000000000000000000000000000000000000000000000000000000000001",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mega-evme replay --rpc.clear-cache")
}

/// Assert the spawned process makes no progress for the whole hold window.
fn assert_blocked_while_held(child: &mut Child) {
    let deadline = Instant::now() + HOLD;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll the clear-cache process") {
            panic!("clear-cache completed while the cache lock was held (status {status})",);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        child.try_wait().expect("poll the clear-cache process").is_none(),
        "clear-cache must still be waiting for the cache lock",
    );
}

/// Wait until the child exits (success or failure) after the lock is free.
fn finish(mut child: Child) -> std::process::Output {
    let deadline = Instant::now() + COMPLETION_DEADLINE;
    loop {
        if child.try_wait().expect("poll the clear-cache process").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "clear-cache did not finish within {COMPLETION_DEADLINE:?} of the lock being released",
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().expect("collect clear-cache output")
}

/// Clear waits on a held sidecar lock, does not unlink while blocked, and
/// wipes the seeded content only after the lock is released.
///
/// The file may reappear empty if a later clean-exit-adjacent persist runs
/// after the clear (online replay still installs a disk store); the proof is
/// that the pre-clear seed is gone, not that the path stays unlinked.
#[tokio::test(flavor = "multi_thread")]
async fn test_clear_cache_serializes_with_a_held_sidecar_lock() {
    let server = MockRpcServer::start().await;
    let chain_id: u64 = 55;
    server.respond_eth_chain_id(chain_id, 1).await;
    // After clear, eth_getTransactionByHash (and friends) may be called; a
    // null result fails the run cleanly without hanging.
    server.respond_jsonrpc_null_result(10).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let cache_file = dir.path().join(format!("rpc-cache-{chain_id}.json"));
    let seed = r#"[{"key":"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","value":"seed-before-clear"}]"#;
    fs::write(&cache_file, seed).expect("seed cache file");
    assert!(cache_file.exists());

    let lock = hold_cache_lock(&cache_file);
    let mut child = spawn_clear_cache(&server.uri(), dir.path());
    assert_blocked_while_held(&mut child);
    assert_eq!(
        fs::read_to_string(&cache_file).expect("read while held"),
        seed,
        "clear must not unlink while the sidecar lock is held",
    );

    drop(lock);
    let out = finish(child);
    let after = fs::read_to_string(&cache_file).unwrap_or_default();
    assert!(
        !after.contains("seed-before-clear"),
        "clear must wipe the seeded content once the lock is free.\n\
         after={after}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// A concurrent writer that lands under the lock while clear is queued must
/// still be wiped: clear's critical section is unlink + exists-check + load,
/// so the file written just before clear acquires cannot be reloaded into the
/// clearing session.
///
/// Inverse of `test_clear_cache_serializes_with_a_held_sidecar_lock`: that test
/// seeds before the hold and proves clear does not act while blocked; this one
/// writes only while clear is blocked (as a clean-exit persist would, under
/// the same lock) and proves the injected entries do not survive the clear.
#[tokio::test(flavor = "multi_thread")]
async fn test_clear_cache_wipes_file_written_while_queued_on_lock() {
    let server = MockRpcServer::start().await;
    let chain_id: u64 = 56;
    server.respond_eth_chain_id(chain_id, 1).await;
    server.respond_jsonrpc_null_result(10).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let cache_file = dir.path().join(format!("rpc-cache-{chain_id}.json"));
    // No seed before the hold: the only pollution is what the concurrent
    // writer leaves under the lock while clear waits.
    assert!(!cache_file.exists(), "precondition: cache file must be absent");

    let lock = hold_cache_lock(&cache_file);
    let mut child = spawn_clear_cache(&server.uri(), dir.path());
    assert_blocked_while_held(&mut child);

    // Concurrent persist finishes under the lock clear is waiting for: its
    // rename lands, then it releases — clear acquires next and must treat this
    // file as the one to wipe, not as content to load after an early unlock.
    let injected = r#"[{"key":"0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","value":"injected-while-clear-queued"}]"#;
    fs::write(&cache_file, injected).expect("write while clear is queued");
    assert_eq!(
        fs::read_to_string(&cache_file).expect("read injected"),
        injected,
        "injected file must still be present while clear is blocked",
    );

    drop(lock);
    let out = finish(child);
    let after = fs::read_to_string(&cache_file).unwrap_or_default();
    assert!(
        !after.contains("injected-while-clear-queued"),
        "clear must delete the concurrent write and start empty; injected \
         entries must not reappear via load-then-persist.\n\
         after={after}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
