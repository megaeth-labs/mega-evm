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

/// Non-target body transaction between `BLOCK_TXS[1]` (index 3) and
/// `BLOCK_TXS[2]` (index 22). Used to abort after an early dumpable target
/// without putting the aborter on the reported target list.
const MID_BLOCK_NON_TARGET: (&str, u64) =
    ("0x63e032fdff2676824fd6a71df09d88f62146d07effc7e4ed7e246034df2e9b22", 4);

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

/// Write a copy of the envelope with the `eth_getTransactionByHash` entry for
/// `tx_hash` removed entirely, and return its path.
///
/// Distinct from [`envelope_without_transaction`]: there the endpoint answers
/// "no such transaction", here it does not answer at all — an offline cache
/// miss, which is how a transport failure reaches the same call site.
fn envelope_dropping_transaction(name: &str, tx_hash: &str) -> std::path::PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let marker = format!("\"hash\":\"{tx_hash}\"");
    let entries = envelope["cache"].as_array_mut().expect("cache entries");
    let before = entries.len();
    entries.retain(|entry| {
        !entry["value"].as_str().expect("entry value is a string").contains(&marker)
    });
    assert_eq!(
        before - entries.len(),
        1,
        "the envelope must hold exactly one response for {tx_hash}"
    );

    let path =
        std::env::temp_dir().join(format!("mega_evme_batch_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write doctored envelope");
    path
}

/// Write a copy of the envelope whose `eth_getBalance` answer for `tx_hash`'s
/// sender at its parent block is zero, and return its path.
///
/// The served transaction stays byte-identical — it still authenticates against
/// the requested hash — but the block executor rejects it (the sender cannot
/// fund its gas) and aborts the block: an execution-class abort raised through
/// served *state*, which carries no proof and cannot be authenticated the way a
/// consensus object can.
fn envelope_with_drained_sender(name: &str, tx_hash: &str) -> std::path::PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    // The transaction's own response names its sender and inclusion block.
    let marker = format!("\"hash\":\"{tx_hash}\"");
    let mut sender_block: Option<(String, u64)> = None;
    for entry in envelope["cache"].as_array().expect("cache entries") {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains(&marker) {
            continue;
        }
        let response: serde_json::Value =
            serde_json::from_str(value).expect("parse transaction response");
        let result = &response["result"];
        let from = result["from"].as_str().expect("transaction `from`").to_string();
        let number = u64::from_str_radix(
            result["blockNumber"].as_str().expect("blockNumber").trim_start_matches("0x"),
            16,
        )
        .expect("hex block number");
        assert!(
            sender_block.replace((from, number)).is_none(),
            "the envelope must hold exactly one response for {tx_hash}"
        );
    }
    let (from, number) = sender_block.expect("the envelope must hold the transaction");
    // The state fork reads the sender at the parent block. The entry is keyed
    // by the same `method\x00params` digest the capturing transport writes, so
    // the key is recomputed rather than searched for by value.
    let params = format!("[\"{from}\",\"0x{:x}\"]", number - 1);
    let key = format!("{}", alloy_primitives::keccak256(format!("eth_getBalance\x00{params}")));
    let mut doctored = 0;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        if entry["key"].as_str() != Some(key.as_str()) {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(entry["value"].as_str().expect("entry value is a string"))
                .expect("parse balance response");
        response["result"] = serde_json::Value::String("0x0".into());
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "the envelope must hold the sender's parent-block balance");

    let path =
        std::env::temp_dir().join(format!("mega_evme_batch_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write doctored envelope");
    path
}

/// Write a copy of the envelope whose `eth_getTransactionByHash` response for
/// `tx_hash` still returns the transaction object, but with `gas` set to `0x0`.
///
/// The tampered body no longer hashes to the requested hash, so the replay must
/// refuse to execute it: authentication fails before the transaction reaches
/// the block executor. Models a tampered capture (or a corrupted backend)
/// serving a body that does not match the hash it was asked for.
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

/// Write a copy of the envelope whose `eth_getTransactionByHash` response for
/// `tx_hash` keeps the signed body byte-identical but reports a different
/// `from` address, and return its path.
///
/// A signed transaction's `from` is not part of its encoding — it is derived
/// from the signature — so the tampered answer still hashes to the requested
/// hash. Executing it would run the transaction under the wrong sender;
/// authentication must instead re-derive the signer and reject the served
/// `from`.
fn envelope_with_reassigned_sender(name: &str, tx_hash: &str) -> std::path::PathBuf {
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
        result["from"] =
            serde_json::Value::String("0x000000000000000000000000000000000000dead".into());
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
    let (stdout, _stderr, code) = replay_envelope_full(envelope_path, args);
    (stdout, code)
}

/// Run `replay` against `envelope_path` and return stdout, stderr, and exit code.
fn replay_envelope_full(
    envelope_path: &std::path::Path,
    args: &[&str],
) -> (String, String, Option<i32>) {
    let mut cmd = mega_evme();
    cmd.args(["replay", "--rpc.replay-file", envelope_path.to_str().expect("path is utf-8")]);
    cmd.args(args);
    let output = cmd.output().expect("failed to run mega-evme");
    (
        String::from_utf8(output.stdout).expect("stdout is utf-8"),
        String::from_utf8(output.stderr).expect("stderr is utf-8"),
        output.status.code(),
    )
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

/// The structured error object a failing single-transaction `--json` run ends
/// with.
///
/// Single-transaction output is pretty-printed rather than NDJSON, so the
/// object is recovered by streaming every JSON value on stdout instead of
/// reading the last line.
fn single_run_error(stdout: &str) -> serde_json::Value {
    let values = common::json_values(stdout);
    let last =
        values.last().unwrap_or_else(|| panic!("a failing --json run must not leave stdout empty"));
    assert!(common::is_run_error(last), "the last stdout value must be the error object: {last}");
    last.clone()
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

/// An abort caused by a block-body transaction resolving to null is not a
/// definitive "unknown hash": the hash came from the block the endpoint already
/// served, so the null is an RPC inconsistency. The aborting target is reported
/// as `rpc` (not `not_found`), and every target swept up behind it is also
/// unanswered (`rpc`) with a message naming the transaction that aborted the
/// block.
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
                Some("rpc"),
                "a block-body hash resolving to null is an RPC inconsistency: {line}"
            );
            assert!(
                line["error"]["message"].as_str().is_some_and(|m| {
                    m.contains(missing) &&
                        m.contains("Block body") &&
                        m.contains("resolves it to null")
                }),
                "the aborting error must name the hash and the inconsistency: {line}"
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

    // Every failure is RPC-class (inconsistency + unanswered sweeps), so the
    // run exits 3 rather than the definitive-answer class of a user-supplied
    // unknown hash.
    assert_eq!(code, Some(3), "a block-body null lookup exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// A preceding transaction of a single-transaction replay resolving to null is
/// an endpoint inconsistency (exit 3), not a definitive unknown transaction.
///
/// The single-transaction path derives its preceding hashes from the block body
/// the endpoint already served, so the batch driver's reasoning applies
/// unchanged: a null lookup contradicts an answer the endpoint gave itself.
/// Doctors the index-0 transaction's response and replays a mid-block target,
/// which executes that transaction before its own.
#[test]
fn test_replay_single_transaction_preceding_null_is_an_rpc_failure() {
    let missing = BLOCK_TXS[0].0;
    let (target, target_index) = BLOCK_TXS[1];
    assert!(target_index > 0, "the target must have preceding transactions to execute");
    let path = envelope_without_transaction("single_preceding_null", missing);

    let (stdout, code) = replay_envelope_with_code(&path, &["--json", target]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(code, Some(3), "a preceding block-body null exits 3: {stdout}");
    let error = single_run_error(&stdout);
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        error["error"]["message"].as_str().is_some_and(|m| {
            m.contains(missing) && m.contains("Block body") && m.contains("resolves it to null")
        }),
        "the failure must name the hash and the inconsistency: {error}"
    );
}

/// The user-supplied target of a single-transaction replay resolving to null
/// keeps the definitive-answer class (exit 1).
///
/// Nothing the endpoint served claims that hash exists, so the null is an
/// answer about the caller's own question rather than a contradiction — the one
/// lookup on this path that stays `TransactionNotFound`.
#[test]
fn test_replay_single_transaction_target_null_is_not_found() {
    let target = BLOCK_TXS[1].0;
    let path = envelope_without_transaction("single_target_null", target);

    let (stdout, code) = replay_envelope_with_code(&path, &["--json", target]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(code, Some(1), "an unknown user-supplied hash exits 1: {stdout}");
    let error = single_run_error(&stdout);
    assert_eq!(error["error"]["code"].as_u64(), Some(1));
    assert_eq!(error["error"]["kind"].as_str(), Some("execution-error"));
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Transaction not found") && m.contains(target)),
        "the failure must stay a definitive not-found naming the hash: {error}"
    );
}

/// A preceding transaction the endpoint never answers is an unanswered
/// question, not a null answer: same exit class (3), different message.
///
/// Pins that reclassifying the null answer did not swallow the transport
/// failure reaching the same call site.
#[test]
fn test_replay_single_transaction_preceding_transport_error_is_an_rpc_failure() {
    let missing = BLOCK_TXS[0].0;
    let target = BLOCK_TXS[1].0;
    let path = envelope_dropping_transaction("single_preceding_miss", missing);

    let (stdout, code) = replay_envelope_with_code(&path, &["--json", target]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(code, Some(3), "an unanswered preceding lookup exits 3: {stdout}");
    let error = single_run_error(&stdout);
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("cache miss"), "the failure must name the missed request: {error}");
    assert!(
        !message.contains("resolves it to null"),
        "an unanswered lookup is not a null answer: {error}"
    );
}

/// An executor/setup abort on a mid-block transaction is still an execution
/// failure for that transaction only: every target behind it is unanswered
/// (`rpc`), not blamed as execution.
///
/// Doctors the sender's parent-block balance to zero — the lookup succeeds and
/// the transaction authenticates, but the block executor rejects it as an
/// invalid transaction (the sender cannot fund its gas) and aborts the block —
/// an execution-class error, not `TransactionNotFound`.
#[test]
fn test_replay_block_sweeps_targets_behind_execution_abort_as_rpc() {
    let (aborting, aborting_index) = EXEC_ABORT_TX;
    let path = envelope_with_drained_sender("exec_abort_block", aborting);

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

/// A non-target deterministic executor abort still exits 1: swept targets stay
/// `rpc` ("unanswered"), but the run floors the exit on the abort's own class so
/// a retryable exit is not reported for a permanent failure.
///
/// Per-target totals stay truthful ("2 of 2"): the abort is not a synthetic
/// third target failure.
///
/// `EXEC_ABORT_TX`'s sender is drained and the transaction kept out of the
/// `--tx-file` target list; only later targets of the same block are requested.
#[test]
fn test_replay_tx_file_non_target_execution_abort_exits_execution() {
    let (aborting, aborting_index) = EXEC_ABORT_TX;
    let (target_a, target_a_index) = BLOCK_TXS[1];
    let (target_b, _) = BLOCK_TXS[2];
    assert!(target_a_index > aborting_index, "targets must sit behind the non-target aborter");
    let path = envelope_with_drained_sender("non_target_exec_abort", aborting);
    let list = format!("{target_a}\n{target_b}\n");
    let list_path = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_non_target_exec_{}.txt", std::process::id()));
    std::fs::write(&list_path, list).expect("write tx list");

    let (stdout, stderr, code) =
        replay_envelope_full(&path, &["--tx-file", list_path.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list_path);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), 2, "only requested targets are reported: {stdout}");
    for line in &lines {
        assert_eq!(
            line["error"]["kind"].as_str(),
            Some("rpc"),
            "swept target stays unanswered: {line}"
        );
        assert!(
            line["error"]["message"].as_str().is_some_and(|m| {
                m.contains(aborting) || m.contains("aborted") || m.contains("Block replay")
            }),
            "the message must name the non-target abort: {line}"
        );
    }

    assert_eq!(code, Some(1), "a non-target execution abort exits 1, not 3");
    let err = run_error(&stdout);
    assert_eq!(err["error"]["kind"].as_str(), Some("execution-error"));
    let message = err["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("2 of 2 target transaction(s) failed"),
        "aggregate must stay truthful about targets (not 3 of 2): {message}"
    );
    assert!(
        stderr.contains("2 of 2 target transaction(s) failed") ||
            message.contains("2 of 2 target transaction(s) failed"),
        "stderr/stdout aggregate must not count the non-target abort as a target: \
         stderr={stderr}\nmessage={message}"
    );
}

/// A served transaction whose body does not hash to the requested hash must
/// not execute: the batch refuses it as a failed body-listed fetch (`rpc`) and
/// sweeps the targets behind it, instead of advancing the block state on the
/// wrong transaction.
///
/// Doctors a mid-block transaction's `gas` — any body change breaks the hash.
#[test]
fn test_replay_block_tampered_transaction_body_fails_authentication_as_rpc() {
    let (tampered, tampered_index) = EXEC_ABORT_TX;
    let path = envelope_with_zero_gas_transaction("tampered_body_block", tampered);

    let (stdout, code) =
        replay_envelope_with_code(&path, &["--block", &BLOCK.to_string(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), BLOCK_TX_COUNT, "every target is still reported exactly once");
    for (index, line) in lines.iter().enumerate() {
        let index = index as u64;
        if index < tampered_index {
            assert!(
                line.get("error").is_none(),
                "targets before the tampered fetch replay: {line}"
            );
            continue;
        }
        assert_eq!(
            line["error"]["kind"].as_str(),
            Some("rpc"),
            "a tampered or swept target went unanswered — never an execution verdict: {line}"
        );
    }
    let message = lines[tampered_index as usize]["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("the endpoint served a different transaction"),
        "the tampered target must name the authentication failure: {message}"
    );

    assert_eq!(code, Some(3), "an unauthenticated body-listed fetch exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// A served transaction whose signed body authenticates but whose `from` field
/// does not match the signature's signer must be refused: executing it would
/// run the transaction under the wrong sender.
#[test]
fn test_replay_single_transaction_reassigned_sender_fails_authentication() {
    let (target, _) = BLOCK_TXS[1];
    let path = envelope_with_reassigned_sender("single_reassigned_from", target);

    let (stdout, code) = replay_envelope_with_code(&path, &["--json", target]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(code, Some(3), "an unauthenticated target fetch exits 3: {stdout}");
    let error = single_run_error(&stdout);
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("does not match the signer") && message.contains(target),
        "the failure must name the sender mismatch and the transaction: {message}"
    );
}

/// A tampered preceding transaction is refused before execution on the single
/// path too: the target's pre-state depends on it, so the run fails as a failed
/// body-listed fetch (`rpc`) naming the tampered hash.
#[test]
fn test_replay_single_transaction_tampered_preceding_fails_authentication() {
    let (tampered, tampered_index) = EXEC_ABORT_TX;
    let (target, target_index) = BLOCK_TXS[1];
    assert!(target_index > tampered_index, "the tampered transaction must precede the target");
    let path = envelope_with_zero_gas_transaction("single_tampered_preceding", tampered);

    let (stdout, code) = replay_envelope_with_code(&path, &["--json", target]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(code, Some(3), "an unauthenticated preceding fetch exits 3: {stdout}");
    let error = single_run_error(&stdout);
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Block body lists transaction") &&
            message.contains(tampered) &&
            message.contains("served a different transaction"),
        "the failure must name the tampered body-listed fetch: {message}"
    );
}

/// A non-target transport abort (cache miss) exits 3 and names the failing
/// fetch so swept entries are distinguishable from the cause.
#[test]
fn test_replay_tx_file_non_target_transport_abort_names_hash_and_exits_rpc() {
    let (aborting, aborting_index) = EXEC_ABORT_TX;
    let (target_a, target_a_index) = BLOCK_TXS[1];
    let (target_b, _) = BLOCK_TXS[2];
    assert!(target_a_index > aborting_index, "targets must sit behind the non-target aborter");
    let path = envelope_dropping_transaction("non_target_transport_abort", aborting);
    let list = format!("{target_a}\n{target_b}\n");
    let list_path = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_non_target_rpc_{}.txt", std::process::id()));
    std::fs::write(&list_path, list).expect("write tx list");

    let (stdout, code) =
        replay_envelope_with_code(&path, &["--tx-file", list_path.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list_path);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), 2, "only requested targets are reported: {stdout}");
    for line in &lines {
        assert_eq!(
            line["error"]["kind"].as_str(),
            Some("rpc"),
            "swept target stays unanswered: {line}"
        );
        assert!(
            line["error"]["message"].as_str().is_some_and(|m| m.contains(aborting)),
            "the abort message must name the failing fetch: {line}"
        );
    }

    assert_eq!(code, Some(3), "a transport abort exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
}

/// With `--dump-fixture-dir`, a target that executed and drafted a fixture
/// before a later transport abort keeps its result line; only the fixture field
/// fails, and that failure inherits the abort's `rpc` class so the run exits 3
/// rather than converting the discard into an execution-class error.
#[test]
fn test_replay_tx_file_dump_fixture_inherits_transport_abort_class() {
    let (aborting, aborting_index) = MID_BLOCK_NON_TARGET;
    // Early non-deposit target builds a Ready draft; a later target forces the
    // job past the non-target aborter between them.
    let (early, early_index) = BLOCK_TXS[1];
    let (late, late_index) = BLOCK_TXS[2];
    assert!(early_index < aborting_index && aborting_index < late_index);

    let path = envelope_dropping_transaction("fixture_inherits_rpc_abort", aborting);
    let list = format!("{early}\n{late}\n");
    let list_path = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_fixture_abort_{}.txt", std::process::id()));
    std::fs::write(&list_path, list).expect("write tx list");
    let dir =
        std::env::temp_dir().join(format!("mega_evme_fixture_abort_dir_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dump dir");

    let (stdout, code) = replay_envelope_with_code(
        &path,
        &[
            "--tx-file",
            list_path.to_str().unwrap(),
            "--dump-fixture-dir",
            dir.to_str().unwrap(),
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&list_path);
    let _ = std::fs::remove_dir_all(&dir);
    let lines = ndjson(&stdout);

    assert_eq!(lines.len(), 2, "both targets are reported: {stdout}");

    let early_line = lines.iter().find(|l| l["tx_hash"] == early).expect("early target line");
    assert!(
        early_line.get("error").is_none(),
        "the early target keeps its execution result line: {early_line}"
    );
    assert!(
        early_line["success"].is_boolean(),
        "result fields are present on the early target: {early_line}"
    );
    let fixture = &early_line["fixture"];
    assert!(
        fixture["error"].is_string(),
        "the drafted fixture must report the abort (not a skip): {early_line}"
    );
    assert!(
        fixture["error"].as_str().is_some_and(|m| m.contains("aborted") || m.contains("discarded")),
        "fixture error names the abort: {early_line}"
    );
    assert!(fixture.get("path").is_none(), "no fixture file is written after abort");
    assert!(fixture.get("skipped").is_none(), "abort discard is an error, not a skip");

    let late_line = lines.iter().find(|l| l["tx_hash"] == late).expect("late target line");
    assert_eq!(
        late_line["error"]["kind"].as_str(),
        Some("rpc"),
        "the late target is swept as unanswered: {late_line}"
    );
    assert!(
        late_line["error"]["message"].as_str().is_some_and(|m| m.contains(aborting)),
        "swept message names the failing fetch: {late_line}"
    );

    // Abort is transport-class; fixture discard inherits it — not exit 1.
    assert_eq!(code, Some(3), "transport abort with fixture discard exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
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

/// Same-block mix of a successful early target and a later inclusion failure:
/// NDJSON line order follows ascending `(block, tx_index)`, so the earlier
/// result precedes the later inclusion failure even when the failure was
/// decided before the execute loop.
///
/// Looks up by hash alone cannot catch this; the test asserts line positions.
#[test]
fn test_replay_tx_file_same_block_mixed_inclusion_preserves_line_order() {
    let (early_target, early_index) = BLOCK_TXS[1];
    let (late_target, late_index) = BLOCK_TXS[2];
    assert!(early_index < late_index);
    let wrong_hash = "0x2222222222222222222222222222222222222222222222222222222222222222";

    // Doctor only the later target's inclusion hash so it fails the membership
    // guard while the earlier target still replays.
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let marker = format!("\"hash\":\"{late_target}\"");
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
    assert_eq!(doctored, 1, "exactly one response describes the late target");

    let envelope_path = std::env::temp_dir()
        .join(format!("mega_evme_batch_mixed_order_{}.json", std::process::id()));
    std::fs::write(&envelope_path, envelope.to_string()).expect("write doctored envelope");

    // List the later (failing) target first so a pre-execution-first emit would
    // put its failure line before the earlier result.
    let list = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_mixed_order_{}.txt", std::process::id()));
    std::fs::write(&list, format!("{late_target}\n{early_target}\n")).expect("write tx list");

    let (stdout, code) =
        replay_envelope_with_code(&envelope_path, &["--tx-file", list.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&envelope_path);
    let _ = std::fs::remove_file(&list);
    let lines = ndjson(&stdout);
    assert_eq!(lines.len(), 2, "every target is reported once: {stdout}");

    // End-to-end line order: earlier body index first, absent/divergent last.
    assert_eq!(
        lines[0]["tx_hash"].as_str(),
        Some(early_target),
        "line 0 must be the earlier target's result, got: {}",
        lines[0]
    );
    assert!(lines[0].get("error").is_none(), "earlier target still replays: {}", lines[0]);
    assert_eq!(lines[0]["tx_index"].as_u64(), Some(early_index));
    assert_eq!(lines[0]["success"].as_bool(), Some(true));

    assert_eq!(
        lines[1]["tx_hash"].as_str(),
        Some(late_target),
        "line 1 must be the later target's inclusion failure, got: {}",
        lines[1]
    );
    assert_eq!(lines[1]["error"]["kind"].as_str(), Some("rpc"), "inclusion failure: {}", lines[1]);

    assert_eq!(code, Some(3), "an unanswered target exits 3");
}

/// Two same-height targets that report different inclusion hashes get
/// independent outcomes: the one that matches the fetched block replays, the
/// one that does not fails as `rpc`. Outcomes must not depend on file order.
///
/// The resolution step used to keep a first-seen job-level anchor and reject
/// later peers that disagreed, so a stale target listed first could poison the
/// canonical peer. Both orders are exercised against the same doctored capture.
#[test]
fn test_replay_tx_file_inclusion_mismatch_is_order_independent() {
    let (stale_target, _) = BLOCK_TXS[1];
    let (canonical_target, canonical_index) = BLOCK_TXS[2];
    let wrong_hash = "0x2222222222222222222222222222222222222222222222222222222222222222";

    // Doctor only the stale target's inclusion hash; the canonical target and
    // the block body keep their original agreement.
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let marker = format!("\"hash\":\"{stale_target}\"");
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
    assert_eq!(doctored, 1, "exactly one response describes the stale target");

    let envelope_path = std::env::temp_dir()
        .join(format!("mega_evme_batch_inclusion_order_{}.json", std::process::id()));
    std::fs::write(&envelope_path, envelope.to_string()).expect("write doctored envelope");

    for (label, first, second) in [
        ("stale_first", stale_target, canonical_target),
        ("canonical_first", canonical_target, stale_target),
    ] {
        let list = std::env::temp_dir()
            .join(format!("mega_evme_tx_list_inclusion_order_{label}_{}.txt", std::process::id()));
        std::fs::write(&list, format!("{first}\n{second}\n")).expect("write tx list");

        let (stdout, code) = replay_envelope_with_code(
            &envelope_path,
            &["--tx-file", list.to_str().unwrap(), "--json"],
        );
        let lines = ndjson(&stdout);
        assert_eq!(lines.len(), 2, "{label}: every target is reported once: {stdout}");

        let stale = lines
            .iter()
            .find(|line| line["tx_hash"].as_str() == Some(stale_target))
            .unwrap_or_else(|| panic!("{label}: stale target must be reported"));
        assert_eq!(
            stale["error"]["kind"].as_str(),
            Some("rpc"),
            "{label}: mismatched inclusion is unanswered: {stale}"
        );
        let message = stale["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("divergent views") &&
                message.contains(wrong_hash) &&
                message.contains("resolved as included"),
            "{label}: message names both views: {message}"
        );

        let ok = lines
            .iter()
            .find(|line| line["tx_hash"].as_str() == Some(canonical_target))
            .unwrap_or_else(|| panic!("{label}: canonical target must be reported"));
        assert!(ok.get("error").is_none(), "{label}: matching inclusion still replays: {ok}");
        assert_eq!(ok["block_number"].as_u64(), Some(BLOCK));
        assert_eq!(ok["tx_index"].as_u64(), Some(canonical_index));
        assert_eq!(ok["success"].as_bool(), Some(true));

        assert_eq!(code, Some(3), "{label}: an unanswered target exits 3");
        assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));

        let _ = std::fs::remove_file(&list);
    }

    let _ = std::fs::remove_file(&envelope_path);
}

/// A target whose reported inclusion hash matches the fetched block, but which
/// the block body does not list, is an endpoint self-contradiction (`rpc`), not
/// a definitive `not_found`.
///
/// The lookup said "in block B"; B's body lacks it. That is the same class as
/// the single-transaction membership guard, not an answer that the hash is
/// unknown.
#[test]
fn test_replay_tx_file_anchored_but_absent_target_is_rpc() {
    let (target, _) = BLOCK_TXS[1];

    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");

    // Keep the transaction lookup intact (correct number + hash) but drop the
    // hash from the block body it claims to belong to.
    let mut body_doctored = 0;
    let mut block_hash = None;
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
        let number = result.get("number").and_then(|n| {
            n.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        });
        if number != Some(BLOCK) {
            continue;
        }
        let Some(txs) = result.get_mut("transactions").and_then(|t| t.as_array_mut()) else {
            continue;
        };
        let before = txs.len();
        txs.retain(|tx| tx.as_str() != Some(target));
        if txs.len() != before {
            block_hash = result.get("hash").and_then(|h| h.as_str()).map(str::to_string);
            entry["value"] = serde_json::Value::String(response.to_string());
            body_doctored += 1;
        }
    }
    assert_eq!(body_doctored, 1, "exactly one block body for {BLOCK} must list the target");
    let block_hash = block_hash.expect("block hash");

    // Pair with another-block target so a clean job still runs alongside the
    // inconsistency: only the absent target fails.
    let envelope_path = std::env::temp_dir()
        .join(format!("mega_evme_batch_anchored_absent_{}.json", std::process::id()));
    std::fs::write(&envelope_path, envelope.to_string()).expect("write doctored envelope");
    let list = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_anchored_absent_{}.txt", std::process::id()));
    std::fs::write(&list, format!("{target}\n{OTHER_BLOCK_TX}\n")).expect("write tx list");

    let (stdout, code) =
        replay_envelope_with_code(&envelope_path, &["--tx-file", list.to_str().unwrap(), "--json"]);
    let lines = ndjson(&stdout);
    assert_eq!(lines.len(), 2, "every target is reported once: {stdout}");

    let failed = lines
        .iter()
        .find(|line| line["tx_hash"].as_str() == Some(target))
        .expect("absent target must be reported");
    assert_eq!(
        failed["error"]["kind"].as_str(),
        Some("rpc"),
        "anchored-but-absent is an RPC inconsistency, not not_found: {failed}"
    );
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(target) &&
            message.contains(&block_hash) &&
            message.contains("does not list") &&
            message.contains("divergent views"),
        "message names the target, the block, and the cause: {message}"
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

/// When every target of a job is absent from the fetched block body, each gets
/// its definitive `rpc` answer pre-loop and the block is never executed.
///
/// After doctoring the body, the envelope is stripped of every response that is
/// not a target transaction lookup or the doctored block body (parent header,
/// state, receipts, body-tx objects). A clean early return needs only those two
/// shapes; forking state or walking the body would miss and fail differently.
#[test]
fn test_replay_tx_file_all_targets_absent_from_body_skips_block_execution() {
    let targets: Vec<&str> = BLOCK_TXS.iter().map(|(h, _)| *h).collect();

    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");

    let mut body_doctored = 0;
    let mut block_hash = None;
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
        let number = result.get("number").and_then(|n| {
            n.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        });
        if number != Some(BLOCK) {
            continue;
        }
        let Some(txs) = result.get_mut("transactions").and_then(|t| t.as_array_mut()) else {
            continue;
        };
        let before = txs.len();
        txs.retain(|tx| {
            let hash = tx.as_str().unwrap_or("");
            !targets.contains(&hash)
        });
        if txs.len() != before {
            block_hash = result.get("hash").and_then(|h| h.as_str()).map(str::to_string);
            entry["value"] = serde_json::Value::String(response.to_string());
            body_doctored += 1;
        }
    }
    assert_eq!(body_doctored, 1, "exactly one block body for {BLOCK} must list the targets");
    let block_hash = block_hash.expect("block hash");

    // Cache keys are request hashes, so filter by response shape: keep only the
    // target transaction lookups and the doctored block-at-height body.
    let entries = envelope["cache"].as_array_mut().expect("cache entries");
    entries.retain(|entry| {
        let value = entry["value"].as_str().unwrap_or("");
        let Ok(response) = serde_json::from_str::<serde_json::Value>(value) else {
            return false;
        };
        let Some(result) = response.get("result") else {
            return false;
        };
        if !result.is_object() {
            return false;
        }
        // Transaction lookup for one of our targets.
        if let Some(hash) = result.get("hash").and_then(|h| h.as_str()) {
            if targets.contains(&hash) && result.get("blockNumber").is_some() {
                return true;
            }
        }
        // Doctored block body at the job height.
        let number = result.get("number").and_then(|n| {
            n.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        });
        number == Some(BLOCK) && result.get("transactions").is_some()
    });

    let envelope_path = std::env::temp_dir()
        .join(format!("mega_evme_batch_all_absent_{}.json", std::process::id()));
    std::fs::write(&envelope_path, envelope.to_string()).expect("write doctored envelope");
    let list = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_all_absent_{}.txt", std::process::id()));
    std::fs::write(&list, format!("{}\n", targets.join("\n"))).expect("write tx list");

    let (stdout, code) =
        replay_envelope_with_code(&envelope_path, &["--tx-file", list.to_str().unwrap(), "--json"]);
    let lines = ndjson(&stdout);
    assert_eq!(lines.len(), targets.len(), "every target is reported once: {stdout}");

    for target in &targets {
        let failed = lines
            .iter()
            .find(|line| line["tx_hash"].as_str() == Some(target))
            .unwrap_or_else(|| panic!("target {target} must be reported"));
        assert_eq!(failed["error"]["kind"].as_str(), Some("rpc"), "all-absent is rpc: {failed}");
        let message = failed["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("does not list") && message.contains(&block_hash),
            "message names absence and block: {message}"
        );
        assert!(
            failed.get("success").is_none() && failed.get("receipt").is_none(),
            "no execution result for an unexecuted target: {failed}"
        );
    }

    assert_eq!(code, Some(3), "all-absent exits 3");
    assert_eq!(run_error(&stdout)["error"]["kind"].as_str(), Some("rpc-failure"));
    // If the driver had forked state or walked body transactions, the stripped
    // envelope would have produced a cache-miss error naming parent/state — not
    // a clean per-target membership rpc answer for every hash.
    assert!(
        !stdout.contains("cache miss") &&
            !stdout.contains("not found in the offline") &&
            !stdout.contains("not present in the offline"),
        "early return must not touch parent/state/body-tx paths:\n{stdout}"
    );

    let _ = std::fs::remove_file(&envelope_path);
    let _ = std::fs::remove_file(&list);
}

/// A hash whose resolution answers `null` keeps the definitive `not_found`
/// class: the endpoint denied the hash, rather than claiming inclusion and then
/// contradicting itself.
#[test]
fn test_replay_tx_file_null_resolution_stays_not_found() {
    let (target, _) = BLOCK_TXS[1];
    let path = envelope_without_transaction("tx_file_null_resolution", target);

    // Pair with a clean other-block target so the run still produces a success
    // line next to the definitive not-found.
    let list = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_null_resolution_{}.txt", std::process::id()));
    std::fs::write(&list, format!("{target}\n{OTHER_BLOCK_TX}\n")).expect("write tx list");

    let (stdout, code) =
        replay_envelope_with_code(&path, &["--tx-file", list.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&path);
    let lines = ndjson(&stdout);
    assert_eq!(lines.len(), 2, "every target is reported once: {stdout}");

    let failed = lines
        .iter()
        .find(|line| line["tx_hash"].as_str() == Some(target))
        .expect("null-resolution target must be reported");
    assert_eq!(
        failed["error"]["kind"].as_str(),
        Some("not_found"),
        "a null lookup is a definitive not_found: {failed}"
    );
    assert_eq!(
        failed["error"]["message"].as_str(),
        Some("Transaction not found"),
        "not_found message is unchanged: {failed}"
    );

    let ok = lines
        .iter()
        .find(|line| line["tx_hash"].as_str() == Some(OTHER_BLOCK_TX))
        .expect("other-block target must be reported");
    assert!(ok.get("error").is_none(), "targets in other blocks still replay: {ok}");
    assert_eq!(ok["success"].as_bool(), Some(true));

    // Definitive not_found is an execution-class failure (exit 1), not rpc.
    assert_eq!(code, Some(1), "a definitive not_found exits 1");

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

/// A `--tx-file` target whose `eth_getTransactionByHash` answer carries an
/// inclusion hash but no block number is unanswered: the endpoint served
/// contradictory metadata, so the target is not treated as pending.
#[test]
fn test_replay_tx_file_rejects_inclusion_hash_without_block_number() {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let (target, _) = BLOCK_TXS[1];

    // Keep the inclusion hash so the response still claims a mined block, but
    // drop the block number. Cache entries are keyed by the request, so the
    // doctored answer still resolves.
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
            result.get("blockHash").is_some_and(|h| !h.is_null()),
            "fixture transaction must report an inclusion hash"
        );
        assert!(
            result.get("blockNumber").is_some_and(|n| !n.is_null()),
            "fixture transaction must report a block number before doctoring"
        );
        result["blockNumber"] = serde_json::Value::Null;
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "exactly one response describes the target transaction");

    let envelope_path = std::env::temp_dir()
        .join(format!("mega_evme_batch_null_number_with_hash_{}.json", std::process::id()));
    std::fs::write(&envelope_path, envelope.to_string()).expect("write doctored envelope");
    // Pair the contradictory target with one from another block so a clean job
    // still runs when resolution fails for only one hash.
    let list = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_null_number_with_hash_{}.txt", std::process::id()));
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
        "contradictory metadata is unanswered as rpc, not pending: {failed}"
    );
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("contradictory") &&
            (message.contains("without a block number") || message.contains("block number")),
        "message names the contradiction: {message}"
    );
    assert!(
        !message.contains("pending"),
        "contradictory metadata must not be classified as pending: {message}"
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

/// A genuinely pending `--tx-file` target (`blockNumber` and `blockHash` both
/// null) keeps the pending classification and is not queued for replay.
#[test]
fn test_replay_tx_file_classifies_null_number_and_hash_as_pending() {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(envelope()).expect("read envelope"))
            .expect("parse envelope");
    let (target, _) = BLOCK_TXS[1];

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
        result["blockNumber"] = serde_json::Value::Null;
        result["blockHash"] = serde_json::Value::Null;
        entry["value"] = serde_json::Value::String(response.to_string());
        doctored += 1;
    }
    assert_eq!(doctored, 1, "exactly one response describes the target transaction");

    let envelope_path = std::env::temp_dir()
        .join(format!("mega_evme_batch_pending_nulls_{}.json", std::process::id()));
    std::fs::write(&envelope_path, envelope.to_string()).expect("write doctored envelope");
    let list = std::env::temp_dir()
        .join(format!("mega_evme_tx_list_pending_nulls_{}.txt", std::process::id()));
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
        Some("pending"),
        "null number and null hash is pending: {failed}"
    );
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(
        message, "Transaction is pending (no block number)",
        "pending message is unchanged: {message}"
    );

    let ok = lines
        .iter()
        .find(|line| line["tx_hash"].as_str() == Some(OTHER_BLOCK_TX))
        .expect("other-block target must be reported");
    assert!(ok.get("error").is_none(), "targets in other blocks still replay: {ok}");
    assert_eq!(ok["block_number"].as_u64(), Some(OTHER_BLOCK));
    assert_eq!(ok["success"].as_bool(), Some(true));

    // Pending counts as an execution-class failure (exit 1), not rpc (exit 3).
    assert_eq!(code, Some(1), "a pending target exits 1");

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

/// Sweeping a block with `--dump-fixture-dir` writes a fixture for every
/// transaction it can express and skips genuine unsupported shapes (deposit)
/// without failing the run.
///
/// Every OP-stack block opens with a deposit, which the fixture format cannot
/// represent. Reporting that as an error rather than a skip would make a
/// whole-block sweep exit non-zero on every block, so this pins the
/// classification end to end: 22 files written, the deposit skipped with its
/// reason, nothing reported as an error, and exit 0.
///
/// (An unanswered on-chain receipt is a separate rpc-class fixture error and is
/// covered by the doctored dump-dir tests in `replay_verify`.)
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
