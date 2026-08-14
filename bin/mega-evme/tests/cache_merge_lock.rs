//! Two-process serialization tests for the `cache merge` output lock.
//!
//! `cache merge` writes a file a live `mega-evme` run may be persisting to at
//! the same time. Both writers take the exclusive advisory lock on the output's
//! sidecar and re-read the file while holding it, so neither side's entries are
//! lost to whichever rename lands last.
//!
//! Mutual exclusion cannot be demonstrated inside one process: a single-process
//! test that "takes the lock" and then calls the merge in-process either
//! deadlocks or proves nothing about a second process. This test holds the
//! sidecar lock in the test process, spawns the real binary, shows it makes no
//! progress while the lock is held, writes a concurrent writer's entries under
//! that same lock, and only then releases — so the merge's output can contain
//! those entries only by re-reading the file after it acquired the lock.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use alloy_primitives::B256;
use serde_json::{json, Value};

/// How long the lock is held while the spawned merge must make no progress.
///
/// Merging two one-entry files takes milliseconds, so staying alive for this
/// long is only explainable by the lock.
const HOLD: Duration = Duration::from_secs(2);

/// Upper bound on how long the merge may take after the lock is released.
/// Generous on purpose: this bound exists to fail a hung merge with a message
/// instead of hanging the suite, not to measure anything.
const COMPLETION_DEADLINE: Duration = Duration::from_secs(60);

/// One `{key, value}` cache entry, keyed by a repeated byte.
fn kv(byte: u8, value: &str) -> Value {
    json!({ "key": B256::repeat_byte(byte), "value": value })
}

/// The advisory lock sidecar the binary locks for `output`.
fn sidecar(output: &Path) -> PathBuf {
    let mut os = output.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

/// Take the exclusive lock on the output's sidecar and keep it until the
/// returned handle is dropped.
fn hold_output_lock(output: &Path) -> File {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(sidecar(output))
        .expect("open the output sidecar");
    file.lock().expect("hold the output lock");
    file
}

/// Spawn the real `mega-evme cache merge` process.
fn spawn_merge(inputs: &[&Path], output: &Path) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mega-evme"));
    cmd.args(["cache", "merge"]);
    for input in inputs {
        cmd.arg(input);
    }
    cmd.arg("--output").arg(output).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.spawn().expect("spawn mega-evme cache merge")
}

/// Assert the spawned merge makes no progress for the whole hold window.
fn assert_blocked_while_held(child: &mut Child) {
    let deadline = Instant::now() + HOLD;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll the merge") {
            panic!("the merge completed while the output lock was held (status {status})");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        child.try_wait().expect("poll the merge").is_none(),
        "the merge must still be waiting for the output lock",
    );
}

/// Wait for the released merge to finish and return its stdout.
fn finish(mut child: Child) -> String {
    let deadline = Instant::now() + COMPLETION_DEADLINE;
    loop {
        if child.try_wait().expect("poll the merge").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the merge did not finish within {COMPLETION_DEADLINE:?} of the lock being released",
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().expect("collect the merge output");
    assert!(
        out.status.success(),
        "the merge must succeed once the lock is free.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Value stored for the entry keyed by a repeated byte, if present.
fn value_of(entries: &[Value], byte: u8) -> Option<String> {
    let key = json!(B256::repeat_byte(byte));
    entries
        .iter()
        .find(|e| e.get("key") == Some(&key))
        .and_then(|e| e.get("value"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// An envelope holding `entries`.
fn envelope(entries: Vec<Value>) -> Value {
    json!({
        "version": 1,
        "chain_id": 4326,
        "cache": entries,
        "external_env": null,
    })
}

/// The merge waits for the lock, then folds in what a concurrent writer left in
/// the output while it waited.
#[test]
fn test_cache_merge_serializes_with_a_concurrent_envelope_writer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.json");
    let b = dir.path().join("b.json");
    let out = dir.path().join("out.json");

    fs::write(&a, serde_json::to_string_pretty(&envelope(vec![kv(1, "from-a")])).unwrap())
        .expect("write a");
    fs::write(&b, serde_json::to_string_pretty(&envelope(vec![kv(2, "from-b")])).unwrap())
        .expect("write b");

    let lock = hold_output_lock(&out);
    let mut child = spawn_merge(&[&a, &b], &out);
    assert_blocked_while_held(&mut child);

    // A concurrent writer lands its entries the way a clean-exit persist does:
    // while it holds the same lock the merge is waiting for.
    fs::write(
        &out,
        serde_json::to_string_pretty(&envelope(vec![kv(9, "from-concurrent-writer")])).unwrap(),
    )
    .expect("concurrent write");
    drop(lock);

    let stdout = finish(child);

    let merged: Value =
        serde_json::from_str(&fs::read_to_string(&out).expect("read merged output"))
            .expect("merged output is an envelope");
    let entries = merged["cache"].as_array().expect("cache array").clone();
    assert_eq!(
        value_of(&entries, 9).as_deref(),
        Some("from-concurrent-writer"),
        "the entry written while the merge was blocked must survive: {entries:?}",
    );
    assert_eq!(value_of(&entries, 1).as_deref(), Some("from-a"), "input entry lost: {entries:?}");
    assert_eq!(value_of(&entries, 2).as_deref(), Some("from-b"), "input entry lost: {entries:?}");
    assert_eq!(entries.len(), 3, "the union is exactly both sides: {entries:?}");
    assert_eq!(merged["chain_id"], json!(4326));
    assert!(
        stdout.contains("already in the output"),
        "the summary must report the folded-in entries: {stdout}",
    );
}
