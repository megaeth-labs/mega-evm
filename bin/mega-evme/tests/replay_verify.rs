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
/// Name of the committed offline capture, resolved through the shared fixture
/// helper so its location lives in exactly one place.
const CACHE: &str = "replay_offline.cache.json";

/// The transaction captured in `CACHE` (a 75,514-gas Rex5 mainnet call).
const TX: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

/// Gas the transaction used on-chain, which a faithful replay reproduces.
const GAS_USED: u64 = 75_514;

/// A transaction hash that is not the replayed target, used to model an endpoint
/// answering a receipt request with another transaction's receipt.
const OTHER_TX: &str = "0x00000000000000000000000000000000000000000000000000000000feed0001";

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
    replay_with_env(cache, args, &[])
}

/// Run `replay` offline against `cache` with additional process environment.
fn replay_with_env(cache: &Path, args: &[&str], envs: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mega-evme"));
    cmd.args(["replay", "--rpc.replay-file", cache.to_str().expect("cache path is utf-8")])
        .args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("failed to run mega-evme");
    Run {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

/// The committed capture, unmodified.
fn cache() -> PathBuf {
    common::fixture(CACHE)
}

/// A temp path unique to this process and this test.
fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mega_evme_verify_{name}_{}.json", std::process::id()))
}

/// Write a copy of the committed capture whose receipt response is rewritten by
/// `doctor`, and return its path.
fn doctored_cache(name: &str, doctor: impl Fn(&mut serde_json::Value)) -> PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
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
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
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

/// A receipt describing a different transaction than the one requested is an
/// infrastructure failure: comparing against it would report a verdict about the
/// wrong transaction, so the target is unverified and the message names both the
/// requested and the served hash.
#[test]
fn test_verify_receipt_for_another_transaction_is_an_infrastructure_error() {
    let path = doctored_cache("wrong_tx", |receipt| {
        receipt["transactionHash"] = OTHER_TX.into();
    });

    let run = replay(&path, &["--verify-receipt", "--json", TX]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 3, "a receipt for another transaction exits 3.\nstderr: {}", run.stderr);
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = run.error_object()["error"]["message"]
        .as_str()
        .expect("the error object carries a message")
        .to_string();
    assert!(
        message.contains(OTHER_TX) && message.contains(TX),
        "the message must name both the served and the requested transaction: {message}"
    );
    assert!(
        !run.stderr.contains("verification mismatch") && !run.stdout.contains("MISMATCH"),
        "a receipt for another transaction must not be reported as a mismatch:\n{}\n{}",
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

/// In batch mode an unavailable receipt keeps the replayed result line and
/// reports the failure on `verification.error` — never as a mismatch, and never
/// by discarding the execution summary.
#[test]
fn test_batch_verify_receipt_missing_receipt_keeps_result_and_is_rpc() {
    let path = cache_without_receipt("batch_pruned");
    let list = tx_file("batch_pruned");

    let run = replay(&path, &["--tx-file", list.to_str().unwrap(), "--verify-receipt", "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 3, "an unverified target exits 3.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "one line per requested transaction");
    assert!(
        lines[0].get("error").is_none(),
        "a replayed target keeps its result line, not a bare error entry: {}",
        lines[0]
    );
    assert!(lines[0]["receipt"].is_object(), "local receipt is kept: {}", lines[0]);
    assert_eq!(lines[0]["success"].as_bool(), Some(true));
    assert_eq!(lines[0]["gas_used"].as_u64(), Some(GAS_USED));
    assert!(
        lines[0]["verification"]["error"].is_string(),
        "verification carries the unanswered receipt: {}",
        lines[0]
    );
    assert!(
        lines[0]["verification"].get("match").is_none(),
        "unavailable is not a match/mismatch verdict: {}",
        lines[0]
    );
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        !run.stderr.contains("verification mismatch"),
        "an unverifiable target must not fail as a mismatch:\n{}",
        run.stderr
    );
}

/// The reorg guard applies in batch mode too: the result is kept and the
/// divergent-inclusion failure is reported on `verification.error`.
#[test]
fn test_batch_verify_receipt_reorg_keeps_result_and_is_rpc() {
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
    assert!(lines[0].get("error").is_none(), "result line is kept: {}", lines[0]);
    assert!(
        lines[0]["verification"]["error"]
            .as_str()
            .is_some_and(|message| message.contains("different inclusion")),
        "expected the reorg/divergent-endpoint hint: {}",
        lines[0]
    );
}

/// The identity guard applies in batch mode too: the target replayed, so it
/// keeps its result line, and the failure to answer its receipt question is
/// reported on `verification.error` naming both hashes — tallied rpc once.
#[test]
fn test_batch_verify_receipt_for_another_transaction_keeps_result_and_is_rpc() {
    let path = doctored_cache("batch_wrong_tx", |receipt| {
        receipt["transactionHash"] = OTHER_TX.into();
    });
    let list = tx_file("batch_wrong_tx");

    let run = replay(&path, &["--tx-file", list.to_str().unwrap(), "--verify-receipt", "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 3, "an unverified target exits 3.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "one line per requested transaction");
    assert!(
        lines[0].get("error").is_none(),
        "a replayed target keeps its result line, not a bare error entry: {}",
        lines[0]
    );
    assert!(lines[0]["receipt"].is_object(), "local receipt is kept: {}", lines[0]);
    assert_eq!(lines[0]["success"].as_bool(), Some(true));
    assert_eq!(lines[0]["gas_used"].as_u64(), Some(GAS_USED));
    assert!(
        lines[0]["verification"]["error"]
            .as_str()
            .is_some_and(|message| message.contains(OTHER_TX) && message.contains(TX)),
        "expected both the served and the requested transaction: {}",
        lines[0]
    );
    assert!(
        lines[0]["verification"].get("match").is_none(),
        "a receipt served for another transaction is not a match/mismatch verdict: {}",
        lines[0]
    );
    let err = run.error_object();
    assert_eq!(err["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = err["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("1 of 1 target transaction(s) failed"),
        "the one unverified target is the one failure: {message}"
    );
    assert!(
        message.contains("(0 execution, 1 rpc)"),
        "an unanswered receipt question is tallied rpc, not execution: {message}"
    );
    assert!(
        !run.stderr.contains("verification mismatch"),
        "an unverifiable target must not fail as a mismatch:\n{}",
        run.stderr
    );
}

/// A receipt with a null `blockHash` cannot be anchored to the replayed block:
/// infrastructure failure, never a match/mismatch verdict.
#[test]
fn test_verify_receipt_null_block_hash_is_an_infrastructure_error() {
    let path = doctored_cache("null_block_hash", |receipt| {
        receipt["blockHash"] = serde_json::Value::Null;
    });

    let run = replay(&path, &["--verify-receipt", "--json", TX]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 3, "an unverifiable target exits 3.\nstderr: {}", run.stderr);
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        run.stderr.contains("no block hash") || run.stderr.contains("block hash"),
        "expected a missing-inclusion-hash hint, got stderr:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("verification mismatch") && !run.stdout.contains("MISMATCH"),
        "an unanchorable receipt must not be reported as a mismatch:\n{}\n{}",
        run.stdout,
        run.stderr,
    );
}

/// Batch mode reports a null `blockHash` on the kept result line as
/// `verification.error` (rpc), not as a bare error entry.
#[test]
fn test_batch_verify_receipt_null_block_hash_keeps_result_and_is_rpc() {
    let path = doctored_cache("batch_null_block_hash", |receipt| {
        receipt["blockHash"] = serde_json::Value::Null;
    });
    let list = tx_file("batch_null_block_hash");

    let run = replay(&path, &["--tx-file", list.to_str().unwrap(), "--verify-receipt", "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 3, "an unverified target exits 3.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert!(lines[0].get("error").is_none(), "result line is kept: {}", lines[0]);
    assert!(
        lines[0]["verification"]["error"]
            .as_str()
            .is_some_and(|message| message.contains("no block hash")),
        "expected a missing-inclusion-hash hint: {}",
        lines[0]
    );
    assert!(
        lines[0]["verification"].get("match").is_none(),
        "unavailable is not a match/mismatch verdict: {}",
        lines[0]
    );
}

/// Dump-dir with a nulled receipt response fails the fidelity gate as rpc on
/// the kept result line (not a silent `fidelity-gate-unavailable` skip).
#[test]
fn test_batch_dump_fixture_dir_null_receipt_is_rpc_fixture_error() {
    let path = doctored_cache("batch_dump_null_receipt", |receipt| {
        // Doctor the whole result to null by replacing the entry value below.
        let _ = receipt;
    });
    // Null the receipt response entirely (result: null), modelling a pruned
    // or unanswered eth_getTransactionReceipt.
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read doctored cache"))
            .expect("parse");
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains("cumulativeGasUsed") {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse receipt response");
        response["result"] = serde_json::Value::Null;
        entry["value"] = serde_json::Value::String(response.to_string());
    }
    std::fs::write(&path, envelope.to_string()).expect("rewrite null-receipt cache");

    let list = tx_file("batch_dump_null_receipt");
    let dir = std::env::temp_dir()
        .join(format!("mega_evme_batch_dump_null_receipt_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let run = replay(
        &path,
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(run.code(), 3, "unanswered receipt for dump exits 3.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].get("error").is_none(), "result line is kept: {}", lines[0]);
    assert!(lines[0]["receipt"].is_object(), "local receipt is kept: {}", lines[0]);
    assert_eq!(lines[0]["success"].as_bool(), Some(true));
    assert!(
        lines[0]["fixture"]["error"].is_string(),
        "fixture reports the unanswered receipt: {}",
        lines[0]
    );
    assert!(
        lines[0]["fixture"].get("skipped").is_none(),
        "receipt fetch failure is not a fidelity-gate skip: {}",
        lines[0]
    );
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// Dump-dir against an envelope that never captured the receipt is the same
/// unanswered class: rpc fixture error, result kept, exit 3.
#[test]
fn test_batch_dump_fixture_dir_missing_receipt_is_rpc_fixture_error() {
    let path = cache_without_receipt("batch_dump_no_receipt");
    let list = tx_file("batch_dump_no_receipt");
    let dir = std::env::temp_dir()
        .join(format!("mega_evme_batch_dump_no_receipt_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let run = replay(
        &path,
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(run.code(), 3, "missing receipt for dump exits 3.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].get("error").is_none(), "result line is kept: {}", lines[0]);
    assert!(lines[0]["receipt"].is_object(), "local receipt is kept: {}", lines[0]);
    assert!(
        lines[0]["fixture"]["error"].as_str().is_some_and(|m| m.contains("receipt") ||
            m.contains("not found") ||
            m.contains("cache")),
        "fixture.error names the unanswered receipt: {}",
        lines[0]
    );
    assert!(
        lines[0]["fixture"].get("skipped").is_none(),
        "missing receipt is not a silent skip: {}",
        lines[0]
    );
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// Combined `--verify-receipt --dump-fixture-dir` with a missing receipt: both
/// result fields report the failure, the shared receipt failure is counted once
/// ("1 of 1"), and the run exits 3.
#[test]
fn test_batch_verify_and_dump_missing_receipt_counted_once() {
    let path = cache_without_receipt("batch_verify_dump_no_receipt");
    let list = tx_file("batch_verify_dump_no_receipt");
    let dir = std::env::temp_dir()
        .join(format!("mega_evme_batch_verify_dump_no_receipt_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let run = replay(
        &path,
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--verify-receipt",
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(run.code(), 3, "shared missing receipt exits 3.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "one target: {}", run.stdout);
    assert!(lines[0].get("error").is_none(), "result line is kept: {}", lines[0]);
    assert!(lines[0]["receipt"].is_object(), "local receipt is kept: {}", lines[0]);
    assert!(
        lines[0]["verification"]["error"].is_string(),
        "verification.error carries the unanswered receipt: {}",
        lines[0]
    );
    assert!(
        lines[0]["fixture"]["error"].is_string(),
        "fixture.error also carries the unanswered receipt: {}",
        lines[0]
    );
    let err = run.error_object();
    let message = err["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("1 of 1 target transaction(s) failed"),
        "shared receipt failure must not double-count: {message}"
    );
    assert!(
        message.contains("1 rpc") || message.contains("(0 execution, 1 rpc)"),
        "exactly one rpc failure in the aggregate: {message}"
    );
    assert_eq!(err["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// A genuine fidelity-gate skip (local gas disagrees with on-chain receipt)
/// still exits 0: the receipt question was answered, the dump was correctly
/// refused, and skips never fail the run.
#[test]
fn test_batch_dump_fixture_dir_fidelity_mismatch_stays_skip() {
    let path = doctored_cache("batch_dump_fidelity_skip", |receipt| {
        receipt["gasUsed"] = "0x1".into();
    });
    let list = tx_file("batch_dump_fidelity_skip");
    let dir = std::env::temp_dir()
        .join(format!("mega_evme_batch_dump_fidelity_skip_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let run = replay(
        &path,
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(run.code(), 0, "a fidelity skip exits 0.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0]["fixture"]["skipped"].as_str().is_some_and(|m| m.contains("fidelity gate failed")),
        "expected fidelity-gate skip: {}",
        lines[0]
    );
    assert!(lines[0]["fixture"].get("error").is_none());
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

/// A target whose fixture could not be written is still verified against its
/// on-chain receipt: the failed dump is reported on the target's result line
/// alongside the verdict, and fails the run as an execution-class failure.
#[test]
fn test_batch_fixture_write_failure_keeps_the_receipt_verification() {
    let list = tx_file("batch_dump_verify");
    let dir =
        std::env::temp_dir().join(format!("mega_evme_batch_dump_verify_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Pre-create the fixture path so the dump is refused without --overwrite.
    std::fs::create_dir_all(&dir).expect("create dump dir");
    std::fs::write(dir.join(format!("{TX}.json")), "{}").expect("pre-create fixture file");

    let run = replay(
        &cache(),
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--verify-receipt",
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(run.code(), 1, "a failed dump exits 1.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "one line per requested transaction");
    assert!(lines[0].get("error").is_none(), "a failed dump is not an error entry: {}", lines[0]);
    assert_eq!(
        lines[0]["verification"],
        serde_json::json!({ "match": true }),
        "the verification still ran: {}",
        lines[0]
    );
    assert!(
        lines[0]["fixture"]["error"].as_str().is_some_and(|m| m.contains("already exists")),
        "the failed dump is reported on the result line: {}",
        lines[0]
    );
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("execution-error"));
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
    // The dump failed for the target, which is an execution-class failure. The
    // target still replayed, so it keeps its result line and reports the failed
    // dump on it.
    assert_eq!(second.code(), 1, "a failed dump exits 1.\nstderr: {}", second.stderr);
    let lines = second.ndjson();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].get("error").is_none(), "a failed dump is not an error entry: {}", lines[0]);
    assert!(lines[0]["receipt"].is_object(), "the replayed target keeps its receipt: {}", lines[0]);
    assert!(
        lines[0]["fixture"]["error"]
            .as_str()
            .is_some_and(|m| m.contains("already exists") && m.contains("--overwrite")),
        "expected overwrite refusal: {}",
        lines[0]
    );
    assert_eq!(second.error_object()["error"]["kind"].as_str(), Some("execution-error"));

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

/// A fixture-construction failure (database / pre-state reads) is reported as
/// `fixture.error` and fails the run — never as a silent skip that exits 0.
///
/// Classification of construction vs unsupported-shape errors is unit-tested in
/// `batch::tests::test_fixture_build_err_classifies_skips_vs_construction_errors`.
///
/// The offline State cache reuses account basics already loaded during
/// execution, so doctoring bytecode responses only kills execution. This test
/// injects a draft-time pre-state failure after execution succeeds
/// (`MEGA_EVME_INJECT_FIXTURE_PRE_STATE_ERROR`), proving the construction path
/// itself — a result line with `fixture.error` containing `construction failed`,
/// exit 1, and no skip.
#[test]
fn test_batch_fixture_construction_failure_is_fixture_error_not_skip() {
    let list = tx_file("fixture_construction");
    let dir =
        std::env::temp_dir().join(format!("mega_evme_fixture_construction_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let run = replay_with_env(
        &cache(),
        &[
            "--tx-file",
            list.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
        &[("MEGA_EVME_INJECT_FIXTURE_PRE_STATE_ERROR", "1")],
    );
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(!run.success, "construction failure must not exit 0.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert_eq!(lines.len(), 1, "one line per requested transaction");
    // Sentinel: execution reached the dump target (receipt present); only the
    // draft failed. The old code path accepted an execution-only failure here.
    assert!(
        lines[0].get("error").is_none(),
        "execution must succeed so the failure is fixture construction, not an error entry: {}",
        lines[0]
    );
    assert!(
        lines[0]["success"].as_bool() == Some(true),
        "target must have executed successfully: {}",
        lines[0]
    );
    let fixture_error = lines[0]["fixture"]["error"]
        .as_str()
        .expect("fixture.error must be set for a construction failure");
    assert!(
        fixture_error.contains("construction failed"),
        "expected construction failed, got: {fixture_error}"
    );
    assert!(
        fixture_error.contains("pre-state") || fixture_error.contains("injected"),
        "expected pre-state/injected failure detail, got: {fixture_error}"
    );
    assert!(
        lines[0]["fixture"].get("skipped").is_none(),
        "construction failure must not be reported as a skip: {}",
        lines[0]
    );
    assert_eq!(run.code(), 1, "fixture construction failure exits 1");
}

/// When the block aborts after a dump target built a Ready draft, no fixture
/// file is written and `--overwrite` does not clobber a pre-existing file.
///
/// Seeds a zero-gas object for the index-2 hash so resolve succeeds but the
/// block loop aborts on that transaction *after* the dump target (index 1)
/// has executed and built a deferred draft. Materialize runs only on a clean
/// loop, so the Ready draft is discarded. Happy-path writes are covered by
/// [`test_batch_dump_fixture_dir_writes_validatable_file`].
#[test]
fn test_batch_dump_does_not_write_or_clobber_when_block_aborts_before_finish() {
    // Index-2 hash of the captured block. Not present as a full TX object in
    // the offline capture; we inject a zero-gas type-2 call so lookup succeeds
    // and execution aborts after the dump target has drafted.
    const LATER: &str = "0xfc0a0b9d76b13125ac1e36e524f6df3a72c25720c023b960b23c6f5891be05bc";
    // `keccak256("eth_getTransactionByHash\0[\"<LATER>\"]")` — same formula as
    // `transport_cache_key` in `common/provider/transport.rs`.
    const LATER_CACHE_KEY: &str =
        "0x91bbb37d27a588e217e5be6aeab0fb377ffea0ad3a2714d1f54ceb69852124f2";

    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
            .expect("parse offline cache");
    // Clone the dump target's TX response shape and rewrite hash + gas so the
    // later index is fetchable but rejected at execution (intrinsic gas).
    let mut template: Option<serde_json::Value> = None;
    let target_marker = format!("\"hash\":\"{TX}\"");
    for entry in envelope["cache"].as_array().expect("cache entries") {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains(&target_marker) {
            continue;
        }
        let response: serde_json::Value =
            serde_json::from_str(value).expect("parse transaction response");
        if response["result"].get("hash").and_then(|h| h.as_str()) == Some(TX) {
            template = Some(response);
            break;
        }
    }
    let mut response = template.expect("offline cache must hold the dump target TX object");
    let result = response["result"].as_object_mut().expect("tx result object");
    result.insert("hash".into(), serde_json::Value::String(LATER.to_string()));
    result.insert("transactionIndex".into(), serde_json::Value::String("0x2".into()));
    result.insert("gas".into(), serde_json::Value::String("0x0".into()));
    envelope["cache"].as_array_mut().expect("cache").push(serde_json::json!({
        "key": LATER_CACHE_KEY,
        "value": response.to_string(),
    }));

    let cache_path = temp_path("dump_abort_after");
    std::fs::write(&cache_path, envelope.to_string()).expect("write doctored cache");

    let list_path = std::env::temp_dir()
        .join(format!("mega_evme_verify_dump_abort_after_{}.txt", std::process::id()));
    std::fs::write(&list_path, format!("{TX}\n{LATER}\n")).expect("write tx list");

    let dir =
        std::env::temp_dir().join(format!("mega_evme_batch_dump_abort_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dump dir");
    let fixture_path = dir.join(format!("{TX}.json"));
    let sentinel = br#"{"pre-existing":"must-not-be-clobbered"}"#;
    std::fs::write(&fixture_path, sentinel).expect("seed pre-existing fixture");

    let run = replay(
        &cache_path,
        &[
            "--tx-file",
            list_path.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--overwrite",
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&cache_path);
    let _ = std::fs::remove_file(&list_path);

    assert!(!run.success, "an aborted block must exit non-zero.\nstderr: {}", run.stderr);
    let lines = run.ndjson();
    assert!(lines.len() >= 2, "expected lines for both targets, got: {lines:?}");

    let dump_line = lines
        .iter()
        .find(|line| line["tx_hash"].as_str() == Some(TX))
        .expect("dump target must appear in the output");
    // Sentinel: the target ran and built a Ready draft that was then discarded.
    // The old test aborted before the target, so there was never a fixture report.
    assert!(
        dump_line.get("error").is_none(),
        "dump target must execute before the abort: {dump_line}"
    );
    assert!(dump_line["success"].as_bool() == Some(true), "dump target must succeed: {dump_line}");
    let fixture_error = dump_line["fixture"]["error"]
        .as_str()
        .expect("discarded Ready draft must surface as fixture.error");
    assert!(
        fixture_error.contains("discarded") || fixture_error.contains("aborted"),
        "expected draft-discarded message, got: {fixture_error}"
    );
    assert!(
        dump_line["fixture"].get("path").is_none(),
        "discarded draft must not report a written path: {dump_line}"
    );

    let kept = std::fs::read(&fixture_path).expect("pre-existing fixture must still exist");
    assert_eq!(kept, sentinel, "--overwrite must not clobber when the dump never finalizes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Replay receipts stamp each inner log with the outer receipt's block/tx
/// identity and a block-global `logIndex` that starts above zero when earlier
/// receipts in the block already emitted logs.
///
/// The single-transaction capture this file otherwise uses is log-less, so this
/// one reads the whole-block capture, whose late transactions emit logs and sit
/// behind other log-emitting receipts. `MEGA_EVME_TEST_ENVELOPE` overrides it.
#[test]
fn test_replay_receipt_inner_log_metadata_matches_outer_receipt() {
    let envelope = match std::env::var("MEGA_EVME_TEST_ENVELOPE") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => common::fixture("replay_batch_blocks.cache.json"),
    };

    // Last transaction of block 22945844: multi-log, with many preceding logs.
    const LATE_TX: &str = "0xb6a0b7a302c741f64b8e46861a3dcb2d5c1047f6f2cb89a35b5c2183c96296b7";

    let run = replay(&envelope, &["--json", LATE_TX]);
    assert!(run.success, "envelope replay must exit 0.\nstderr: {}", run.stderr);
    let summary = run.json();
    let receipt = &summary["receipt"];
    let block_hash = receipt["blockHash"].as_str().expect("receipt blockHash");
    let tx_hash = receipt["transactionHash"].as_str().expect("receipt transactionHash");
    let tx_index = receipt["transactionIndex"].as_str().expect("receipt transactionIndex");
    assert_eq!(tx_hash, LATE_TX);
    let logs = receipt["logs"].as_array().expect("receipt logs array");
    assert!(!logs.is_empty(), "late envelope tx must emit logs (got empty); envelope may be stale");
    // Sentinel: preceding receipts emitted logs, so the first log_index is > 0.
    let first_log_index = u64::from_str_radix(
        logs[0]["logIndex"].as_str().expect("logIndex string").trim_start_matches("0x"),
        16,
    )
    .expect("parse logIndex");
    assert!(
        first_log_index > 0,
        "expected non-zero preceding-log offset, got logIndex={first_log_index}"
    );
    for (i, log) in logs.iter().enumerate() {
        assert_eq!(log["blockHash"].as_str(), Some(block_hash), "log {i} blockHash");
        assert_eq!(log["transactionHash"].as_str(), Some(tx_hash), "log {i} transactionHash");
        assert_eq!(log["transactionIndex"].as_str(), Some(tx_index), "log {i} transactionIndex");
        assert!(log["logIndex"].is_string(), "log {i} must carry logIndex: {log}");
    }

    // Batch path stamps the same fields.
    let list_path = std::env::temp_dir()
        .join(format!("mega_evme_verify_batch_log_meta_{}.txt", std::process::id()));
    std::fs::write(&list_path, format!("{LATE_TX}\n")).expect("write tx list");
    let batch = replay(&envelope, &["--tx-file", list_path.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&list_path);
    assert!(batch.success, "batch replay must exit 0.\nstderr: {}", batch.stderr);
    let line = &batch.ndjson()[0];
    let batch_receipt = &line["receipt"];
    assert_eq!(batch_receipt["blockHash"].as_str(), Some(block_hash));
    assert_eq!(batch_receipt["transactionHash"].as_str(), Some(tx_hash));
    let batch_logs = batch_receipt["logs"].as_array().expect("batch receipt logs");
    assert!(!batch_logs.is_empty(), "batch path must also carry non-empty logs");
    let batch_first = u64::from_str_radix(
        batch_logs[0]["logIndex"].as_str().expect("logIndex").trim_start_matches("0x"),
        16,
    )
    .expect("parse batch logIndex");
    assert!(batch_first > 0, "batch logIndex must start above zero, got {batch_first}");
    for (i, log) in batch_logs.iter().enumerate() {
        assert_eq!(log["blockHash"].as_str(), Some(block_hash), "batch log {i} blockHash");
        assert_eq!(log["transactionHash"].as_str(), Some(tx_hash), "batch log {i} transactionHash");
        assert!(log["logIndex"].is_string(), "batch log {i} must carry logIndex: {log}");
    }
}
