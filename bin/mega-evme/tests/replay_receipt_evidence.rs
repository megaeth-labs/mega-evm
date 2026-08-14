//! Integration tests for the on-chain receipt evidence the single-transaction
//! replay path builds.
//!
//! `--dump-fixture` and `--verify-receipt` both need the target's on-chain
//! receipt: the dump anchors its fidelity gate to it, the verification compares
//! against it. Both must be answered by one and the same receipt — asking the
//! endpoint twice lets a reorg, or a load-balanced endpoint serving divergent
//! views, hand the two consumers different receipts, so the fixture would be
//! anchored to one on-chain execution while the verdict is derived from another.
//!
//! These tests run against a mock JSON-RPC endpoint and count the requests it
//! receives. An offline capture cannot show this at all: identical requests are
//! served from the same keyed entry, so one fetch and two fetches are
//! indistinguishable there.
//!
//! What the count sees is what crosses the process boundary. The provider's
//! in-memory LRU also serves a repeated identical request, so these tests pin
//! that a run asks the endpoint for the receipt exactly as often as it needs it
//! — never more, and never zero when a consumer needs one. That the two
//! consumers read one and the same admitted receipt is a property of the
//! evidence type they share, not something an endpoint can observe.

use std::process::Command;

use serde_json::{json, Value};

mod common;
use common::MockRpcServer;

/// `MegaETH` mainnet, whose published schedule the replayed block runs under.
const CHAIN_ID: u64 = 4326;

/// Height of the block the target is mined in.
const BLOCK: u64 = 18_172_461;

/// A mainnet timestamp inside the `MiniRex` window.
const TIMESTAMP: u64 = 1_764_000_000;

/// Hash of the block the target is mined in.
const BLOCK_HASH: &str = "0x2801837c261826beb8047e46139dfc4eb93ab5b3196ce23f312d3c7658262a62";

/// Hash of the parent of [`BLOCK_HASH`], the block the replay forks from.
const PARENT_HASH: &str = "0xd482d481e9d11dd116ef6c41bf95ca608f159206c8f07900b1b53936d196ccb3";

/// Parent of the parent block, so its header is well formed too.
const GRANDPARENT_HASH: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";

/// Signature of the replayed transaction: a fixed, well-formed secp256k1 pair.
///
/// The replay authenticates every served transaction — its hash is recomputed
/// from the encoding and its sender re-derived from the signature — so the mock
/// cannot serve invented `hash`/`from` constants; [`tx_identity`] computes the
/// authentic pair. The sender is whatever address this signature recovers to,
/// funded like every other account by the mock's blanket balance.
const SIG_R: &str = "0xa19f0f1f52e2951452711b4f4aa5d177442c9a56abeb609b803fe2412ed24946";
const SIG_S: &str = "0x7af21777b2e7d91c745d0077ba2726ee1bb75ccf00039a6218d64fdced768491";

/// Recipient of the replayed transaction: an account with no code, so the call
/// succeeds without depending on any contract the mock does not serve.
const RECIPIENT: &str = "0x681e908b8ab57c49c74d770f369754ccc3e1ae09";

/// Gas the plain value transfer uses, and therefore what the mock's receipt
/// reports so the fidelity gate and the verification both pass.
const GAS_USED: u64 = 21_000;

/// The authentic identity of the replayed transaction: `(hash, from)`.
///
/// Builds the same consensus object the replay will deserialize from
/// [`tx_json`], hashes its encoding, and recovers its signer — the two values
/// the replay authenticates the served answer against.
fn tx_identity() -> (String, String) {
    use mega_evm::{
        alloy_consensus::{transaction::SignerRecoverable, SignableTransaction, TxEip1559},
        op_alloy_consensus::OpTxEnvelope,
    };

    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: 0x249f0,
        max_fee_per_gas: 0x200b20,
        max_priority_fee_per_gas: 0x186a0,
        to: alloy_primitives::TxKind::Call(RECIPIENT.parse().expect("`to` is an address")),
        value: alloy_primitives::U256::ZERO,
        access_list: Default::default(),
        input: alloy_primitives::Bytes::new(),
    };
    let signature = alloy_primitives::Signature::new(
        SIG_R.parse().expect("r is a hex word"),
        SIG_S.parse().expect("s is a hex word"),
        false,
    );
    let signed = tx.into_signed(signature);
    let hash = format!("{:#x}", signed.hash());
    let from = OpTxEnvelope::Eip1559(signed).recover_signer().expect("signature recovers");
    (hash, format!("{from:#x}"))
}

/// A block header the RPC backend and the replay accept, carrying only the
/// fields either of them reads.
fn block_json(number: u64, hash: &str, parent_hash: &str, transactions: Value) -> Value {
    json!({
        "hash": hash,
        "parentHash": parent_hash,
        "number": format!("0x{number:x}"),
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
        "transactions": transactions,
    })
}

/// The replayed transaction, reported as mined in [`BLOCK`].
fn tx_json() -> Value {
    let (hash, from) = tx_identity();
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
        "r": SIG_R,
        "s": SIG_S,
        "yParity": "0x0",
        "v": "0x0",
        "hash": hash,
        "from": from,
        "blockHash": BLOCK_HASH,
        "blockNumber": format!("0x{BLOCK:x}"),
        "transactionIndex": "0x0",
    })
}

/// The on-chain receipt of the replayed transaction, reporting the facts the
/// local replay reproduces.
fn receipt_json() -> Value {
    let (hash, from) = tx_identity();
    json!({
        "type": "0x2",
        "status": "0x1",
        "cumulativeGasUsed": format!("0x{GAS_USED:x}"),
        "logs": [],
        "logsBloom": format!("0x{}", "0".repeat(512)),
        "transactionHash": hash,
        "transactionIndex": "0x0",
        "blockHash": BLOCK_HASH,
        "blockNumber": format!("0x{BLOCK:x}"),
        "gasUsed": format!("0x{GAS_USED:x}"),
        "effectiveGasPrice": "0x10c8e0",
        "from": from,
        "to": RECIPIENT,
        "contractAddress": null,
        "l1GasPrice": "0x0",
        "l1GasUsed": "0x0",
        "l1Fee": "0x0",
        "l1BaseFeeScalar": "0x0",
        "l1BlobBaseFee": "0x0",
        "l1BlobBaseFeeScalar": "0x0",
    })
}

/// A mock endpoint holding one mined transaction, its block, its parent block,
/// and its receipt.
///
/// Account reads are answered blanket: every account holds 1 ETH, has nonce 0,
/// no code, and zero storage.
async fn mock_chain() -> MockRpcServer {
    let (tx_hash, _) = tx_identity();
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(CHAIN_ID, 1).await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{BLOCK:x}"), false]),
            block_json(BLOCK, BLOCK_HASH, PARENT_HASH, json!([tx_hash])),
            2,
        )
        .await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{:x}", BLOCK - 1), false]),
            block_json(BLOCK - 1, PARENT_HASH, GRANDPARENT_HASH, json!([])),
            2,
        )
        .await;
    server.respond_method_json("eth_getTransactionByHash", tx_json(), 3).await;
    server.respond_method_json("eth_getTransactionReceipt", receipt_json(), 3).await;
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
}

/// Replay the mock's mined transaction with the given extra flags.
fn replay(server: &MockRpcServer, extra: &[&str]) -> Run {
    let (tx_hash, _) = tx_identity();
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["replay", &tx_hash, "--rpc", &server.uri()])
        .args(["--rpc.no-cache-file", "--rpc.max-retries", "0", "--rpc.backoff-ms", "1", "--json"])
        .args(extra)
        .output()
        .expect("failed to run mega-evme");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

/// Dumping a fixture and verifying the receipt in the same run fetches the
/// receipt exactly once, and both consumers are answered from it: the fixture is
/// written (its fidelity gate passed against the receipt's gas and status) and
/// the verification reports a match.
#[tokio::test(flavor = "multi_thread")]
async fn test_dump_and_verify_share_one_receipt_fetch() {
    let server = mock_chain().await;
    let scratch = tempfile::tempdir().expect("failed to create a scratch directory");
    let fixture_path = scratch.path().join("fixture.json");

    let run = replay(
        &server,
        &["--verify-receipt", "--dump-fixture", &fixture_path.display().to_string()],
    );

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        server.received_method_count("eth_getTransactionReceipt").await,
        1,
        "both consumers must share one fetched receipt:\n{}",
        run.stdout,
    );
    assert!(
        fixture_path.is_file(),
        "the fixture must be written:\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr,
    );
    assert_eq!(
        run.summary()["verification"],
        json!({ "match": true }),
        "the verification must be answered from the same receipt:\n{}",
        run.stdout,
    );
}

/// Verifying alone fetches the receipt exactly once too — the shared evidence
/// does not turn one consumer into two fetches.
#[tokio::test(flavor = "multi_thread")]
async fn test_verify_alone_fetches_the_receipt_once() {
    let server = mock_chain().await;

    let run = replay(&server, &["--verify-receipt"]);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        server.received_method_count("eth_getTransactionReceipt").await,
        1,
        "verification needs exactly one receipt:\n{}",
        run.stdout,
    );
}

/// A run that asks for neither the dump nor the verification never fetches a
/// receipt: the evidence is built only for the consumers that need it.
#[tokio::test(flavor = "multi_thread")]
async fn test_plain_replay_fetches_no_receipt() {
    let server = mock_chain().await;

    let run = replay(&server, &[]);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        server.received_method_count("eth_getTransactionReceipt").await,
        0,
        "a plain replay must not fetch the on-chain receipt:\n{}",
        run.stdout,
    );
}
