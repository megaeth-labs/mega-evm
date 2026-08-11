//! Offline integration tests for `mega-evme replay --block` / `--tx-file`.
//!
//! Replaying whole blocks needs an RPC capture covering every transaction of
//! each block, which is far larger than a single-transaction capture. The
//! envelope is committed as a gzipped archive and extracted into a temporary
//! directory once per test binary, so these run in CI without setup. They are
//! the only tests that exercise the batch driver's multi-target paths: more
//! than one target in a block, whole-block mode, grouping targets across
//! blocks, sweeping targets on both sides of a mid-block abort, and
//! block-global log indexing against real logs.
//!
//! Set `MEGA_EVME_TEST_ENVELOPE` to replay against a different capture instead.
//!
//! To regenerate the archive, capture both blocks into one envelope (the second
//! run merges into the first) and repack it. `--verify-receipt` is what puts the
//! receipts in the capture, which the verification and fixture-dump tests need:
//!
//! ```bash
//! for block in 22945844 22945853; do
//!   mega-evme replay --rpc <URL> --rpc.capture-file replay_batch_blocks.cache.json \
//!     --block "$block" --verify-receipt --json
//! done
//! tar -czf replay_batch_blocks.cache.json.tar.gz replay_batch_blocks.cache.json
//! ```
//!
//! The endpoint must serve state at those blocks; a pruning node fails every
//! target with "state at block #N is pruned".

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

/// A mid-block type-0x2 call (not a deposit): zeroing its gas makes the block
/// executor reject the transaction as invalid and abort — unlike deposits,
/// which can still halt as `FailedDeposit` without aborting the block.
const EXEC_ABORT_TX: (&str, u64) =
    ("0xa637d68cda9423d67826e008b1c90295193f30f19cd74a6f4acf54022d56cae2", 2);

/// Last transaction of the envelope's second block.
const OTHER_BLOCK: u64 = 22_945_853;
const OTHER_BLOCK_TX: &str = "0x18302160f2395069a44e1654d173fa9eed95ead8f922f12bfe07b6bdcc0a14f2";
const OTHER_BLOCK_TX_INDEX: u64 = 23;

/// Path of the offline envelope.
///
/// `MEGA_EVME_TEST_ENVELOPE` overrides the committed capture with another one.
fn envelope() -> String {
    if let Ok(path) = std::env::var("MEGA_EVME_TEST_ENVELOPE") {
        return path;
    }
    common::fixture(ENVELOPE_NAME).display().to_string()
}

/// Name of the committed capture, stored compressed alongside the other fixtures.
const ENVELOPE_NAME: &str = "replay_batch_blocks.cache.json";

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

/// Write a copy of the envelope whose `eth_getTransactionByHash` response for
/// `tx_hash` still returns the transaction object, but with `gas` set to `0x0`
/// so execution/setup fails (intrinsic gas / validation) rather than a missing
/// lookup. Models an executor abort mid-block.
fn envelope_with_zero_gas_transaction(name: &str, tx_hash: &str) -> std::path::PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let marker = format!("\"hash\":\"{tx_hash}\"");
    let mut doctored = 0;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains(&marker) {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse transaction response");
        let result = response.get_mut("result").expect("transaction result");
        assert!(result.is_object(), "expected a transaction object for {tx_hash}");
        result["gas"] = serde_json::Value::String("0x0".into());
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
/// `--block N --verify-receipt` verifies every transaction of the block against
/// its on-chain receipt and exits 0 when they all reproduce.
///
/// This is the multi-target verification fan-out: one receipt fetch and one
/// verdict per target, each carried on that target's own result line.
#[test]
fn test_replay_block_verify_receipt_reports_a_verdict_per_target() {
    let (stdout, code) =
        replay_with_code(&["--block", &BLOCK.to_string(), "--verify-receipt", "--json"]);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), BLOCK_TX_COUNT, "every target is reported exactly once");
    for line in &lines {
        assert!(line.get("error").is_none(), "a verified target is not an error entry: {line}");
        assert_eq!(
            line["verification"]["match"].as_bool(),
            Some(true),
            "every target must reproduce its on-chain receipt: {line}"
        );
    }
    assert_eq!(code, Some(0), "a fully matching run exits 0");
}

/// An abort caused by one transaction of the block is not an answer about the
/// targets behind it: only the transaction the endpoint denied is reported as
/// `not_found`, and every target swept up behind it is reported as unanswered
/// (`rpc`) with a message naming the transaction that aborted the block.
#[test]
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

/// An executor/setup abort on a mid-block transaction is still an execution
/// failure for that transaction only: every target behind it is unanswered
/// (`rpc`), not blamed as execution.
///
/// Doctors the envelope so a mid-block type-0x2 call has gas `0x0` — the lookup
/// succeeds, but the block executor rejects it as an invalid transaction
/// (intrinsic/call gas) and aborts the block — an execution-class error, not
/// `TransactionNotFound`.
#[test]
fn test_replay_block_sweeps_targets_behind_execution_abort_as_rpc() {
    let (aborting, aborting_index) = EXEC_ABORT_TX;
    let path = envelope_with_zero_gas_transaction("exec_abort_block", aborting);

    let (stdout, code) =
        replay_envelope_with_code(&path, &["--block", &BLOCK.to_string(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), BLOCK_TX_COUNT, "every target is still reported exactly once");
    for (index, line) in lines.iter().enumerate() {
        let index = index as u64;
        if index < aborting_index {
            assert!(line.get("error").is_none(), "targets before the abort replay: {line}");
            continue;
        }
        if index == aborting_index {
            assert_eq!(
                line["error"]["kind"].as_str(),
                Some("execution"),
                "the aborting transaction keeps its own execution kind: {line}"
            );
            continue;
        }
        assert_eq!(
            line["error"]["kind"].as_str(),
            Some("rpc"),
            "a target swept up behind an execution abort went unanswered: {line}"
        );
        assert!(
            line["error"]["message"].as_str().is_some_and(|m| {
                m.contains(aborting) || m.contains("aborted") || m.contains("Block replay")
            }),
            "the message must name the abort cause: {line}"
        );
    }

    // The aborting transaction is an execution-class failure, which outranks
    // the unanswered ones.
    assert_eq!(code, Some(1), "a definitive execution failure exits 1");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("execution-error"));
}

/// Targets swept up by an abort are reported in block transaction-index order,
/// whatever order `--tx-file` listed them in.
#[test]
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

/// A parent block whose hash does not match the child block's `parentHash` is an
/// A `--tx-file` target whose reported inclusion block is not the block fetched
/// by that number is unanswered, not replayed.
///
/// The endpoint answers `eth_getTransactionByHash` and `eth_getBlockByNumber`
/// separately, so a reorg or a load-balanced backend can serve two views. The
/// resolution step records the inclusion hash so the mismatch is caught before
/// the block runs, instead of replaying targets against a block they are not in.
#[test]
fn test_replay_tx_file_rejects_a_block_that_does_not_match_the_resolved_inclusion() {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let wrong_hash = "0x2222222222222222222222222222222222222222222222222222222222222222";
    let (target, _) = BLOCK_TXS[1];

    // Rewrite only the transaction's own response: it now claims to belong to a
    // block whose hash differs from the one `eth_getBlockByNumber` returns.
    let marker = format!("\"hash\":\"{target}\"");
    let mut doctored = 0;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains(&marker) {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse transaction response");
        let result = response.get_mut("result").expect("transaction result");
        assert!(result.is_object(), "expected a transaction object");
        result["blockHash"] = serde_json::Value::String(wrong_hash.into());
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "exactly one response describes the target transaction");

    let envelope_path =
        std::env::temp_dir().join(format!("mega_evme_batch_inclusion_{}.json", std::process::id()));
    std::fs::write(&envelope_path, envelope.to_string()).expect("write doctored envelope");
    let list = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_inclusion_{}.txt", std::process::id()));
    std::fs::write(&list, format!("{target}\n")).expect("write tx list");

    let (stdout, code) =
        replay_envelope_with_code(&envelope_path, &["--tx-file", list.to_str().unwrap(), "--json"]);
    let lines = ndjson(&stdout);
    assert_eq!(lines.len(), 1, "the single target is reported once: {stdout}");
    assert_eq!(
        lines[0]["error"]["kind"].as_str(),
        Some("rpc"),
        "divergent views are unanswered, not a wrong answer: {}",
        lines[0]
    );
    let message = lines[0]["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("divergent views"), "message names the cause: {message}");
    assert_eq!(code, Some(3), "an unanswered target exits 3");

    let _ = std::fs::remove_file(&envelope_path);
    let _ = std::fs::remove_file(&list);
}

/// A mined `--tx-file` target whose `eth_getTransactionByHash` answer carries a
/// block number but no inclusion hash is unanswered: the endpoint served an
/// unanchored view, so the target is not queued and other blocks still replay.
#[test]
fn test_replay_tx_file_rejects_mined_target_without_inclusion_hash() {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let (target, _) = BLOCK_TXS[1];

    // Keep the block number so the response still looks mined, but drop the
    // inclusion hash. Cache entries are keyed by the request, so the doctored
    // answer still resolves.
    let marker = format!("\"hash\":\"{target}\"");
    let mut doctored = 0;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains(&marker) {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse transaction response");
        let result = response.get_mut("result").expect("transaction result");
        assert!(result.is_object(), "expected a transaction object");
        assert!(
            result.get("blockNumber").is_some_and(|n| !n.is_null()),
            "fixture transaction must report a block number"
        );
        result["blockHash"] = serde_json::Value::Null;
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "exactly one response describes the target transaction");

    let envelope_path = std::env::temp_dir()
        .join(format!("mega_evme_batch_null_inclusion_{}.json", std::process::id()));
    std::fs::write(&envelope_path, envelope.to_string()).expect("write doctored envelope");
    // Pair the unanchored target with one from another block so a clean job
    // still runs when resolution fails for only one hash.
    let list = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_null_inclusion_{}.txt", std::process::id()));
    std::fs::write(&list, format!("{target}\n{OTHER_BLOCK_TX}\n")).expect("write tx list");

    let (stdout, code) =
        replay_envelope_with_code(&envelope_path, &["--tx-file", list.to_str().unwrap(), "--json"]);
    let lines = ndjson(&stdout);
    assert_eq!(lines.len(), 2, "every target is reported once: {stdout}");

    let failed = lines
        .iter()
        .find(|line| line["tx_hash"].as_str() == Some(target))
        .expect("doctored target must be reported");
    assert_eq!(
        failed["error"]["kind"].as_str(),
        Some("rpc"),
        "a mined target without an inclusion hash is unanswered: {failed}"
    );
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("inclusion hash") && message.contains("unanchored"),
        "message names the unanchored view: {message}"
    );

    let ok = lines
        .iter()
        .find(|line| line["tx_hash"].as_str() == Some(OTHER_BLOCK_TX))
        .expect("other-block target must be reported");
    assert!(ok.get("error").is_none(), "targets in other blocks still replay: {ok}");
    assert_eq!(ok["block_number"].as_u64(), Some(OTHER_BLOCK));
    assert_eq!(ok["success"].as_bool(), Some(true));

    assert_eq!(code, Some(3), "an unanswered target exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));

    let _ = std::fs::remove_file(&envelope_path);
    let _ = std::fs::remove_file(&list);
}

/// infrastructure failure for every target of that block (reorg / divergent views).
#[test]
fn test_replay_block_rejects_mismatched_parent_hash() {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let wrong_parent = "0x1111111111111111111111111111111111111111111111111111111111111111";
    let mut expected_parent = None;
    let mut doctored = 0;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        let Ok(mut response) = serde_json::from_str::<serde_json::Value>(value) else {
            continue;
        };
        let Some(result) = response.get_mut("result") else {
            continue;
        };
        if !result.is_object() {
            continue;
        }
        // Doctor the parent block (number == BLOCK - 1), not the target block.
        let number = result.get("number").and_then(|n| {
            n.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        });
        if number != Some(BLOCK - 1) {
            continue;
        }
        let original = result.get("hash").and_then(|h| h.as_str()).map(str::to_string);
        assert!(original.is_some(), "parent block must report a hash");
        expected_parent = original;
        result["hash"] = serde_json::Value::String(wrong_parent.into());
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "the envelope must hold exactly one parent-block body for {BLOCK}");
    let expected_parent = expected_parent.expect("parent hash");

    let path = std::env::temp_dir()
        .join(format!("mega_evme_batch_parent_mismatch_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write doctored envelope");

    let (stdout, code) =
        replay_envelope_with_code(&path, &["--block", &BLOCK.to_string(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), BLOCK_TX_COUNT, "every target is still reported exactly once");
    for line in &lines {
        assert_eq!(
            line["error"]["kind"].as_str(),
            Some("rpc"),
            "a parent/block linkage failure is an infrastructure error: {line}"
        );
        let message = line["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.contains(wrong_parent) && message.contains(&expected_parent),
            "the message must name both hashes (got parent {expected_parent}, wrong {wrong_parent}): {line}"
        );
    }
    assert_eq!(code, Some(3), "an infrastructure failure exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// Receipts from a late multi-log transaction stamp every inner log with the
/// outer block/tx identity and a block-global `logIndex` that starts above zero
/// (preceding receipts already emitted logs).
#[test]
fn test_replay_receipt_inner_log_metadata_nonzero_preceding_offset() {
    // Last transaction of BLOCK: multi-log, with many preceding logs in-block.
    const LATE_TX: &str = "0xb6a0b7a302c741f64b8e46861a3dcb2d5c1047f6f2cb89a35b5c2183c96296b7";

    let stdout = replay(&["--json", LATE_TX], true);
    let summary = common::json_values(&stdout)
        .into_iter()
        .find(|v| v.get("receipt").is_some())
        .expect("replay summary with receipt");
    let receipt = &summary["receipt"];
    let block_hash = receipt["blockHash"].as_str().expect("blockHash");
    let tx_hash = receipt["transactionHash"].as_str().expect("transactionHash");
    let logs = receipt["logs"].as_array().expect("logs");
    assert!(!logs.is_empty(), "late tx must emit logs");
    let first = u64::from_str_radix(
        logs[0]["logIndex"].as_str().expect("logIndex").trim_start_matches("0x"),
        16,
    )
    .expect("parse logIndex");
    assert!(first > 0, "expected non-zero preceding-log offset, got {first}");
    for (i, log) in logs.iter().enumerate() {
        assert_eq!(log["blockHash"].as_str(), Some(block_hash), "log {i}");
        assert_eq!(log["transactionHash"].as_str(), Some(tx_hash), "log {i}");
        assert!(log["logIndex"].is_string(), "log {i} logIndex");
    }
}

/// Sweeping a block with `--dump-fixture-dir` against an envelope that carries
/// no receipts skips every target on the fidelity gate and still exits 0.
///
/// Fixture skips are expected (not infrastructure failures); the development
/// `--block N --dump-fixture-dir` writes a fixture for every transaction it can
/// express and skips the ones it cannot, without failing the run.
///
/// Every OP-stack block opens with a deposit, which the fixture format cannot
/// represent. Reporting that as an error rather than a skip would make a
/// whole-block sweep exit non-zero on every block, so this pins the
/// classification end to end: 22 files written, the deposit skipped with its
/// reason, nothing reported as an error, and exit 0.
#[test]
fn test_replay_block_dump_fixture_dir_writes_all_but_the_deposit() {
    let dir = std::env::temp_dir()
        .join(format!("mega_evme_dump_dir_sweep_{}_{BLOCK}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = replay(
        &["--block", &BLOCK.to_string(), "--dump-fixture-dir", dir.to_str().unwrap(), "--json"],
        true,
    );
    let lines = ndjson(&stdout);
    assert_eq!(lines.len(), BLOCK_TX_COUNT, "every target is reported exactly once");

    let mut written = 0;
    let mut skipped = Vec::new();
    for line in &lines {
        assert!(line.get("error").is_none(), "a sweep must not produce error entries: {line}");
        if line["fixture"]["path"].is_string() {
            written += 1;
        } else {
            skipped.push(
                line["fixture"]["skipped"]
                    .as_str()
                    .unwrap_or_else(|| panic!("a line reported neither path nor skip: {line}"))
                    .to_string(),
            );
        }
    }

    assert_eq!(skipped.len(), 1, "only the index-0 deposit is unsupported: {skipped:?}");
    assert!(
        skipped[0].contains("does not support deposit"),
        "the skip must name the reason: {}",
        skipped[0]
    );
    assert_eq!(written, BLOCK_TX_COUNT - 1, "every other transaction is dumped");

    let on_disk = std::fs::read_dir(&dir).expect("read dump dir").count();
    assert_eq!(on_disk, written, "each reported path is a file on disk");
    let _ = std::fs::remove_dir_all(&dir);
}
