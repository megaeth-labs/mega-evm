//! What the online `--rpc.cache-dir` cache carries from one run to the next.
//!
//! The cache lives at the transport, so it sees every RPC method rather than
//! the handful a provider-level cache overrides. That makes two questions
//! observable from outside the process, and this file answers both by counting
//! the requests a mock endpoint receives across two runs that share one cache
//! directory:
//!
//! - **What must not survive a run.** An answer that is only true for "now" — a still-pending
//!   transaction's metadata, the chain tip — would otherwise be frozen into the file and served to
//!   every later run, with no in-tool recovery short of `--rpc.clear-cache`.
//! - **What must survive it.** Everything anchored to a fixed block: the block body, its parent,
//!   the target's mined metadata, its receipt, and the state reads. A warm run asks the endpoint
//!   for none of them.
//!
//! Counting is the only way to see this. In-process assertions cannot
//! distinguish "served from the file" from "fetched again", and an offline
//! capture cannot show it at all: identical requests are served from the same
//! keyed entry, so one fetch and two fetches look the same there.

use std::{path::Path, process::Command};

use serde_json::{json, Value};

mod common;
use common::MockRpcServer;

/// `MegaETH` mainnet, whose published schedule the replayed block runs under.
const CHAIN_ID: u64 = 4326;

/// Height of the block the mined target is in, and the height the endpoint
/// reports as `latest` for the pending target.
const BLOCK: u64 = 18_172_461;

/// A mainnet timestamp inside the `MiniRex` window.
const TIMESTAMP: u64 = 1_764_000_000;

/// `parentHash` of the parent block, so its header is well formed too.
const GRANDPARENT_HASH: &str = "0x4444444444444444444444444444444444444444444444444444444444444444";

/// Hash of the parent of [`BLOCK`], the block a mined replay forks from.
///
/// The replay authenticates every block header it fetches against the hash the
/// endpoint reports beside it, so the mock cannot serve an invented block hash
/// any more than an invented transaction hash: the chain is built from the
/// bottom up, each block sealed under the hash its own header produces.
fn parent_hash() -> String {
    common::block_hash_of(&block_json(BLOCK - 1, GRANDPARENT_HASH, json!([])))
}

/// Hash of [`BLOCK`], whose header names [`parent_hash`] as its parent.
///
/// The transaction list is not part of the consensus header, so the hash does
/// not depend on which body the mock serves under it.
fn block_hash() -> String {
    common::block_hash_of(&block_json(BLOCK, &parent_hash(), json!([])))
}

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
/// reports so `--verify-receipt` matches.
const GAS_USED: u64 = 21_000;

/// The authentic identity of the replayed transaction: `(hash, from)`.
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

/// A block header the RPC backend and the replay accept, sealed under the hash
/// its own consensus fields produce.
fn block_json(number: u64, parent_hash: &str, transactions: Value) -> Value {
    common::sealed_block(json!({
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
    }))
}

/// The replayed transaction, carrying whichever `(blockNumber, blockHash)` pair
/// the endpoint reports for it.
fn tx_json(block_number: Value, block_hash: Value) -> Value {
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
        "blockHash": block_hash,
        "blockNumber": block_number,
        "transactionIndex": if block_number.is_null() { Value::Null } else { json!("0x0") },
    })
}

/// The on-chain receipt of the mined transaction, reporting the facts the local
/// replay reproduces.
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
        "blockHash": block_hash(),
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

/// A mock endpoint that resolves the target to `tx` and otherwise serves one
/// chain: [`BLOCK`] and its parent, the target's receipt, and blanket account
/// reads (every account holds 1 ETH, has nonce 0, no code, and zero storage).
///
/// The mock's answers never change, so any count difference between two runs
/// comes from the cache and nothing else.
async fn mock_chain_serving(tx: Value) -> MockRpcServer {
    let (tx_hash, _) = tx_identity();
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(CHAIN_ID, 1).await;
    server.respond_method_result("eth_blockNumber", &format!("0x{BLOCK:x}"), 2).await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{BLOCK:x}"), false]),
            block_json(BLOCK, &parent_hash(), json!([tx_hash])),
            2,
        )
        .await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{:x}", BLOCK - 1), false]),
            block_json(BLOCK - 1, GRANDPARENT_HASH, json!([])),
            2,
        )
        .await;
    server.respond_method_json("eth_getTransactionByHash", tx, 3).await;
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

/// The mock with the target reported as mined in [`BLOCK`].
async fn mined_chain() -> MockRpcServer {
    mock_chain_serving(tx_json(json!(format!("0x{BLOCK:x}")), json!(block_hash()))).await
}

/// The mock with the target reported as pending: no block number, no inclusion hash.
async fn pending_chain() -> MockRpcServer {
    mock_chain_serving(tx_json(Value::Null, Value::Null)).await
}

/// Outcome of one `mega-evme replay` invocation.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Replay the mock's target through the on-disk cache at `cache_dir`.
fn replay(server: &MockRpcServer, cache_dir: &Path, extra: &[&str]) -> Run {
    let (tx_hash, _) = tx_identity();
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["replay", &tx_hash, "--rpc", &server.uri()])
        .args(["--rpc.cache-dir", cache_dir.to_str().expect("utf-8 cache dir")])
        .args(["--rpc.max-retries", "0", "--rpc.backoff-ms", "1", "--json"])
        .args(extra)
        .output()
        .expect("failed to run mega-evme");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

/// Per-method request counts, for the methods these tests reason about.
struct Counts {
    chain_id: usize,
    tx_by_hash: usize,
    block_number: usize,
    block_by_number: usize,
    receipt: usize,
    total: usize,
}

impl Counts {
    async fn take(server: &MockRpcServer) -> Self {
        Self {
            chain_id: server.received_method_count("eth_chainId").await,
            tx_by_hash: server.received_method_count("eth_getTransactionByHash").await,
            block_number: server.received_method_count("eth_blockNumber").await,
            block_by_number: server.received_method_count("eth_getBlockByNumber").await,
            receipt: server.received_method_count("eth_getTransactionReceipt").await,
            total: server.received_request_count().await,
        }
    }

    /// What the second run added on top of the first.
    fn since(&self, before: &Self) -> Self {
        Self {
            chain_id: self.chain_id - before.chain_id,
            tx_by_hash: self.tx_by_hash - before.tx_by_hash,
            block_number: self.block_number - before.block_number,
            block_by_number: self.block_by_number - before.block_by_number,
            receipt: self.receipt - before.receipt,
            total: self.total - before.total,
        }
    }
}

/// Request sequence of a single online replay, warm and cold.
///
/// This is the baseline the transport-level cache establishes. The counts that
/// changed when online caching moved down from the provider layer are the block
/// fetches: a provider-level cache overrode neither `eth_getBlockByNumber` nor
/// `eth_chainId`, so a warm run re-fetched the block and its parent every time
/// (old warm `eth_getBlockByNumber` = 2, new = 0). Everything else was already
/// covered by the overridden methods and is unchanged (`eth_getTransactionByHash`,
/// `eth_getTransactionReceipt` and the state reads: warm = 0 before and after).
///
/// The chain-id probe is the one request that must stay live in both worlds: it
/// runs on a bare provider so the cache can never authenticate itself.
#[tokio::test(flavor = "multi_thread")]
async fn test_online_replay_request_sequence() {
    let server = mined_chain().await;
    let dir = tempfile::tempdir().expect("tempdir");

    let cold_run = replay(&server, dir.path(), &["--verify-receipt"]);
    assert_eq!(
        cold_run.code,
        Some(0),
        "the cold run must replay.\nstdout:\n{}\nstderr:\n{}",
        cold_run.stdout,
        cold_run.stderr,
    );
    let cold = Counts::take(&server).await;
    assert_eq!(cold.chain_id, 1, "one live chain-id probe");
    assert_eq!(cold.tx_by_hash, 1, "the target is fetched once");
    assert_eq!(cold.block_by_number, 2, "the block and its parent");
    assert_eq!(cold.receipt, 1, "one receipt for --verify-receipt");
    assert_eq!(cold.block_number, 0, "a mined target never asks for the chain tip");

    let cache_file = dir.path().join(format!("rpc-cache-{CHAIN_ID}.json"));
    assert!(cache_file.is_file(), "the cold run must persist the cache");

    let warm_run = replay(&server, dir.path(), &["--verify-receipt"]);
    assert_eq!(
        warm_run.code,
        Some(0),
        "the warm run must replay too.\nstdout:\n{}\nstderr:\n{}",
        warm_run.stdout,
        warm_run.stderr,
    );
    let warm = Counts::take(&server).await.since(&cold);

    assert_eq!(warm.chain_id, 1, "the chain-id probe is never served from the cache");
    assert_eq!(warm.tx_by_hash, 0, "a mined target's metadata never changes again");
    assert_eq!(warm.block_by_number, 0, "the block and its parent come from the cache");
    assert_eq!(warm.receipt, 0, "the receipt comes from the cache");
    assert_eq!(
        warm.total, 1,
        "the chain-id probe is the only request a warm replay makes; \
         got {} requests.\nstdout:\n{}\nstderr:\n{}",
        warm.total, warm_run.stdout, warm_run.stderr,
    );
}

/// A run that saw the target while it was still pending must not leave that
/// answer behind for the next one.
///
/// The first run fails — `--verify-receipt` has no on-chain receipt to compare
/// against — and a failing run still persists its cache, which is exactly when
/// the trap would be set: pending metadata is a perfectly ordinary non-null
/// result, so a cache that keeps it reports the transaction as pending forever,
/// including long after it lands. The second run must therefore ask the endpoint
/// again. The same holds for the chain tip the pending path reads to pick its
/// block: `eth_blockNumber` is the answer to "where is the chain now", and
/// freezing it pins every later run to the height of the first.
///
/// The block fetched at that height is the control: it *is* anchored to a fixed
/// block, so it is cached and the warm run does not ask for it.
#[tokio::test(flavor = "multi_thread")]
async fn test_online_cache_keeps_no_pending_or_chain_tip_answer() {
    let server = pending_chain().await;
    let dir = tempfile::tempdir().expect("tempdir");

    let first = replay(&server, dir.path(), &["--verify-receipt"]);
    assert_eq!(
        first.code,
        Some(1),
        "--verify-receipt cannot verify a pending target.\nstdout:\n{}\nstderr:\n{}",
        first.stdout,
        first.stderr,
    );
    let cold = Counts::take(&server).await;
    assert_eq!(cold.tx_by_hash, 1, "the target was looked up");
    assert_eq!(cold.block_number, 1, "the pending path asked for the chain tip");
    assert_eq!(cold.block_by_number, 1, "and fetched the block at that height");

    let cache_file = dir.path().join(format!("rpc-cache-{CHAIN_ID}.json"));
    assert!(cache_file.is_file(), "a failing run still persists its cache");
    let persisted = std::fs::read_to_string(&cache_file).expect("read cache");
    assert!(
        !persisted.contains("\"blockNumber\":null"),
        "no pending metadata may reach the file:\n{persisted}",
    );

    let second = replay(&server, dir.path(), &["--verify-receipt"]);
    assert_eq!(second.code, Some(1), "stdout:\n{}\nstderr:\n{}", second.stdout, second.stderr);
    let warm = Counts::take(&server).await.since(&cold);

    assert_eq!(
        warm.tx_by_hash, 1,
        "the pending target must be looked up again, not read off disk.\nstdout:\n{}\nstderr:\n{}",
        second.stdout, second.stderr,
    );
    assert_eq!(
        warm.block_number, 1,
        "the chain tip must be asked again, not read off disk.\nstdout:\n{}\nstderr:\n{}",
        second.stdout, second.stderr,
    );
    assert_eq!(
        warm.block_by_number, 0,
        "the block at a fixed height is cached — the control that shows the two \
         counts above are a policy decision, not a cache that never worked",
    );
}
