//! Offline integration tests for `mega-evme replay --block` / `--tx-file`.
//!
//! These run against an RPC capture envelope large enough to replay whole
//! blocks, which is too big to commit; point `MEGA_EVME_TEST_ENVELOPE` at one
//! and run them explicitly:
//!
//! ```bash
//! MEGA_EVME_TEST_ENVELOPE=<path> cargo test -p mega-evme -- --ignored
//! ```
//!
//! They are `#[ignore]`d so CI, which has no envelope, skips them.

use std::process::Command;

mod common;

/// Block fully covered by the envelope, and its transaction count.
const BLOCK: u64 = 22_945_844;
const BLOCK_TX_COUNT: usize = 23;

/// Sample transactions of `BLOCK`: the index-0 deposit, a mid-block call, and
/// the last transaction.
const BLOCK_TXS: [(&str, u64); 3] = [
    ("0xde3d56dc739484166b8af1bea757bf7e3e9a4b9a0fb62d722703345570dfc1d6", 0),
    ("0x323ddc8e67dfc134284d78c65f3c1dc7ff45ba1db02eeaf62e211ae3253478ef", 3),
    ("0xb6a0b7a302c741f64b8e46861a3dcb2d5c1047f6f2cb89a35b5c2183c96296b7", 22),
];

/// Last transaction of the envelope's second block.
const OTHER_BLOCK: u64 = 22_945_853;
const OTHER_BLOCK_TX: &str = "0x18302160f2395069a44e1654d173fa9eed95ead8f922f12bfe07b6bdcc0a14f2";
const OTHER_BLOCK_TX_INDEX: u64 = 23;

/// Path of the offline envelope, or a skip message when it is not configured.
fn envelope() -> String {
    std::env::var("MEGA_EVME_TEST_ENVELOPE").expect(
        "set MEGA_EVME_TEST_ENVELOPE to an RPC capture covering the replayed blocks; \
         these tests are #[ignore]d precisely because that envelope is not committed",
    )
}

fn mega_evme() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mega-evme"))
}

/// Run `replay` offline and return its stdout, asserting the exit status.
fn replay(args: &[&str], expect_success: bool) -> String {
    let envelope = envelope();
    let mut cmd = mega_evme();
    cmd.args(["replay", "--rpc.replay-file", &envelope]);
    cmd.args(args);
    let output = cmd.output().expect("failed to run mega-evme");
    assert_eq!(
        output.status.success(),
        expect_success,
        "unexpected exit status for {args:?}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("stdout is utf-8")
}

/// Run `replay` offline and return its stdout plus its exit code.
fn replay_with_code(args: &[&str]) -> (String, Option<i32>) {
    let envelope = envelope();
    let mut cmd = mega_evme();
    cmd.args(["replay", "--rpc.replay-file", &envelope]);
    cmd.args(args);
    let output = cmd.output().expect("failed to run mega-evme");
    (String::from_utf8(output.stdout).expect("stdout is utf-8"), output.status.code())
}

/// Write a copy of the envelope whose `eth_getTransactionByHash` response for
/// `tx_hash` answers "unknown transaction", and return its path.
///
/// Entries are keyed by the request, not the response, so the doctored answer
/// still resolves. This models the endpoint losing one transaction of a block it
/// still serves — the block body lists the hash, the lookup denies it.
fn envelope_without_transaction(name: &str, tx_hash: &str) -> std::path::PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    // Only the transaction's own response carries it as the `hash` field; the
    // block body lists bare hashes and a receipt names it `transactionHash`.
    let marker = format!("\"hash\":\"{tx_hash}\"");
    let mut doctored = 0;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains(&marker) {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse transaction response");
        response["result"] = serde_json::Value::Null;
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "the envelope must hold exactly one response for {tx_hash}");

    let path =
        std::env::temp_dir().join(format!("mega_evme_batch_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write doctored envelope");
    path
}

/// Run `replay` against `envelope_path` and return its stdout plus its exit code.
fn replay_envelope_with_code(
    envelope_path: &std::path::Path,
    args: &[&str],
) -> (String, Option<i32>) {
    let mut cmd = mega_evme();
    cmd.args(["replay", "--rpc.replay-file", envelope_path.to_str().expect("path is utf-8")]);
    cmd.args(args);
    let output = cmd.output().expect("failed to run mega-evme");
    (String::from_utf8(output.stdout).expect("stdout is utf-8"), output.status.code())
}

/// Parse NDJSON stdout into one JSON value per line, dropping the structured
/// error object a failing run ends with.
fn ndjson(stdout: &str) -> Vec<serde_json::Value> {
    let mut lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| {
            assert!(!line.trim().is_empty(), "NDJSON output must not contain blank lines");
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("stdout line is not compact JSON ({e}): {line}"))
        })
        .collect();
    if lines.last().is_some_and(common::is_run_error) {
        lines.pop();
    }
    lines
}

/// The structured error object a failing `--json` run ends with.
fn run_error(stdout: &str) -> serde_json::Value {
    let last = stdout.lines().last().unwrap_or_else(|| panic!("stdout must not be empty"));
    let value: serde_json::Value = serde_json::from_str(last)
        .unwrap_or_else(|e| panic!("last stdout line is not compact JSON ({e}): {last}"));
    assert!(common::is_run_error(&value), "the last line must be the error object: {value}");
    value
}

/// `--block N --json` emits exactly one NDJSON line per transaction of the
/// block, in transaction order, and exits 0.
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_block_emits_one_ndjson_line_per_transaction() {
    let stdout = replay(&["--block", &BLOCK.to_string(), "--json"], true);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), BLOCK_TX_COUNT, "expected one line per transaction of the block");
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(
            line["block_number"].as_u64(),
            Some(BLOCK),
            "every line must report the replayed block: {line}"
        );
        assert_eq!(
            line["tx_index"].as_u64(),
            Some(index as u64),
            "lines must be ordered by transaction index: {line}"
        );
        assert!(line["tx_hash"].is_string(), "line must carry the transaction hash: {line}");
        assert!(line["receipt"].is_object(), "line must carry the receipt: {line}");
        assert!(line.get("error").is_none(), "line must not be an error entry: {line}");
        // Batch mode rejects the trace/dump flags, so those fields never appear.
        assert!(line.get("trace").is_none(), "batch output must carry no trace: {line}");
        assert!(line.get("state").is_none(), "batch output must carry no state dump: {line}");
    }
}

/// A batch line and a single-transaction replay of the same transaction must
/// agree on the execution outcome.
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_batch_matches_single_transaction_replay() {
    let batch = ndjson(&replay(&["--block", &BLOCK.to_string(), "--json"], true));

    for (tx_hash, tx_index) in BLOCK_TXS {
        let single: serde_json::Value = serde_json::from_str(&replay(&["--json", tx_hash], true))
            .expect("single-transaction output is JSON");
        let line = batch
            .iter()
            .find(|line| line["tx_hash"] == tx_hash)
            .unwrap_or_else(|| panic!("batch output is missing {tx_hash}"));

        assert_eq!(line["tx_index"].as_u64(), Some(tx_index), "wrong index for {tx_hash}");
        for field in ["success", "gas_used", "logs_count"] {
            assert_eq!(
                line[field], single[field],
                "batch and single-transaction replay disagree on {field} for {tx_hash}",
            );
        }
    }
}

/// `--tx-file` replays transactions from several blocks in one process and
/// reports them ordered by (block, transaction index).
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_tx_file_spans_blocks_in_order() {
    // Deliberately unordered, with a comment, a blank line, and a duplicate.
    let list = format!(
        "# sample corpus\n{OTHER_BLOCK_TX}\n\n{}\n  {}\n{}\n{}\n",
        BLOCK_TXS[2].0, BLOCK_TXS[0].0, BLOCK_TXS[1].0, BLOCK_TXS[0].0,
    );
    let path = std::env::temp_dir().join(format!("mega_evme_tx_list_{}.txt", std::process::id()));
    std::fs::write(&path, list).expect("write tx list");

    let stdout = replay(&["--tx-file", path.to_str().unwrap(), "--json"], true);
    let _ = std::fs::remove_file(&path);
    let lines = ndjson(&stdout);

    let observed: Vec<(u64, u64, &str)> = lines
        .iter()
        .map(|line| {
            (
                line["block_number"].as_u64().expect("block number"),
                line["tx_index"].as_u64().expect("transaction index"),
                line["tx_hash"].as_str().expect("transaction hash"),
            )
        })
        .collect();
    let expected: Vec<(u64, u64, &str)> = vec![
        (BLOCK, BLOCK_TXS[0].1, BLOCK_TXS[0].0),
        (BLOCK, BLOCK_TXS[1].1, BLOCK_TXS[1].0),
        (BLOCK, BLOCK_TXS[2].1, BLOCK_TXS[2].0),
        (OTHER_BLOCK, OTHER_BLOCK_TX_INDEX, OTHER_BLOCK_TX),
    ];

    assert_eq!(observed, expected, "results must be ordered by (block, transaction index)");
}

/// A hash that cannot be resolved is reported as an error entry, the remaining
/// targets still replay, and the process exits non-zero with the class of the
/// failure — here an unanswered lookup against the offline envelope.
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_tx_file_reports_unresolved_targets_and_exits_nonzero() {
    let unknown = "0x0000000000000000000000000000000000000000000000000000000000000001";
    let path =
        std::env::temp_dir().join(format!("mega_evme_tx_list_bad_{}.txt", std::process::id()));
    std::fs::write(&path, format!("{unknown}\n{}\n", BLOCK_TXS[1].0)).expect("write tx list");

    let (stdout, code) = replay_with_code(&["--tx-file", path.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), 2, "every target gets exactly one line, including failures");
    assert_eq!(lines[0]["tx_hash"].as_str(), Some(unknown));
    assert!(lines[0]["error"]["kind"].is_string(), "failure line carries an error kind");
    assert!(lines[0]["error"]["message"].is_string(), "failure line carries a message");
    assert_eq!(lines[1]["tx_hash"].as_str(), Some(BLOCK_TXS[1].0));
    assert_eq!(lines[1]["success"].as_bool(), Some(true), "the resolvable target still replays");

    // A hash the envelope cannot answer is an RPC-class failure for the run.
    assert_eq!(code, Some(3), "an unanswered target exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// `--verify-receipt` against an envelope that carries no receipts: every target
/// becomes an `rpc` error entry (unverified), never a mismatch, and the run
/// exits non-zero.
///
/// The development envelope is captured by replays, which do not fetch receipts,
/// so this pins the endpoint-cannot-serve-the-receipt path end to end.
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_block_verify_receipt_without_receipts_reports_rpc_errors() {
    let (stdout, code) =
        replay_with_code(&["--block", &BLOCK.to_string(), "--verify-receipt", "--json"]);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), BLOCK_TX_COUNT, "every target is still reported exactly once");
    for line in &lines {
        assert_eq!(
            line["error"]["kind"].as_str(),
            Some("rpc"),
            "a receipt the envelope cannot serve is an infrastructure error: {line}"
        );
        assert!(line.get("verification").is_none(), "an unverified target carries no verdict");
    }

    // Unverified targets are RPC-class failures, never mismatches.
    assert_eq!(code, Some(3), "a run of unverified targets exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// An abort caused by one transaction of the block is not an answer about the
/// targets behind it: only the transaction the endpoint denied is reported as
/// `not_found`, and every target swept up behind it is reported as unanswered
/// (`rpc`) with a message naming the transaction that aborted the block.
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_block_sweeps_targets_behind_an_abort_as_unanswered() {
    let (missing, missing_index) = BLOCK_TXS[1];
    let path = envelope_without_transaction("abort_block", missing);

    let (stdout, code) =
        replay_envelope_with_code(&path, &["--block", &BLOCK.to_string(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), BLOCK_TX_COUNT, "every target is still reported exactly once");
    for (index, line) in lines.iter().enumerate() {
        let index = index as u64;
        if index < missing_index {
            assert!(line.get("error").is_none(), "targets before the abort replay: {line}");
            continue;
        }
        if index == missing_index {
            assert_eq!(
                line["error"]["kind"].as_str(),
                Some("not_found"),
                "only the denied transaction is unknown: {line}"
            );
            continue;
        }
        assert_eq!(
            line["error"]["kind"].as_str(),
            Some("rpc"),
            "a target swept up behind the abort went unanswered: {line}"
        );
        assert!(
            line["error"]["message"].as_str().is_some_and(|m| m.contains(missing)),
            "the message must name the transaction that aborted the block: {line}"
        );
    }

    // The denied transaction is an execution-class failure, which outranks the
    // unanswered ones.
    assert_eq!(code, Some(1), "a definitive negative answer exits 1");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("execution-error"));
}

/// Targets swept up by an abort are reported in block transaction-index order,
/// whatever order `--tx-file` listed them in.
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_tx_file_sweeps_targets_in_block_order() {
    let missing = BLOCK_TXS[0].0;
    let path = envelope_without_transaction("abort_order", missing);
    // Deliberately reversed: the last transaction of the block first.
    let list = format!("{}\n{}\n", BLOCK_TXS[2].0, BLOCK_TXS[1].0);
    let list_path =
        std::env::temp_dir().join(format!("mega_evme_tx_list_order_{}.txt", std::process::id()));
    std::fs::write(&list_path, list).expect("write tx list");

    let (stdout, code) =
        replay_envelope_with_code(&path, &["--tx-file", list_path.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list_path);
    let lines = ndjson(&stdout);

    let observed: Vec<&str> = lines.iter().map(|line| line["tx_hash"].as_str().unwrap()).collect();
    assert_eq!(
        observed,
        vec![BLOCK_TXS[1].0, BLOCK_TXS[2].0],
        "swept targets must follow the block's transaction order, not the input order",
    );
    for line in &lines {
        assert_eq!(line["error"]["kind"].as_str(), Some("rpc"), "swept target: {line}");
        assert!(
            line["error"]["message"].as_str().is_some_and(|m| m.contains(missing)),
            "the message must name the transaction that aborted the block: {line}"
        );
    }

    // No target was answered definitively, so the run is an RPC failure.
    assert_eq!(code, Some(3), "targets that went unanswered exit 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// Batch mode rejects the single-transaction-only flags before doing any work.
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_batch_rejects_single_transaction_flags() {
    let envelope = envelope();
    for (extra, expected) in [
        (vec!["--dump-fixture", "/tmp/should-not-exist.json"], "--dump-fixture"),
        (vec!["--override.gas-limit", "50000"], "transaction overrides"),
        (vec!["--override.spec", "Rex4"], "--override.spec"),
        (vec!["--trace"], "trace options"),
        (vec!["--dump"], "state dump options"),
    ] {
        let mut cmd = mega_evme();
        cmd.args(["replay", "--rpc.replay-file", &envelope, "--block", &BLOCK.to_string()]);
        cmd.args(&extra);
        let output = cmd.output().expect("failed to run mega-evme");

        assert!(!output.status.success(), "batch mode must reject {extra:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected) && stderr.contains("batch replay"),
            "unexpected error for {extra:?}: {stderr}"
        );
        assert!(output.stdout.is_empty(), "a rejected batch run must print nothing on stdout");
    }
}

/// Sweeping a block with `--dump-fixture-dir` against an envelope that carries
/// no receipts skips every target on the fidelity gate and still exits 0.
///
/// Fixture skips are expected (not infrastructure failures); the development
/// envelope is captured without receipts, so this pins the skip path end to end.
#[test]
#[ignore = "requires MEGA_EVME_TEST_ENVELOPE"]
fn test_replay_block_dump_fixture_dir_skips_without_receipts() {
    let dir = std::env::temp_dir().join(format!(
        "mega_evme_dump_dir_skip_{}_{}",
        std::process::id(),
        BLOCK
    ));
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = replay(
        &["--block", &BLOCK.to_string(), "--dump-fixture-dir", dir.to_str().unwrap(), "--json"],
        true,
    );
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), BLOCK_TX_COUNT, "every target is still reported exactly once");
    for line in &lines {
        assert!(line.get("error").is_none(), "skips must not turn into error entries: {line}");
        let skipped = line["fixture"]["skipped"]
            .as_str()
            .unwrap_or_else(|| panic!("every line must carry a fixture skip reason: {line}"));
        assert!(
            skipped.contains("fidelity-gate-unavailable"),
            "expected fidelity-gate-unavailable skip, got: {skipped}"
        );
        assert!(line["fixture"].get("path").is_none(), "a skip must not report a path: {line}");
    }

    // No fixtures written: the directory may exist (create_dir_all) but be empty.
    if dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&dir).expect("read dump dir").collect();
        assert!(entries.is_empty(), "fidelity skips must write no fixture files");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
