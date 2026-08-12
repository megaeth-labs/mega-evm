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

use std::process::Command;

use serde_json::{json, Value};

mod common;
use common::MockRpcServer;

/// `MegaETH` mainnet, whose published schedule the replayed block runs under.
const CHAIN_ID: u64 = 4326;

/// Height the endpoint reports as `latest`, and the only block it serves.
const LATEST: u64 = 18_172_461;

/// A mainnet timestamp inside the `MiniRex` window.
const TIMESTAMP: u64 = 1_764_000_000;

/// Hash of the block the endpoint serves first for `LATEST`.
const LATEST_HASH: &str = "0x2801837c261826beb8047e46139dfc4eb93ab5b3196ce23f312d3c7658262a62";

/// Hash of the replacement block the endpoint serves for `LATEST` from the
/// second call on — the divergent view a second fetch would pick up.
const REPLACEMENT_HASH: &str = "0x3333333333333333333333333333333333333333333333333333333333333333";

/// Parent of `LATEST_HASH`, so its header is well formed.
const PARENT_HASH: &str = "0xd482d481e9d11dd116ef6c41bf95ca608f159206c8f07900b1b53936d196ccb3";

/// Parent of the replacement block: a different chain, as a reorg would leave it.
const REPLACEMENT_PARENT_HASH: &str =
    "0x4444444444444444444444444444444444444444444444444444444444444444";

/// Hash of the pending transaction being replayed.
const TX_HASH: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

/// Sender of the pending transaction, funded by the mock's blanket balance.
const SENDER: &str = "0x14112799a39f2905b901067d3cd4a1f63c1cebda";

/// Recipient of the pending transaction: an account with no code, so the call
/// succeeds without depending on any contract the mock does not serve.
const RECIPIENT: &str = "0x681e908b8ab57c49c74d770f369754ccc3e1ae09";

/// A block header the RPC backend and the replay accept, carrying only the
/// fields either of them reads.
fn block_json(hash: &str, parent_hash: &str) -> Value {
    json!({
        "hash": hash,
        "parentHash": parent_hash,
        "number": format!("0x{LATEST:x}"),
        "timestamp": format!("0x{TIMESTAMP:x}"),
        "gasLimit": "0x2540be400",
        "gasUsed": "0x0",
        "baseFeePerGas": "0xf4240",
        "blobGasUsed": "0x0",
        "excessBlobGas": "0x0",
        "difficulty": "0x0",
        "extraData": "0x00000000fa00000001",
        "logsBloom": format!("0x{}", "0".repeat(512)),
        "miner": "0x4200000000000000000000000000000000000011",
        "mixHash": "0x5cd8791a477b467456670744425e11d5bd91fd54575d6d3bf80d761ab39d957f",
        "nonce": "0x0000000000000000",
        "parentBeaconBlockRoot":
            "0x67123956bf748ccfcfa68f03531dd12c1c647f9f31cc91935ce4271fa7399e24",
        "receiptsRoot": "0x16fe124682128dd43a5da7f2cee0a3bf076deaf12682d19c656914bbea4615e3",
        "requestsHash": "0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
        "size": "0x43e7",
        "stateRoot": "0xa342aba318978654abcf7f09f9494ed271e2136040b628edacb6d384e9074416",
        "transactionsRoot": "0x2f3c5d0b0c4c8d34dd4e1c8bb4b4a4b6d6a2a3d3b8f6a9a2c1d0e9f8a7b6c5d4",
        "withdrawalsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
        "uncles": [],
        "withdrawals": [],
        "transactions": [],
    })
}

/// The replayed transaction, carrying the `(blockNumber, blockHash)` pair the
/// endpoint reports for it. Everything else is the same transaction, so a test
/// varies only the metadata the classification reads.
fn tx_json(block_number: Value, block_hash: Value) -> Value {
    json!({
        "type": "0x2",
        "chainId": format!("0x{CHAIN_ID:x}"),
        "nonce": "0x0",
        "gas": "0x249f0",
        "maxFeePerGas": "0x200b20",
        "maxPriorityFeePerGas": "0x186a0",
        "gasPrice": "0x10c8e0",
        "to": RECIPIENT,
        "value": "0x0",
        "accessList": [],
        "input": "0x",
        "r": "0xa19f0f1f52e2951452711b4f4aa5d177442c9a56abeb609b803fe2412ed24946",
        "s": "0x7af21777b2e7d91c745d0077ba2726ee1bb75ccf00039a6218d64fdced768491",
        "yParity": "0x0",
        "v": "0x0",
        "hash": TX_HASH,
        "from": SENDER,
        "blockHash": block_hash,
        "blockNumber": block_number,
        "transactionIndex": Value::Null,
    })
}

/// The replayed transaction, reported as pending: no block number and no
/// inclusion hash.
fn pending_tx_json() -> Value {
    tx_json(Value::Null, Value::Null)
}

/// A mock endpoint holding one pending transaction, whose `latest` height is
/// answered with [`LATEST_HASH`] once and with [`REPLACEMENT_HASH`] from the
/// second call on.
async fn mock_chain() -> MockRpcServer {
    mock_chain_serving(pending_tx_json()).await
}

/// A mock endpoint that resolves the target to `tx`, and otherwise behaves like
/// [`mock_chain`]: the `latest` height is answered with [`LATEST_HASH`] once and
/// with [`REPLACEMENT_HASH`] from the second call on.
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
            block_json(LATEST_HASH, PARENT_HASH),
            1,
            2,
        )
        .await;
    server
        .respond_method_json(
            "eth_getBlockByNumber",
            block_json(REPLACEMENT_HASH, REPLACEMENT_PARENT_HASH),
            3,
        )
        .await;
    server.respond_method_json("eth_getTransactionByHash", tx, 3).await;
    server.respond_method_result("eth_getBalance", "0xde0b6b3a7640000", 4).await;
    server.respond_method_result("eth_getTransactionCount", "0x0", 4).await;
    server.respond_method_result("eth_getCode", "0x", 4).await;
    server
        .respond_method_result(
            "eth_getStorageAt",
            "0x0000000000000000000000000000000000000000000000000000000000000000",
            4,
        )
        .await;
    server
}

/// Outcome of one `mega-evme replay` invocation.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The single `--json` summary the run printed.
    fn summary(&self) -> Value {
        let mut values = common::json_values(&self.stdout);
        if values.last().is_some_and(common::is_run_error) {
            values.pop();
        }
        assert_eq!(
            values.len(),
            1,
            "expected one summary on stdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr,
        );
        values.pop().expect("checked above")
    }

    /// The structured error object a failing `--json` run ends with.
    fn error_object(&self) -> Value {
        let values = common::json_values(&self.stdout);
        let last = values.last().unwrap_or_else(|| {
            panic!("a failing --json run must not leave stdout empty:\nstderr:\n{}", self.stderr)
        });
        assert!(
            common::is_run_error(last),
            "the last stdout value must be the error object, got: {last}"
        );
        last.clone()
    }
}

/// Replay the mock's pending transaction.
fn replay(server: &MockRpcServer) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["replay", TX_HASH, "--rpc", &server.uri()])
        .args(["--rpc.no-cache-file", "--rpc.max-retries", "0", "--rpc.backoff-ms", "1", "--json"])
        .output()
        .expect("failed to run mega-evme");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
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
        Some(LATEST_HASH),
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
        Some(TX_HASH),
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
