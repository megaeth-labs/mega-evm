//! Binary-level tests for the diagnostics `cache merge` owes the user.
//!
//! `cache merge` has safeguards that can only warn: chain identity that cannot
//! be derived from a filename, and an unreadable output file that the merge is
//! about to replace. Both report a result that is silently wrong or lossy, and
//! both are worthless if the user never sees them.
//!
//! The CLI initializes tracing with the filter at `off` unless `-v` flags or
//! `RUST_LOG` raise it, so these cannot be asserted through a tracing capture
//! in-process: doing that would prove the event is emitted while the default
//! command line still shows nothing. These tests therefore run the real binary
//! with no verbosity flags and `RUST_LOG` removed from its environment, and
//! read what an operator would actually see on stderr.

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use alloy_primitives::B256;
use serde_json::{json, Value};

/// One `{key, value}` provider-cache entry, keyed by a repeated byte.
fn kv(byte: u8, value: &str) -> Value {
    json!({ "key": B256::repeat_byte(byte), "value": value })
}

/// A capture envelope holding `entries`.
fn envelope(entries: Vec<Value>) -> Value {
    json!({ "version": 1, "chain_id": 4326, "cache": entries, "external_env": null })
}

/// Run `mega-evme cache merge` exactly as a default command line would: no `-v`
/// flags, and no inherited `RUST_LOG` that could raise the filter for us.
fn run_merge(inputs: &[&Path], output: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mega-evme"));
    cmd.args(["cache", "merge"]);
    for input in inputs {
        cmd.arg(input);
    }
    cmd.arg("--output").arg(output);
    cmd.env_remove("RUST_LOG");
    cmd.output().expect("run mega-evme cache merge")
}

/// Assert the merge succeeded, and return `(stdout, stderr)`.
fn succeeds(out: &Output) -> (String, String) {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "the merge must still succeed.\nstdout: {stdout}\nstderr: {stderr}",
    );
    (stdout, stderr)
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

/// Read the merged provider-cache array at `path`.
fn read_provider(path: &Path) -> Vec<Value> {
    serde_json::from_str(&fs::read_to_string(path).expect("read merged output"))
        .expect("merged output is a provider-cache array")
}

/// Filenames that carry no chain id leave the cross-chain safeguard unable to
/// run. The merge proceeds — renamed shards are legitimate — but the user is
/// told, on stderr, without asking for verbosity.
#[test]
fn test_cache_merge_warns_on_stderr_when_chain_identity_cannot_be_validated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("worker-a.json");
    let b = dir.path().join("worker-b.json");
    let out = dir.path().join("merged.json");

    fs::write(&a, serde_json::to_string(&vec![kv(1, "from-a")]).unwrap()).expect("write a");
    fs::write(&b, serde_json::to_string(&vec![kv(2, "from-b")]).unwrap()).expect("write b");

    let output = run_merge(&[&a, &b], &out);
    let (stdout, stderr) = succeeds(&output);

    assert!(
        stderr.contains("chain identity cannot be validated"),
        "the safeguard must announce that it could not run: stderr={stderr}",
    );
    assert!(
        stderr.contains("worker-a.json") && stderr.contains("worker-b.json"),
        "each unvalidatable file must be named: stderr={stderr}",
    );

    // Behavior is otherwise unchanged: the merge still produced the union.
    let merged = read_provider(&out);
    assert_eq!(value_of(&merged, 1).as_deref(), Some("from-a"), "{merged:?}");
    assert_eq!(value_of(&merged, 2).as_deref(), Some("from-b"), "{merged:?}");
    assert_eq!(merged.len(), 2, "{merged:?}");
    assert!(stdout.contains("Merged"), "the summary still goes to stdout: {stdout}");
}

/// The warning is specific to unvalidatable names: filenames that agree on a
/// chain id let the safeguard run, and a quiet merge stays quiet.
#[test]
fn test_cache_merge_is_silent_when_filenames_agree_on_the_chain_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let worker0 = dir.path().join("worker0");
    let worker1 = dir.path().join("worker1");
    let merged_dir = dir.path().join("merged");
    for d in [&worker0, &worker1, &merged_dir] {
        fs::create_dir(d).expect("create dir");
    }
    let a = worker0.join("rpc-cache-4326.json");
    let b = worker1.join("rpc-cache-4326.json");
    let out = merged_dir.join("rpc-cache-4326.json");

    fs::write(&a, serde_json::to_string(&vec![kv(1, "from-a")]).unwrap()).expect("write a");
    fs::write(&b, serde_json::to_string(&vec![kv(2, "from-b")]).unwrap()).expect("write b");

    let output = run_merge(&[&a, &b], &out);
    let (_, stderr) = succeeds(&output);

    assert!(
        !stderr.contains("chain identity"),
        "a validated same-chain merge must not warn: stderr={stderr}",
    );
    assert!(stderr.is_empty(), "a clean merge writes nothing to stderr: stderr={stderr}");

    let merged = read_provider(&out);
    assert_eq!(merged.len(), 2, "{merged:?}");
}

/// An unreadable provider output is replaced by the merged inputs, dropping
/// whatever it held. That data loss reaches stderr at default verbosity too.
#[test]
fn test_cache_merge_warns_on_stderr_when_replacing_an_unreadable_provider_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let inputs = dir.path().join("inputs");
    let merged_dir = dir.path().join("merged");
    for d in [&inputs, &merged_dir] {
        fs::create_dir(d).expect("create dir");
    }
    // Convention-following names on both sides, so the only warning that can
    // fire here is the one under test.
    let a = inputs.join("rpc-cache-4326.json");
    let out = merged_dir.join("rpc-cache-4326.json");

    fs::write(&a, serde_json::to_string(&vec![kv(1, "from-a")]).unwrap()).expect("write a");
    fs::write(&out, "not-json{{{").expect("write a corrupt output");

    let output = run_merge(&[&a], &out);
    let (_, stderr) = succeeds(&output);

    assert!(
        stderr.contains("Replacing the existing merge output"),
        "the replacement must be announced: stderr={stderr}",
    );
    assert!(
        stderr.contains("discarded"),
        "the user must be told entries are lost: stderr={stderr}",
    );
    assert!(
        !stderr.contains("chain identity"),
        "no chain-identity warning is due here: stderr={stderr}",
    );

    let merged = read_provider(&out);
    assert_eq!(value_of(&merged, 1).as_deref(), Some("from-a"), "{merged:?}");
    assert_eq!(merged.len(), 1, "{merged:?}");
}

/// The envelope shape replaces an unreadable output the same way, and warns the
/// same way.
#[test]
fn test_cache_merge_warns_on_stderr_when_replacing_an_unreadable_envelope_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.json");
    let out = dir.path().join("out.json");

    fs::write(&a, serde_json::to_string_pretty(&envelope(vec![kv(1, "from-a")])).unwrap())
        .expect("write a");
    fs::write(&out, "not-json{{{").expect("write a corrupt output");

    let output = run_merge(&[&a], &out);
    let (_, stderr) = succeeds(&output);

    assert!(
        stderr.contains("Replacing the existing merge output"),
        "the replacement must be announced: stderr={stderr}",
    );
    assert!(
        stderr.contains("discarded"),
        "the user must be told entries are lost: stderr={stderr}",
    );

    let merged: Value = serde_json::from_str(&fs::read_to_string(&out).expect("read output"))
        .expect("merged output is an envelope");
    let entries = merged["cache"].as_array().expect("cache array").clone();
    assert_eq!(value_of(&entries, 1).as_deref(), Some("from-a"), "{entries:?}");
    assert_eq!(entries.len(), 1, "{entries:?}");
}
