//! Integration tests for the CLI's exit-code taxonomy and its failure output.
//!
//! They run fully offline against the committed RPC capture
//! (`fixtures/replay_offline.cache.json`), so they are deterministic: a hash the
//! capture cannot answer models an endpoint that never answers, and the
//! validation paths need no provider at all. The mismatch class (exit 2) is
//! covered by `replay_verify.rs`, which doctors a copy of the same capture.

use std::process::{Command, Output};

mod common;

/// Offline RPC capture used as the replay file.
/// Name of the committed offline capture, resolved through the shared fixture
/// helper so its location lives in exactly one place.
const CACHE: &str = "replay_offline.cache.json";

/// Path of the committed offline capture.
fn cache() -> std::path::PathBuf {
    common::fixture(CACHE)
}

/// The transaction the committed capture can replay.
const TX_OK: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

/// Number of the block `TX_OK` was mined in, as the capture reports it.
const BLOCK_NUMBER: u64 = 18_172_461;

/// A hash the capture holds no response for: the question goes unanswered.
const UNANSWERABLE_TX: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";

/// Request fingerprint of a state read `TX_OK` performs while it executes.
///
/// Entries are keyed by the request, so dropping this one from a copy of the
/// capture models an endpoint that stops answering mid-execution — the read
/// then fails inside the EVM and surfaces as a block execution error.
const IN_EXECUTION_STATE_READ: &str =
    "0x0d9aee1b171e0c4a2be0107def891d838cc94d71e4046cb95b00a1c2a61cffed";

/// Request fingerprint of the EIP-2935 history-storage slot the pre-block
/// system call writes when replaying `TX_OK`'s block.
///
/// Dropping this entry makes `apply_pre_execution_changes` fail inside the
/// blockhash contract call. mega-evm stringifies that database failure into
/// `BlockHashContractCall { message }`, so classification must recover the RPC
/// class from the stable `RPC error:` Display prefix rather than from a typed
/// cause chain.
const PRE_BLOCK_HISTORY_STORAGE_READ: &str =
    "0x3abed4482ce079cf23c80a8e43bd75f8ac32b8b108e925b39f0a5c93de4aff48";

/// Request fingerprint of an EIP-4788 beacon-roots storage slot the pre-block
/// system call touches for the same block — a second stringified validation
/// path (`BeaconRootContractCall`) with the same recovery rule.
const PRE_BLOCK_BEACON_ROOT_STORAGE_READ: &str =
    "0x49e3c5174c528b49897a0556c762d4fb88e1ad5e6aa8f8795ddbc37aa6c278f0";

/// Outcome of one `mega-evme` invocation.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The process exit code the run ended with.
    fn code(&self) -> i32 {
        self.code.expect("mega-evme was killed by a signal")
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

    /// How many failure reports stderr carries.
    ///
    /// Counted by the report prefix: a message may itself span lines (an RPC
    /// error appends a re-capture hint), and only the report opens one.
    fn error_lines(&self) -> usize {
        self.stderr.lines().filter(|line| line.starts_with("error: ")).count()
    }
}

fn run(args: &[&str]) -> Run {
    let output: Output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(args)
        .output()
        .expect("failed to run mega-evme");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

/// Run `replay` against the committed offline capture.
fn replay(args: &[&str]) -> Run {
    let cache = cache();
    let mut argv =
        vec!["replay", "--rpc.replay-file", cache.to_str().expect("fixture path is utf-8")];
    argv.extend_from_slice(args);
    run(&argv)
}

/// Write a copy of the committed capture without the entry `key` answers, and
/// return its path.
fn cache_without_entry(name: &str, key: &str) -> std::path::PathBuf {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
            .expect("parse offline cache");
    let entries = envelope["cache"].as_array_mut().expect("cache entries");
    let before = entries.len();
    entries.retain(|entry| entry["key"].as_str() != Some(key));
    assert_eq!(entries.len() + 1, before, "the capture must hold exactly one entry for {key}");

    let path =
        std::env::temp_dir().join(format!("mega_evme_exit_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write pruned cache");
    path
}

/// Write a copy of the committed capture whose parent-block response reports a
/// hash that does not link to the replayed block, and return its path together
/// with the hash the untouched capture reported.
///
/// The capture answers `eth_getBlockByNumber` for exactly two heights: the
/// replayed block and its parent. Rewriting the lower-numbered body's own `hash`
/// models an endpoint serving divergent views of the chain, since the two blocks
/// are fetched in separate calls. Only that field changes — state reads are
/// keyed by block number, so every other response still resolves.
fn cache_with_unlinked_parent(name: &str, wrong_hash: &str) -> (std::path::PathBuf, String) {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
            .expect("parse offline cache");
    let entries = envelope["cache"].as_array_mut().expect("cache entries");

    let mut blocks: Vec<(usize, u64)> = vec![];
    for (index, entry) in entries.iter().enumerate() {
        let value = entry["value"].as_str().expect("entry value is a string");
        let Ok(response) = serde_json::from_str::<serde_json::Value>(value) else {
            continue;
        };
        let Some(result) = response.get("result") else {
            continue;
        };
        // A block body is the only response carrying a parent hash.
        if !result.is_object() || result.get("parentHash").is_none() {
            continue;
        }
        let number = result["number"].as_str().expect("block number is a string");
        let number =
            u64::from_str_radix(number.trim_start_matches("0x"), 16).expect("block number is hex");
        blocks.push((index, number));
    }
    assert_eq!(blocks.len(), 2, "the capture must hold the replayed block and its parent");
    blocks.sort_unstable_by_key(|&(_, number)| number);
    let (parent_index, _) = blocks[0];

    let entry = &mut entries[parent_index];
    let mut response: serde_json::Value =
        serde_json::from_str(entry["value"].as_str().expect("entry value is a string"))
            .expect("parse parent block response");
    let result = &mut response["result"];
    let original = result["hash"].as_str().expect("parent block hash").to_string();
    result["hash"] = serde_json::Value::String(wrong_hash.to_string());
    entry["value"] = serde_json::Value::String(response.to_string());

    let path =
        std::env::temp_dir().join(format!("mega_evme_exit_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write doctored cache");
    (path, original)
}

/// Write a copy of the committed capture whose `eth_getTransactionByHash`
/// response for `TX_OK` carries `block_hash` as its inclusion hash, and return
/// its path together with the hash the untouched capture reported.
///
/// Entries are keyed by the request, so the doctored answer still resolves. The
/// transaction lookup and the block fetch are separate calls, so rewriting only
/// the lookup models an endpoint that describes the target's inclusion
/// differently than the block it serves for that number. Passing
/// [`serde_json::Value::Null`] models a lookup that reports a mined transaction
/// without anchoring it to any block at all.
fn cache_with_inclusion_hash(
    name: &str,
    block_hash: serde_json::Value,
) -> (std::path::PathBuf, String) {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
            .expect("parse offline cache");
    // Only the transaction's own response carries it as the `hash` field; the
    // block body lists bare hashes and a receipt names it `transactionHash`.
    let marker = format!("\"hash\":\"{TX_OK}\"");
    let mut original = None;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        if !value.contains(&marker) {
            continue;
        }
        let mut response: serde_json::Value =
            serde_json::from_str(value).expect("parse transaction response");
        let result = response.get_mut("result").expect("transaction result");
        assert!(result.is_object(), "expected a transaction object for {TX_OK}");
        assert!(
            result.get("blockNumber").is_some_and(|n| !n.is_null()),
            "the captured transaction must report a block number"
        );
        original = Some(result["blockHash"].as_str().expect("inclusion hash").to_string());
        result["blockHash"] = block_hash.clone();
        entry["value"] = serde_json::Value::String(response.to_string());
    }
    let original = original.expect("the capture must hold exactly one response for the target");

    let path =
        std::env::temp_dir().join(format!("mega_evme_exit_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write doctored cache");
    (path, original)
}

/// Write a copy of the committed capture whose replayed block no longer lists
/// `TX_OK` in its body, and return its path together with that block's hash.
///
/// The block keeps its own hash, so the lookup's inclusion hash still matches
/// what the endpoint serves for that number: only the membership the preceding
/// transaction set is derived from is gone.
fn cache_without_target_in_block_body(name: &str) -> (std::path::PathBuf, String) {
    let mut envelope: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cache()).expect("read offline cache"))
            .expect("parse offline cache");
    let mut block_hash = None;
    for entry in envelope["cache"].as_array_mut().expect("cache entries").iter_mut() {
        let value = entry["value"].as_str().expect("entry value is a string");
        let Ok(mut response) = serde_json::from_str::<serde_json::Value>(value) else {
            continue;
        };
        // A block body is the only response carrying a transaction list.
        let Some(txs) = response
            .get_mut("result")
            .and_then(|result| result.get_mut("transactions"))
            .and_then(|txs| txs.as_array_mut())
        else {
            continue;
        };
        if !txs.iter().any(|hash| hash.as_str() == Some(TX_OK)) {
            continue;
        }
        txs.retain(|hash| hash.as_str() != Some(TX_OK));
        block_hash = Some(response["result"]["hash"].as_str().expect("block hash").to_string());
        entry["value"] = serde_json::Value::String(response.to_string());
    }
    let block_hash = block_hash.expect("exactly one captured block body lists the target");

    let path =
        std::env::temp_dir().join(format!("mega_evme_exit_{name}_{}.json", std::process::id()));
    std::fs::write(&path, envelope.to_string()).expect("write doctored cache");
    (path, block_hash)
}

/// Write a `--tx-file` holding `contents`, and return its path.
fn tx_file(name: &str, contents: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("mega_evme_exit_{name}_{}.txt", std::process::id()));
    std::fs::write(&path, contents).expect("write tx list");
    path
}

/// Bad input is an execution-class failure: exit 1, with the structured object
/// as the last stdout line.
#[test]
fn test_invalid_input_exits_one_with_a_json_error_object() {
    let list = tx_file("bad_hash", "not-a-hash\n");

    let run = replay(&["--tx-file", list.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 1, "bad input exits 1.\nstderr: {}", run.stderr);
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(1));
    assert_eq!(error["error"]["kind"].as_str(), Some("execution-error"));
    assert!(
        error["error"]["message"].as_str().is_some_and(|m| m.contains("not-a-hash")),
        "the message must name the offending input: {error}"
    );
}

/// A rejected flag combination is bad input too, and still ends `--json` stdout
/// with the error object rather than nothing at all.
#[test]
fn test_rejected_flag_combination_exits_one_with_a_json_error_object() {
    let run = replay(&["--dump-fixture-dir", "/tmp/mega-evme-should-not-exist", "--json", TX_OK]);

    assert_eq!(run.code(), 1, "a rejected flag combination exits 1.\nstderr: {}", run.stderr);
    assert_eq!(run.error_object()["error"]["kind"].as_str(), Some("execution-error"));
}

/// A transaction the offline capture cannot answer is an RPC failure: the
/// question went unanswered, which is distinct from a definitive "no".
#[test]
fn test_offline_cache_miss_exits_rpc_failure_with_a_json_error_object() {
    let run = replay(&["--json", UNANSWERABLE_TX]);

    assert_eq!(run.code(), 3, "a cache miss exits 3.\nstderr: {}", run.stderr);
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        error["error"]["message"].as_str().is_some_and(|m| m.contains("cache miss")),
        "the message must explain the miss: {error}"
    );
}

/// A state read that fails while the EVM is executing arrives as a block
/// execution error, but it is still an unanswered question: the run exits 3, and
/// a batch reports the target as an `rpc` failure rather than an execution one.
#[test]
fn test_state_read_failure_during_execution_is_an_rpc_failure() {
    let path = cache_without_entry("state_read", IN_EXECUTION_STATE_READ);
    let cache = path.to_str().unwrap();

    let single = run(&["replay", "--rpc.replay-file", cache, "--json", TX_OK]);
    assert_eq!(single.code(), 3, "an unanswered state read exits 3.\nstderr: {}", single.stderr);
    let error = single.error_object();
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Block execution error") && m.contains("cache miss")),
        "the failure must be the block error carrying the missed read: {error}"
    );

    let list = tx_file("state_read", &format!("{TX_OK}\n"));
    let batch = run(&["replay", "--rpc.replay-file", cache, "--tx-file", list.to_str().unwrap()]);
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_file(&path);

    assert_eq!(batch.code(), 3, "the batch run exits 3 too.\nstderr: {}", batch.stderr);
    assert!(
        batch.stdout.contains("Error (rpc):"),
        "the target is reported as unanswered:\n{}",
        batch.stdout
    );
}

/// A cache miss during the pre-block EIP-2935 blockhash system call is an
/// unanswered RPC question even though mega-evm stringifies it into
/// `BlockHashContractCall { message }` before the exit classifier sees it.
///
/// Without prefix recovery this lands as exit 1 / `execution-error`; with it,
/// single-run and batch both report the RPC class (exit 3).
#[test]
fn test_pre_block_blockhash_system_call_cache_miss_is_an_rpc_failure() {
    let path = cache_without_entry("pre_block_2935", PRE_BLOCK_HISTORY_STORAGE_READ);
    let cache = path.to_str().unwrap();

    let single = run(&["replay", "--rpc.replay-file", cache, "--json", TX_OK]);
    assert_eq!(
        single.code(),
        3,
        "a pre-block history-storage miss exits 3 (was exit 1 before stringified-RPC recovery).\n\
         stderr: {}",
        single.stderr
    );
    let error = single.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("blockhash contract call") &&
            message.contains("RPC error:") &&
            message.contains("cache miss"),
        "the message must name the stringified pre-block path and the miss: {error}"
    );

    let list = tx_file("pre_block_2935", &format!("{TX_OK}\n"));
    let batch = run(&["replay", "--rpc.replay-file", cache, "--tx-file", list.to_str().unwrap()]);
    let _ = std::fs::remove_file(&list);
    let _ = std::fs::remove_file(&path);

    assert_eq!(batch.code(), 3, "the batch run exits 3 too.\nstderr: {}", batch.stderr);
    assert!(
        batch.stdout.contains("Error (rpc):"),
        "the target is reported as unanswered:\n{}",
        batch.stdout
    );
}

/// Same recovery for the EIP-4788 beacon-root pre-block system call, which uses
/// `BeaconRootContractCall { message }` rather than the blockhash variant.
#[test]
fn test_pre_block_beacon_root_system_call_cache_miss_is_an_rpc_failure() {
    let path = cache_without_entry("pre_block_4788", PRE_BLOCK_BEACON_ROOT_STORAGE_READ);
    let cache = path.to_str().unwrap();

    let single = run(&["replay", "--rpc.replay-file", cache, "--json", TX_OK]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        single.code(),
        3,
        "a pre-block beacon-root miss exits 3.\nstderr: {}",
        single.stderr
    );
    let error = single.error_object();
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("beacon root contract call") &&
            message.contains("RPC error:") &&
            message.contains("cache miss"),
        "the message must name the stringified beacon-root path and the miss: {error}"
    );
}

/// A parent block that does not link to the replayed block is an unanswered
/// question, not a wrong answer: the single-transaction run exits 3, with or
/// without `--verify-receipt`.
///
/// The block and its parent are fetched by number in two separate calls, so a
/// reorg (or a load-balanced endpoint serving divergent views) can answer them
/// from different chains. Replaying anyway would fork from a pre-state that does
/// not precede the block, and the divergence would surface later as a receipt
/// mismatch (exit 2) or as a silently wrong replay.
#[test]
fn test_unlinked_parent_block_is_an_rpc_failure() {
    const WRONG_PARENT: &str = "0x1111111111111111111111111111111111111111111111111111111111111111";

    let (path, expected_parent) = cache_with_unlinked_parent("unlinked_parent", WRONG_PARENT);
    let cache = path.to_str().unwrap();

    for extra in [&[][..], &["--verify-receipt"][..]] {
        let mut argv = vec!["replay", "--rpc.replay-file", cache, "--json"];
        argv.extend_from_slice(extra);
        argv.push(TX_OK);
        let outcome = run(&argv);

        assert_eq!(
            outcome.code(),
            3,
            "a broken parent linkage exits 3 for {extra:?}.\nstderr: {}",
            outcome.stderr
        );
        let error = outcome.error_object();
        assert_eq!(error["error"]["code"].as_u64(), Some(3));
        assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
        let message = error["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(WRONG_PARENT) && message.contains(&expected_parent),
            "the message must name both hashes (parent {expected_parent}, served \
             {WRONG_PARENT}): {error}"
        );
        assert!(message.contains("divergent views"), "the message must name the cause: {error}");
        assert!(
            !outcome.stdout.contains("MISMATCH") &&
                !outcome.stderr.contains("verification mismatch"),
            "an unanswered question must not be reported as a mismatch:\n{}\n{}",
            outcome.stdout,
            outcome.stderr,
        );
    }

    let _ = std::fs::remove_file(&path);
}

/// A block whose hash is not the one the target was resolved as included in is
/// an unanswered question: the single-transaction run exits 3 rather than
/// replaying the target against a block it never ran in.
///
/// The parent linkage can hold while both numbered fetches answer from a
/// replacement block, so the linkage guard alone does not anchor the target.
#[test]
fn test_block_that_does_not_match_the_reported_inclusion_is_an_rpc_failure() {
    const WRONG_INCLUSION: &str =
        "0x2222222222222222222222222222222222222222222222222222222222222222";

    let (path, served) =
        cache_with_inclusion_hash("wrong_inclusion", serde_json::json!(WRONG_INCLUSION));
    let run = run(&["replay", "--rpc.replay-file", path.to_str().unwrap(), "--json", TX_OK]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 3, "a divergent inclusion exits 3.\nstderr: {}", run.stderr);
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(WRONG_INCLUSION) && message.contains(&served),
        "the message must name both hashes (served {served}, reported {WRONG_INCLUSION}): {error}"
    );
    assert!(message.contains("divergent views"), "the message must name the cause: {error}");
}

/// A block body that does not list the target is an unanswered question too: the
/// run exits 3 instead of treating every transaction of the block as preceding
/// and executing the target after the whole block.
#[test]
fn test_target_absent_from_the_block_body_is_an_rpc_failure() {
    let (path, block_hash) = cache_without_target_in_block_body("absent_target");
    let run = run(&["replay", "--rpc.replay-file", path.to_str().unwrap(), "--json", TX_OK]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 3, "a target absent from the body exits 3.\nstderr: {}", run.stderr);
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(TX_OK) && message.contains(&block_hash),
        "the message must name the target and the block it is missing from: {error}"
    );
    assert!(
        !run.stdout.contains("\"success\""),
        "the run must not produce an execution summary:\n{}",
        run.stdout
    );
}

/// A mined lookup carrying no inclusion hash is an unanchored view: the block
/// number alone cannot prove which body the target belongs to, so the run exits
/// 3 — the same class the batch driver rejects it with.
#[test]
fn test_mined_target_without_an_inclusion_hash_is_an_rpc_failure() {
    let (path, _) = cache_with_inclusion_hash("unanchored", serde_json::Value::Null);
    let run = run(&["replay", "--rpc.replay-file", path.to_str().unwrap(), "--json", TX_OK]);
    let _ = std::fs::remove_file(&path);

    assert_eq!(run.code(), 3, "an unanchored view exits 3.\nstderr: {}", run.stderr);
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("inclusion hash") && message.contains("unanchored"),
        "the message must name the unanchored view: {error}"
    );
    assert!(
        message.contains(&BLOCK_NUMBER.to_string()),
        "the message must name the block number the lookup reported: {error}"
    );
}

/// A batch run's error object follows the per-target lines, so a parser reading
/// the stream sees every target before the run-level verdict.
#[test]
fn test_batch_error_object_follows_the_per_target_lines() {
    let list = tx_file("batch_miss", &format!("{UNANSWERABLE_TX}\n"));

    let run = replay(&["--tx-file", list.to_str().unwrap(), "--json"]);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 3, "an unanswered target exits 3.\nstderr: {}", run.stderr);
    let values = common::json_values(&run.stdout);
    assert_eq!(values.len(), 2, "one per-target line plus the error object:\n{}", run.stdout);
    assert_eq!(
        values[0]["tx_hash"].as_str(),
        Some(UNANSWERABLE_TX),
        "the per-target line comes first: {}",
        values[0]
    );
    assert!(common::is_run_error(&values[1]), "the error object comes last: {}", values[1]);
}

/// Human mode reports the failure once, as `Display` text, and leaves stdout
/// untouched.
#[test]
fn test_human_failure_prints_exactly_one_error_line() {
    let list = tx_file("human", "not-a-hash\n");

    let run = replay(&["--tx-file", list.to_str().unwrap()]);
    let _ = std::fs::remove_file(&list);

    assert_eq!(run.code(), 1);
    assert_eq!(run.stderr.lines().count(), 1, "exactly one line on stderr:\n{}", run.stderr);
    assert!(run.stderr.starts_with("error: "), "the report is prefixed:\n{}", run.stderr);
    assert!(
        !run.stderr.contains("Evme(") && !run.stderr.contains("InvalidInput("),
        "the report must be Display-formatted, not Debug:\n{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "human mode prints no failure on stdout:\n{}", run.stdout);
}

/// A message that carries its own extra lines (the RPC hint) is still reported
/// exactly once, and never in `Debug` form.
#[test]
fn test_human_failure_reports_a_multi_line_message_once() {
    let run = replay(&[UNANSWERABLE_TX]);

    assert_eq!(run.code(), 3);
    assert_eq!(run.error_lines(), 1, "exactly one report on stderr:\n{}", run.stderr);
    assert!(
        !run.stderr.contains("Evme(") && !run.stderr.contains("RpcError("),
        "the report must be Display-formatted, not Debug:\n{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "human mode prints no failure on stdout:\n{}", run.stdout);
}

/// A successful run exits 0 and prints no error object: the failure surface
/// leaves the success output untouched.
#[test]
fn test_successful_run_exits_zero_without_an_error_object() {
    let run = replay(&["--json", TX_OK]);

    assert_eq!(run.code(), 0, "a faithful replay exits 0.\nstderr: {}", run.stderr);
    let values = common::json_values(&run.stdout);
    assert_eq!(values.len(), 1, "only the summary is printed:\n{}", run.stdout);
    assert!(!common::is_run_error(&values[0]), "a successful run prints no error object");
    assert_eq!(run.error_lines(), 0, "a successful run reports nothing on stderr");
}

/// A capture that could not be persisted is reported even when the run it was
/// capturing also failed: the run error keeps the exit code (it is the root
/// cause), and both failures are named on stderr without `-v`, so a stale or
/// missing capture file cannot go unnoticed.
#[tokio::test(flavor = "multi_thread")]
async fn test_capture_persist_failure_is_reported_next_to_the_run_error() {
    let server = common::MockRpcServer::start().await;
    // Chain id resolves, every other call fails: the replay itself goes
    // unanswered while the capture store still has entries to write.
    server.respond_eth_chain_id(6342, 1).await;
    server.respond_status_always(500).await;
    let url = server.uri();

    // A capture path whose parent is a regular file: persisting cannot succeed.
    let blocker =
        std::env::temp_dir().join(format!("mega_evme_capture_blocker_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&blocker);
    std::fs::write(&blocker, b"not a directory").expect("write blocker file");
    let capture = blocker.join("capture.json");

    let mut argv = vec![
        "replay",
        "--rpc",
        &url,
        "--rpc.capture-file",
        capture.to_str().unwrap(),
        "--rpc.max-retries",
        "0",
        "--rpc.backoff-ms",
        "1",
        TX_OK,
    ];
    let human = run(&argv);
    argv.push("--json");
    let json = run(&argv);
    let _ = std::fs::remove_file(&blocker);

    for run in [&human, &json] {
        assert_eq!(run.code(), 3, "the run error keeps the exit code.\nstderr: {}", run.stderr);
        assert_eq!(run.error_lines(), 2, "both failures are reported:\n{}", run.stderr);
        assert!(
            run.stderr.contains("Failed to fetch transaction"),
            "the run error must be reported:\n{}",
            run.stderr
        );
        assert!(
            run.stderr.contains(blocker.to_str().expect("blocker path is utf-8")),
            "the persist failure must name where the capture could not be written:\n{}",
            run.stderr
        );
    }

    // The structured object still reports the run error, which owns the code.
    let error = json.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Failed to fetch transaction")),
        "the object carries the run error, not the persist failure: {error}"
    );
}

/// A usage error is bad input, so it joins the execution class instead of
/// colliding with the mismatch code; `--help` stays a successful run.
#[test]
fn test_usage_errors_exit_one_and_help_exits_zero() {
    // No replay target: rejected by argument parsing.
    let usage = run(&["replay"]);
    assert_eq!(usage.code(), 1, "a usage error exits 1.\nstderr: {}", usage.stderr);
    assert!(usage.stdout.is_empty(), "a usage error prints nothing on stdout");

    let help = run(&["--help"]);
    assert_eq!(help.code(), 0, "--help exits 0");
    assert!(help.stdout.contains("mega-evme"), "--help prints usage on stdout");
}

/// A usage error of a `--json` run still ends stdout with the structured error
/// object: argument parsing fails before the command exists, but a
/// machine-readable run must never end with empty stdout.
#[test]
fn test_usage_error_in_json_mode_ends_stdout_with_the_error_object() {
    // No replay target: rejected by argument parsing.
    let usage = run(&["replay", "--json"]);

    assert_eq!(usage.code(), 1, "a usage error exits 1.\nstderr: {}", usage.stderr);
    let error = usage.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(1));
    assert_eq!(error["error"]["kind"].as_str(), Some("execution-error"));
    let message = error["error"]["message"].as_str().expect("the object carries a message");
    assert!(!message.contains('\n'), "the message is a single line: {message}");
    assert!(
        message.contains("required arguments"),
        "the message must summarize the usage error: {message}"
    );
    // clap keeps rendering its own report, including the usage block.
    assert!(usage.stderr.contains("Usage:"), "clap still reports on stderr:\n{}", usage.stderr);
}

/// `--help` in a `--json` run is still not a failure: no error object, exit 0.
#[test]
fn test_help_in_json_mode_prints_no_error_object() {
    let help = run(&["--help", "--json"]);

    assert_eq!(help.code(), 0, "--help exits 0");
    assert!(
        !help.stdout.lines().any(|line| line.trim_start().starts_with(r#"{"error""#)),
        "--help prints no error object:\n{}",
        help.stdout
    );
}

/// Closing stdout mid-batch must not abort the process.
///
/// Rust ignores SIGPIPE, so the next NDJSON `println!` panics with a broken
/// pipe. The panic hook still has to reach `exit(1)` even when it cannot write
/// the structured error object to the same closed stdout — otherwise the
/// runtime aborts (SIGABRT, shell status 134) and scripts that branch on the
/// documented 0/1/2/3 exit classes see an undefined status.
#[test]
fn test_closed_stdout_during_json_batch_exits_one() {
    use std::{
        io::{BufRead, BufReader, Read},
        process::{Command, Stdio},
        thread,
    };

    // Multi-target offline batch: many NDJSON lines, so dropping the pipe after
    // the first line still leaves further writes that hit the broken pipe.
    let envelope = common::fixture("replay_batch_blocks.cache.json");
    let mut child = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args([
            "replay",
            "--rpc.replay-file",
            envelope.to_str().expect("fixture path is utf-8"),
            "--block",
            "22945844",
            "--json",
        ])
        // Bound panic-hook stderr volume regardless of the ambient env: a full
        // backtrace can fill the pipe buffer and deadlock child-vs-`wait()` if
        // stderr is never drained. We still drain (below) so a caller that
        // exports `RUST_BACKTRACE=full` cannot hang this test either.
        .env("RUST_BACKTRACE", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn mega-evme");

    // Drain stderr on a reader thread before any wait, so a noisy panic hook
    // cannot fill the pipe and stall the child forever.
    let stderr = child.stderr.take().expect("child stderr was piped");
    let stderr_drain = thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut sink);
        sink
    });

    let stdout = child.stdout.take().expect("child stdout was piped");
    let mut first_line = String::new();
    BufReader::new(stdout)
        .read_line(&mut first_line)
        .expect("failed to read the first NDJSON line");
    assert!(
        !first_line.trim().is_empty(),
        "batch --json must print at least one NDJSON line before further writes"
    );
    // Dropping the BufReader closes the read end. The child's next stdout write
    // then fails with EPIPE and panics into the process-wide hook.
    // (Binding ends here; no further use of the pipe.)

    let status = child.wait().expect("failed to wait for mega-evme");
    let _stderr_bytes = stderr_drain.join().expect("stderr drain thread panicked");
    assert_eq!(
        status.code(),
        Some(1),
        "closed stdout must exit 1 (execution-error), not signal death.\nstatus: {status:?}"
    );
    assert!(
        status.code().is_some(),
        "process must not be signal-killed (e.g. SIGABRT from a double panic in the hook)"
    );
}

/// A panic under `--json` with an open stdout ends the stream with the standard
/// error envelope (`code: 1`, `kind: "execution-error"`).
///
/// The closed-stdout case only proves `exit(1)` when the hook cannot write.
/// This test pins the machine-readable object the hook prints when stdout is
/// still open. Triggered via the test-only `MEGA_EVME_INJECT_PANIC` hook (same
/// `test-utils` gate as the fixture pre-state inject), not via invalid input.
#[test]
fn test_panic_under_json_prints_execution_error_envelope() {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["--json"])
        .env("MEGA_EVME_INJECT_PANIC", "1")
        .env("RUST_BACKTRACE", "0")
        .output()
        .expect("failed to run mega-evme");

    assert_eq!(
        output.status.code(),
        Some(1),
        "injected panic must exit 1.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    let values = common::json_values(&stdout);
    let last = values
        .last()
        .unwrap_or_else(|| panic!("panic under --json must not leave stdout empty:\n{stdout}"));
    assert!(
        common::is_run_error(last),
        "the final stdout line must be the run-level error object, got: {last}"
    );
    assert_eq!(last["error"]["code"].as_u64(), Some(1));
    assert_eq!(last["error"]["kind"].as_str(), Some("execution-error"));
    let message = last["error"]["message"].as_str().expect("error.message must be a string");
    assert!(
        message.starts_with("panic: "),
        "panic-hook message must keep the `panic: …` prefix: {message}"
    );
}
