//! Binary-level tests for the diagnostics `cache merge` owes the user.
//!
//! Two of them cannot be inferred from the exit code: an unreadable output file
//! the merge is about to replace, and an input in the format a retired build
//! wrote, which no merge can convert. The first reports a lossy result, the
//! second a refused one, and both are worthless if the user never sees them.
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

/// One `{key, value}` cache entry, keyed by a repeated byte.
fn kv(byte: u8, value: &str) -> Value {
    json!({ "key": B256::repeat_byte(byte), "value": value })
}

/// A cache envelope holding `entries`.
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

/// Read the merged envelope's cache entries at `path`.
fn read_entries(path: &Path) -> Vec<Value> {
    let merged: Value =
        serde_json::from_str(&fs::read_to_string(path).expect("read merged output"))
            .expect("merged output is an envelope");
    merged["cache"].as_array().expect("cache array").clone()
}

/// A merge with nothing to report says nothing: every warning below is a
/// deviation from this baseline, not background noise.
#[test]
fn test_cache_merge_is_silent_on_a_clean_run() {
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

    fs::write(&a, serde_json::to_string_pretty(&envelope(vec![kv(1, "from-a")])).unwrap())
        .expect("write a");
    fs::write(&b, serde_json::to_string_pretty(&envelope(vec![kv(2, "from-b")])).unwrap())
        .expect("write b");

    let output = run_merge(&[&a, &b], &out);
    let (stdout, stderr) = succeeds(&output);

    assert!(stderr.is_empty(), "a clean merge writes nothing to stderr: stderr={stderr}");
    assert!(stdout.contains("Merged"), "the summary still goes to stdout: {stdout}");
    assert_eq!(read_entries(&out).len(), 2);
}

/// An input in the retired array format cannot be merged into anything a build
/// can serve. The refusal exits non-zero and names the file, the likely cause,
/// and the way forward — on stderr, at default verbosity.
#[test]
fn test_cache_merge_rejects_a_retired_array_input_with_guidance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("rpc-cache-4326.json");
    let out = dir.path().join("merged.json");

    fs::write(&a, serde_json::to_string(&vec![kv(1, "from-a")]).unwrap()).expect("write a");

    let output = run_merge(&[&a], &out);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(1), "an unmergeable input is a usage failure");
    assert!(stderr.contains("rpc-cache-4326.json"), "the file must be named: stderr={stderr}");
    assert!(stderr.contains("not a cache envelope"), "stderr={stderr}");
    assert!(stderr.contains("bare JSON array"), "the likely cause: stderr={stderr}");
    assert!(stderr.contains("Delete it"), "the way forward: stderr={stderr}");
    assert!(stderr.contains("--rpc.capture-file"), "the other way forward: stderr={stderr}");
    assert!(!out.exists(), "a refused merge writes no output");
}

/// An unreadable output is replaced by the merged inputs, dropping whatever it
/// held. That data loss reaches stderr at default verbosity too.
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

/// An input that is a hard link to the output is folded in as the output, not
/// read a second time as an input.
///
/// The two names are one file, so the merged *content* is the same either way —
/// the pre-lock copy only overwrites a concurrent writer when there is one. The
/// summary is what tells them apart: an input that reached the output by another
/// name is not counted as one, because it never went through the pre-lock read.
#[cfg(unix)]
#[test]
fn test_cache_merge_does_not_count_a_hard_linked_input_as_an_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.json");
    let link = dir.path().join("link.json");
    let new = dir.path().join("new.json");

    fs::write(&out, serde_json::to_string_pretty(&envelope(vec![kv(1, "from-output")])).unwrap())
        .expect("write out");
    fs::write(&new, serde_json::to_string_pretty(&envelope(vec![kv(2, "from-new")])).unwrap())
        .expect("write new");
    fs::hard_link(&out, &link).expect("hard link the output");

    let output = run_merge(&[&link, &new], &out);
    let (stdout, stderr) = succeeds(&output);

    assert!(
        stdout.contains("Merged 1 inputs (1 entries in + 1 already in the output)"),
        "the hard link must be folded in as the output, not counted as an input: \
         stdout={stdout} stderr={stderr}",
    );

    let entries = read_entries(&out);
    assert_eq!(value_of(&entries, 1).as_deref(), Some("from-output"), "{entries:?}");
    assert_eq!(value_of(&entries, 2).as_deref(), Some("from-new"), "{entries:?}");
    assert_eq!(entries.len(), 2, "{entries:?}");
}
