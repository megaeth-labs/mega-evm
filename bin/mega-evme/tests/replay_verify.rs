//! Integration tests for `mega-evme replay --verify-receipt`: the end-to-end
//! comparison against the on-chain receipt in single-transaction and batch mode.
//!
//! They run fully offline against the committed RPC capture that carries the
//! on-chain receipt (`fixtures/replay_offline.cache.json`), so they are
//! deterministic. Mismatch and infrastructure cases are produced by doctoring a
//! copy of that capture: its entries are keyed by the request, not the response,
//! so a doctored response still resolves.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

mod common;

/// Offline RPC capture, including the transaction's on-chain receipt.
const CACHE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/replay_offline.cache.json");

/// The transaction captured in `CACHE` (a 75,514-gas Rex5 mainnet call).
const TX: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

/// Gas the transaction used on-chain, which a faithful replay reproduces.
const GAS_USED: u64 = 75_514;

/// Outcome of one `mega-evme replay` invocation.
struct Run {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The process exit code the run ended with.
    fn code(&self) -> i32 {
        self.code.expect("mega-evme was killed by a signal")
    }

    /// The results printed on stdout, without the structured error object a
    /// failing `--json` run ends with.
    fn results(&self) -> Vec<serde_json::Value> {
        let mut values = common::json_values(&self.stdout);
        if values.last().is_some_and(common::is_run_error) {
            values.pop();
        }
        values
    }

    /// Parse the stdout of a `--json` single-transaction run.
    fn json(&self) -> serde_json::Value {
        let mut results = self.results();
        assert_eq!(results.len(), 1, "expected one summary on stdout:\n{}", self.stdout);
        results.pop().expect("checked above")
    }

    /// Parse the stdout of a `--json` batch run as one value per NDJSON line.
    fn ndjson(&self) -> Vec<serde_json::Value> {
        self.results()
    }

    /// The structured error object a failing `--json` run ends with.
    fn error_object(&self) -> serde_json::Value {
        let values = common::json_values(&self.stdout);
        let last = values
            .last()
            .unwrap_or_else(|| panic!("a failing --json run must not leave stdout empty"));
        assert!(
            common::is_run_error(last),
            "the last stdout value must be the error object, got: {last}"
        );
        last.clone()
    }
}

/// Run `replay` offline against `cache`.
fn replay(cache: &Path, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["replay", "--rpc.replay-file", cache.to_str().expect("cache path is utf-8")])
        .args(args)
        .output()
        .expect("failed to run mega-evme");
    Run {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

/// The committed capture, unmodified.
fn cache() -> PathBuf {
    PathBuf::from(CACHE)
}

/// A temp path unique to this process and this test.
fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mega_evme_verify_{name}_{}.json", std::process::id()))
}

/// Write a copy of the committed capture whose receipt response is rewritten by
/// `doctor`, and return its path.
fn doctored_cache(name: &str, doctor: impl Fn(&mut serde_json::Value)) -> PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(CACHE).expect("read offline cache"))
            .expect("parse offline cache");
    let mut doctored = false;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        // The receipt is the only cached response carrying cumulativeGasUsed.
        if !value.contains("cumulativeGasUsed") {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse receipt response");
        doctor(&mut response["result"]);
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored = true;
    }
    assert!(doctored, "offline cache should contain the receipt entry");

    let path = temp_path(name);
    std::fs::write(&path, envelope.to_string()).expect("write doctored cache");
    path
}

/// Write a copy of the committed capture with the receipt dropped entirely,
/// modelling an endpoint that has pruned it.
fn cache_without_receipt(name: &str) -> PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(CACHE).expect("read offline cache"))
            .expect("parse offline cache");
    let entries = envelope["cache"].as_array_mut().expect("cache entries");
    let before = entries.len();
    entries.retain(|entry| {
        !entry["value"].as_str().expect("entry value is a string").contains("cumulativeGasUsed")
    });
    assert!(entries.len() < before, "offline cache should contain the receipt entry");

    let path = temp_path(name);
    std::fs::write(&path, envelope.to_string()).expect("write pruned cache");
    path
}

/// Write a `--tx-file` holding the single captured transaction.
fn tx_file(name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("mega_evme_verify_{name}_{}.txt", std::process::id()));
    std::fs::write(&path, format!("{TX}\n")).expect("write tx list");
    path
}

/// A faithful replay reproduces the on-chain receipt, reports a match, and exits 0.
#[test]
fn test_verify_receipt_reports_a_match() {
    let run = replay(&cache(), &["--verify-receipt", "--json", TX]);

    assert!(run.success, "a matching verification must exit 0.\nstderr: {}", run.stderr);
    assert_eq!(run.json()["verification"], serde_json::json!({ "match": true }));
}

/// Human-readable output carries one verdict line per transaction.
#[test]
fn test_verify_receipt_prints_a_human_verdict_line() {
    let run = replay(&cache(), &["--verify-receipt", TX]);

    assert!(run.success, "a matching verification must exit 0.\nstderr: {}", run.stderr);
    assert!(
        run.stdout.contains("verification: MATCH"),
        "expected a verdict line, got stdout:\n{}",
        run.stdout
    );
}

/// Without the flag the single-transaction JSON is unchanged: no `verification`
/// key, and the flag adds that key and nothing else.
#[test]
fn test_single_transaction_json_is_unchanged_without_the_flag() {
    let plain = replay(&cache(), &["--json", TX]);
    let verified = replay(&cache(), &["--verify-receipt", "--json", TX]);

    assert!(plain.success && verified.success, "both runs must exit 0");
    assert!(
        !plain.stdout.contains("verification"),
        "output without the flag must not mention verification:\n{}",
        plain.stdout
    );
    assert!(plain.json().get("verification").is_none(), "the key must be absent without the flag");

    let mut stripped = verified.json();
    stripped.as_object_mut().expect("summary is an object").remove("verification");
    assert_eq!(stripped, plain.json(), "--verify-receipt must add the verdict and nothing else");
}

/// A gas divergence is reported as a `gas_used` diff and fails the run with the
/// dedicated mismatch exit code.
#[test]
fn test_verify_receipt_reports_a_gas_mismatch() {
    let path = doctored_cache("gas", |receipt| receipt["gasUsed"] = "0x1".into());

    let run = replay(&path, &["--verify-receipt", "--json", TX]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 2, "a mismatch exits 2.\nstderr: {}", run.stderr);
    assert_eq!(
        run.json()["verification"],
        serde_json::json!({
            "match": false,
            "diff": { "gas_used": { "onchain": 1, "replay": GAS_USED } },
        })
    );
    assert_eq!(run.error_object()["error"]["code"].as_u64(), Some(2));
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("verification-mismatch"));
    assert!(
        run.stderr.contains("Receipt verification mismatch"),
        "expected the mismatch error, got stderr:\n{}",
        run.stderr
    );
}

/// A status divergence is reported on its own, without dragging in the
/// dimensions that agreed.
#[test]
fn test_verify_receipt_reports_a_status_mismatch() {
    let path = doctored_cache("status", |receipt| receipt["status"] = "0x0".into());

    let run = replay(&path, &["--verify-receipt", "--json", TX]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 2, "a mismatch exits 2.\nstderr: {}", run.stderr);
    assert_eq!(
        run.json()["verification"],
        serde_json::json!({
            "match": false,
            "diff": { "status": { "onchain": false, "replay": true } },
        })
    );
}

/// A log divergence is reported under `logs`.
#[test]
fn test_verify_receipt_reports_a_log_mismatch() {
    let path = doctored_cache("logs", |receipt| {
        receipt["logs"] = serde_json::json!([{
            "address": "0x00000000000000000000000000000000000000aa",
            "topics": ["0x000000000000000000000000000000000000000000000000000000000000000a"],
            "data": "0xdeadbeef",
            "blockHash": receipt["blockHash"],
            "blockNumber": receipt["blockNumber"],
            "transactionHash": receipt["transactionHash"],
            "transactionIndex": receipt["transactionIndex"],
            "logIndex": "0x0",
            "removed": false,
        }]);
    });

    let run = replay(&path, &["--verify-receipt", "--json", TX]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 2, "a mismatch exits 2.\nstderr: {}", run.stderr);
    assert_eq!(
        run.json()["verification"],
        serde_json::json!({
            "match": false,
            "diff": { "logs": { "count": { "onchain": 1, "replay": 0 } } },
        })
    );
}

/// A receipt describing a different inclusion than the replayed block is an
/// infrastructure failure: the transaction is unverified, never mismatched.
#[test]
fn test_verify_receipt_reorg_is_an_infrastructure_error() {
    let path = doctored_cache("reorg", |receipt| {
        receipt["blockHash"] =
            "0x1111111111111111111111111111111111111111111111111111111111111111".into();
    });

    let run = replay(&path, &["--verify-receipt", "--json", TX]);
    let _ = std::fs::remove_file(&path);

    // The comparison never ran, so the run fails as an RPC-class failure, not
    // as a mismatch.
    assert_eq!(run.code(), 3, "an unverifiable target exits 3.\nstderr: {}", run.stderr);
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        run.stderr.contains("different inclusion"),
        "expected the reorg/divergent-endpoint hint, got stderr:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("verification mismatch") && !run.stdout.contains("MISMATCH"),
        "an unverifiable transaction must not be reported as a mismatch:\n{}\n{}",
        run.stdout,
        run.stderr,
    );
}

/// A receipt the endpoint cannot serve (e.g. pruned below its retention height)
/// is an infrastructure failure, not a mismatch.
#[test]
fn test_verify_receipt_missing_receipt_is_an_infrastructure_error() {
    let path = cache_without_receipt("pruned");

    let run = replay(&path, &["--verify-receipt", "--json", TX]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 3, "an unavailable receipt exits 3.\nstderr: {}", run.stderr);
    assert!(
        run.stderr.contains("receipt"),
        "expected an error naming the receipt, got stderr:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("verification mismatch") && !run.stdout.contains("MISMATCH"),
        "an unverifiable transaction must not be reported as a mismatch:\n{}\n{}",
        run.stdout,
        run.stderr,
    );
}

/// Batch mode carries the verdict on the transaction's NDJSON line.
#[test]
fn test_batch_verify_receipt_reports_a_match() {
    let list = tx_file("batch_match");

    let run =
        replay(&cache(), &["--tx-file", list.to_str().unwrap(), "--verify-receipt", "--json"]);
    let _ = std::fs::remove_file(&list);

    assert!(run.success, "a matching verification must exit 0.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "one line per requested transaction");
    assert_eq!(lines[0]["tx_hash"].as_str(), Some(TX));
    assert_eq!(lines[0]["verification"], serde_json::json!({ "match": true }));
}

/// A batch mismatch keeps the result line (with its diff) and fails the run
/// through the dedicated verification error.
#[test]
fn test_batch_verify_receipt_reports_a_mismatch_and_exits_nonzero() {
    let path = doctored_cache("batch_gas", |receipt| receipt["gasUsed"] = "0x1".into());
    let list = tx_file("batch_gas");

    let run = replay(&path, &["--tx-file", list.to_str().unwrap(), "--verify-receipt", "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 2, "a mismatch exits 2.\nstderr: {}", run.stderr);
    assert_eq!(run.error_object()["error"]["code"].as_u64(), Some(2));
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "a mismatch is still a result line, not an error entry");
    assert!(lines[0].get("error").is_none(), "a mismatch is not an infrastructure error");
    assert_eq!(
        lines[0]["verification"],
        serde_json::json!({
            "match": false,
            "diff": { "gas_used": { "onchain": 1, "replay": GAS_USED } },
        })
    );
    assert!(
        run.stderr.contains("Receipt verification mismatch"),
        "expected the mismatch error, got stderr:\n{}",
        run.stderr
    );
}

/// In batch mode an unavailable receipt turns the target into an `rpc` error
/// entry — reported as unverified, never as a mismatch.
#[test]
fn test_batch_verify_receipt_missing_receipt_is_an_rpc_error_entry() {
    let path = cache_without_receipt("batch_pruned");
    let list = tx_file("batch_pruned");

    let run = replay(&path, &["--tx-file", list.to_str().unwrap(), "--verify-receipt", "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 3, "an unverified target exits 3.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "one line per requested transaction");
    assert_eq!(lines[0]["error"]["kind"].as_str(), Some("rpc"));
    assert!(lines[0].get("verification").is_none(), "an unverified target carries no verdict");
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        !run.stderr.contains("verification mismatch"),
        "an unverifiable target must not fail as a mismatch:\n{}",
        run.stderr
    );
}

/// The reorg guard applies in batch mode too, as an `rpc` error entry.
#[test]
fn test_batch_verify_receipt_reorg_is_an_rpc_error_entry() {
    let path = doctored_cache("batch_reorg", |receipt| {
        receipt["blockHash"] =
            "0x1111111111111111111111111111111111111111111111111111111111111111".into();
    });
    let list = tx_file("batch_reorg");

    let run = replay(&path, &["--tx-file", list.to_str().unwrap(), "--verify-receipt", "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 3, "an unverified target exits 3.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines[0]["error"]["kind"].as_str(), Some("rpc"));
    assert!(
        lines[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("different inclusion")),
        "expected the reorg/divergent-endpoint hint: {}",
        lines[0]
    );
}

/// Batch `--dump-fixture-dir` writes a self-validating fixture for a target
/// whose capture includes the on-chain receipt, and exits 0.
#[test]
fn test_batch_dump_fixture_dir_writes_validatable_file() {
    let list = tx_file("batch_dump");
    let dir = std::env::temp_dir().join(format!("mega_evme_batch_dump_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let run = replay(
        &cache(),
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&list);

    assert!(run.success, "a successful dump must exit 0.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "one line per requested transaction");
    assert_eq!(lines[0]["tx_hash"].as_str(), Some(TX));
    let path = lines[0]["fixture"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a written fixture path: {}", lines[0]));
    assert!(
        path.ends_with(&format!("{TX}.json")),
        "fixture path should be <DIR>/<tx_hash>.json, got: {path}"
    );
    assert!(std::path::Path::new(path).exists(), "fixture file must exist at {path}");

    let elapsed = std::sync::Arc::new(std::sync::Mutex::new(std::time::Duration::ZERO));
    let result = state_test::runner::execute_test_suite(Path::new(path), &elapsed, false, false);
    let _ = std::fs::remove_dir_all(&dir);
    result.unwrap_or_else(|e| panic!("dumped fixture failed to validate: {e}"));
}

/// Without `--overwrite`, a second dump into a directory that already holds the
/// fixture fails that target as an infrastructure error.
#[test]
fn test_batch_dump_fixture_dir_refuses_overwrite_without_flag() {
    let list = tx_file("batch_dump_ow");
    let dir = std::env::temp_dir().join(format!("mega_evme_batch_dump_ow_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let first = replay(
        &cache(),
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(first.success, "first dump must succeed.\nstderr: {}", first.stderr);

    let second = replay(
        &cache(),
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    // The dump failed for the target, which is an execution-class failure.
    assert_eq!(second.code(), 1, "a failed dump exits 1.\nstderr: {}", second.stderr);
    let lines = second.ndjson();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["error"]["kind"].as_str(), Some("execution"));
    assert!(
        lines[0]["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("already exists") && m.contains("--overwrite")),
        "expected overwrite refusal: {}",
        lines[0]
    );

    // With --overwrite the second dump succeeds and replaces the file.
    let third = replay(
        &cache(),
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--overwrite",
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&list);
    assert!(third.success, "dump with --overwrite must succeed.\nstderr: {}", third.stderr);
    let lines = third.ndjson();
    assert!(lines[0]["fixture"]["path"].is_string(), "overwrite must report a written path");
    let _ = std::fs::remove_dir_all(&dir);
}
