//! Integration tests for the pending-transaction single-transaction replay path
//! and for the target-metadata classification that decides who enters it.
//!
//! A pending target has no parent/block pair: its state base *is* the latest
//! block, which is also the block it is replayed in. The two roles must
//! therefore be filled by one and the same block, and these tests pin that from
//! outside the process — an endpoint that changes its answer between two calls
//! at the same height must not be able to produce a mixed-view replay.
//!
//! Only a target reporting neither a block number nor an inclusion hash is
//! pending. The other `(block_number, block_hash)` shapes are classified from the
//! metadata alone, before any block is fetched, and these tests pin that too by
//! counting the requests the endpoint receives.
//!
//! They run against a mock JSON-RPC endpoint rather than a recorded capture. An
//! offline capture cannot represent this case at all: identical requests are
//! served from the same keyed entry, so one fetch and two fetches are
//! indistinguishable offline, and a capture recorded for a mined replay answers
//! its state reads at the parent height while a pending replay reads them at the
//! latest one.

use serde_json::{json, Value};

mod common;
use common::{
    mock_chain::{self as chain, pending_tx_json, tx_identity, tx_json, CHAIN_ID, RECIPIENT},
    MockRpcServer, Run,
};

/// Height the endpoint reports as `latest`, and the only block it serves.
const LATEST: u64 = chain::BLOCK;

/// `parentHash` of the block the endpoint serves first for `LATEST`.
const PARENT_HASH: &str = "0xd482d481e9d11dd116ef6c41bf95ca608f159206c8f07900b1b53936d196ccb3";

/// `parentHash` of the replacement block: a different chain, as a reorg would
/// leave it.
const REPLACEMENT_PARENT_HASH: &str =
    "0x4444444444444444444444444444444444444444444444444444444444444444";

/// Hash of the block the endpoint serves first for `LATEST`.
///
/// The replay authenticates every block header it fetches against the hash the
/// endpoint reports beside it, so the mock cannot serve an invented block hash
/// any more than an invented transaction hash: each block is sealed under the
/// hash its own header produces.
fn latest_hash() -> String {
    common::block_hash_of(&block_json(PARENT_HASH))
}

/// The block the endpoint serves for `LATEST`, descending from `parent_hash`.
/// The two views the endpoint serves for `LATEST` differ only in the chain they
/// descend from.
fn block_json(parent_hash: &str) -> Value {
    chain::block_json(LATEST, parent_hash, json!([]))
}

/// A mock endpoint holding one pending transaction, whose `latest` height is
/// answered with the [`PARENT_HASH`] chain once and with the
/// [`REPLACEMENT_PARENT_HASH`] chain from the second call on.
async fn mock_chain() -> MockRpcServer {
    mock_chain_serving(pending_tx_json()).await
}

/// A mock endpoint that resolves the target to `tx`, and otherwise behaves like
/// [`mock_chain`]: the `latest` height is answered with the [`PARENT_HASH`]
/// chain once and with the [`REPLACEMENT_PARENT_HASH`] chain from the second
/// call on.
///
/// Account reads are answered blanket: every account holds 1 ETH, has nonce 0,
/// no code, and zero storage.
async fn mock_chain_serving(tx: Value) -> MockRpcServer {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(CHAIN_ID, 1).await;
    server.respond_method_result("eth_blockNumber", &format!("0x{LATEST:x}"), 2).await;
    server
        .respond_method_params_json_n_times(
            "eth_getBlockByNumber",
            json!([format!("0x{LATEST:x}"), false]),
            block_json(PARENT_HASH),
            1,
            2,
        )
        .await;
    server
        .respond_method_json("eth_getBlockByNumber", block_json(REPLACEMENT_PARENT_HASH), 3)
        .await;
    server.respond_method_json("eth_getTransactionByHash", tx, 3).await;
    chain::respond_account_reads(&server, 4).await;
    server
}

/// Replay the mock's pending transaction.
fn replay(server: &MockRpcServer) -> Run {
    replay_with(server, &[])
}

/// Replay the mock's pending transaction with `extra` flags.
fn replay_with(server: &MockRpcServer, extra: &[&str]) -> Run {
    common::replay_online(&server.uri(), &tx_identity().0, extra)
}

/// A pending replay fetches the latest block exactly once and fills both the
/// state-base and the replayed-block role from that one answer.
///
/// The endpoint changes its answer for the same height after the first call, so
/// a second fetch would hand the run a replacement block: the pre-state would
/// come from one view and the block environment from the other, and the run
/// would still exit 0 while reporting a receipt anchored to a block it never
/// forked from. One fetch removes that possibility structurally rather than
/// detecting it afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn test_pending_replay_fetches_the_latest_block_once() {
    let server = mock_chain().await;

    let run = replay(&server);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        server.received_method_count("eth_getBlockByNumber").await,
        1,
        "the two roles must be filled by a single fetch:\n{}",
        run.stdout,
    );
    let receipt = &run.summary()["receipt"];
    assert_eq!(
        receipt["blockHash"].as_str(),
        Some(latest_hash().as_str()),
        "the replay must report the block it forked from, not the replacement: {receipt}",
    );
    assert_eq!(receipt["blockNumber"].as_str(), Some(format!("0x{LATEST:x}").as_str()));
}

/// A pending target still replays against a coherent endpoint: the reused block
/// fills both roles, so the transaction executes on top of the latest block and
/// reports its result there.
#[tokio::test(flavor = "multi_thread")]
async fn test_pending_replay_executes_against_the_latest_block() {
    let server = mock_chain().await;

    let run = replay(&server);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let summary = run.summary();
    assert_eq!(summary["success"], json!(true), "the pending transaction must execute: {summary}");
    assert_eq!(
        summary["receipt"]["transactionHash"].as_str(),
        Some(tx_identity().0.as_str()),
        "the receipt must describe the replayed transaction: {summary}",
    );
}

/// An inclusion hash paired with a null block number is contradictory metadata,
/// not a pending transaction: the hash proves inclusion while the null number
/// denies it. The run answers that from the metadata alone — exit 3 without a
/// single block fetch — rather than reading the null number as "pending",
/// skipping every inclusion and body guard, and replaying the target against
/// latest with exit 0.
#[tokio::test(flavor = "multi_thread")]
async fn test_inclusion_hash_without_a_block_number_is_rejected_before_any_fetch() {
    const INCLUSION: &str = "0x5555555555555555555555555555555555555555555555555555555555555555";

    let server = mock_chain_serving(tx_json(Value::Null, json!(INCLUSION))).await;

    let run = replay(&server);

    assert_eq!(
        run.code,
        Some(3),
        "contradictory metadata exits 3.\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr,
    );
    assert_eq!(
        server.received_method_count("eth_getBlockByNumber").await,
        0,
        "the verdict must precede every block fetch:\n{}",
        run.stdout,
    );
    assert_eq!(
        server.received_method_count("eth_blockNumber").await,
        0,
        "the verdict must precede the latest-height lookup too:\n{}",
        run.stdout,
    );
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(INCLUSION) && message.contains("contradictory metadata"),
        "the message must name the hash and the contradiction: {error}"
    );
    assert!(
        !run.stdout.contains("\"success\""),
        "the run must not produce an execution summary:\n{}",
        run.stdout,
    );
}

/// A target resolved into block 0 is contradictory endpoint data: the genesis
/// block has no parent to fork from. Same guard and exit class as the batch
/// path — and the `n - 1` state-base computation must never run (in a debug
/// build it would underflow).
#[tokio::test(flavor = "multi_thread")]
async fn test_target_resolved_into_block_zero_is_contradictory_endpoint_data() {
    const GENESIS_HASH: &str = "0x6666666666666666666666666666666666666666666666666666666666666666";

    let server = mock_chain_serving(tx_json(json!("0x0"), json!(GENESIS_HASH))).await;

    let run = replay(&server);

    assert_eq!(
        run.code,
        Some(3),
        "a block-0 inclusion claim exits 3.\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr,
    );
    let error = run.error_object();
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("block 0") && message.contains("no parent block"),
        "the message must name the block-0 contradiction: {error}"
    );
}

/// A block number paired with a null inclusion hash is an unanchored view: the
/// number alone cannot prove which block body the target belongs to. The run
/// answers that from the metadata alone — exit 3 without a single block fetch —
/// so neither a missing block nor a broken parent linkage can mask it.
#[tokio::test(flavor = "multi_thread")]
async fn test_mined_target_without_an_inclusion_hash_is_rejected_before_any_fetch() {
    let server = mock_chain_serving(tx_json(json!(format!("0x{LATEST:x}")), Value::Null)).await;

    let run = replay(&server);

    assert_eq!(
        run.code,
        Some(3),
        "an unanchored view exits 3.\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr,
    );
    assert_eq!(
        server.received_method_count("eth_getBlockByNumber").await,
        0,
        "the verdict must precede every block fetch:\n{}",
        run.stdout,
    );
    let error = run.error_object();
    assert_eq!(error["error"]["code"].as_u64(), Some(3));
    assert_eq!(error["error"]["kind"].as_str(), Some("rpc-failure"));
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("inclusion hash") && message.contains("unanchored"),
        "the message must name the unanchored view: {error}"
    );
    assert!(
        message.contains(&LATEST.to_string()),
        "the message must name the block number the lookup reported: {error}"
    );
}

/// A pending replay asks the endpoint for the target once.
///
/// The online cache never keeps pending metadata, so every lookup of the target
/// reaches the endpoint; the one the run resolves the target with is the only
/// one it makes, and the execution reuses that answer.
#[tokio::test(flavor = "multi_thread")]
async fn test_pending_replay_looks_the_target_up_once() {
    let server = mock_chain().await;

    let run = replay(&server);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        server.received_method_count("eth_getTransactionByHash").await,
        1,
        "the target must be looked up exactly once:\n{}",
        run.stdout,
    );
}

/// A pending replay traces its target like a mined one does.
#[tokio::test(flavor = "multi_thread")]
async fn test_pending_replay_traces_the_target() {
    let server = mock_chain().await;

    let run = replay_with(&server, &["--trace", "--tracer", "call"]);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let summary = run.summary();
    let trace = &summary["trace"];
    assert_eq!(trace["type"].as_str(), Some("CALL"), "the trace must be a call frame: {summary}");
    assert_eq!(
        trace["to"].as_str().map(str::to_lowercase),
        Some(RECIPIENT.to_lowercase()),
        "the trace must describe the target's call: {summary}",
    );
    assert_eq!(
        trace["from"].as_str().map(str::to_lowercase),
        Some(tx_identity().1),
        "the trace must describe the target's sender: {summary}",
    );
}

/// A pending replay executes its target as the transaction overrides rewrite
/// it.
#[tokio::test(flavor = "multi_thread")]
async fn test_pending_replay_applies_transaction_overrides() {
    // One endpoint per run: each answers its first `latest` fetch with the same
    // block.
    let plain = replay_with(&mock_chain().await, &["--dump"]);
    let overridden = replay_with(&mock_chain().await, &["--dump", "--override.value", "7"]);

    for run in [&plain, &overridden] {
        assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    }
    let balance = |run: &Run| {
        let summary = run.summary();
        let state = summary["state"].as_object().expect("--dump inlines the state").clone();
        let recipient = state
            .iter()
            .find(|(address, _)| address.eq_ignore_ascii_case(RECIPIENT))
            .map(|(_, account)| account.clone())
            .unwrap_or_else(|| panic!("the recipient must be in the state dump: {summary}"));
        alloy_primitives::U256::from_str_radix(
            recipient["balance"].as_str().expect("a hex balance").trim_start_matches("0x"),
            16,
        )
        .expect("a hex balance")
    };
    assert_eq!(
        balance(&overridden),
        balance(&plain) + alloy_primitives::U256::from(7),
        "the value override must reach the executed transaction"
    );
}
