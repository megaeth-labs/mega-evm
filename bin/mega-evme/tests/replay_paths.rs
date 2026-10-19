//! A transaction replayed alone, over RPC, is the same transaction the block replay runs.
//!
//! `replay <TX_HASH>` forks the parent block's state lazily over RPC and runs the block's
//! earlier transactions first; `replay --block` runs the whole block on the pre-state recorded
//! with it. The two paths share no state source, so their agreeing on every transaction checked
//! here is evidence for both: on the legacy engine (the 1.7.1 CLI against the chain's receipts)
//! and on Satin (the in-tree port against the block driver).
//!
//! The RPC is a mock that answers only from the recorded block: a read the recording does not
//! hold is an error, not a guess. The two exceptions are what Satin's pre-block changes read and
//! no recording holds: the EIP-7997 factory, served as absent, and the L1 block contract's L1 fee
//! overhead, served as zero, as the chain held both. `block_replay.rs` shows neither changes a
//! transaction row.
#![cfg(feature = "legacy")]

mod common;

use std::sync::Arc;

use alloy_primitives::{address, Address, B256, U256};
use common::blocks::{
    cache_copy_for_satin, code_of, fixtures, is_deposit, overhead_slot, read_block, run_evme,
    satin_genesis, timestamp_of, FACTORY, L1_BLOCK,
};
use mega_evme::block::{CachedBlock, PreAccount};
use serde_json::{json, Value};
use wiremock::{matchers, Mock, MockServer, Respond, ResponseTemplate};

const BLOCK: u64 = 26_400_110;

/// Answers JSON-RPC from the recorded block alone.
struct Recording {
    block: CachedBlock,
    /// The parent block's timestamp; the block's own when not given.
    parent_timestamp: Option<u64>,
}

impl Recording {
    fn account(&self, address: Address) -> Option<PreAccount> {
        match self.block.prestate.accounts.get(&address) {
            Some(account) if address == L1_BLOCK => {
                let mut account = account.clone();
                account.storage.entry(overhead_slot()).or_insert(U256::ZERO);
                Some(account)
            }
            Some(account) => Some(account.clone()),
            None if address == FACTORY => Some(PreAccount::default()),
            None => None,
        }
    }

    fn parent(&self) -> Value {
        let mut parent = self.block.block.clone();
        parent["number"] = json!(format!("0x{:x}", BLOCK - 1));
        parent["hash"] = self.block.block["parentHash"].clone();
        parent["transactions"] = json!([]);
        if let Some(timestamp) = self.parent_timestamp {
            parent["timestamp"] = json!(format!("0x{timestamp:x}"));
        }
        parent
    }

    fn result(&self, method: &str, params: &[Value]) -> Result<Value, String> {
        let block_arg = |i: usize| params.get(i).and_then(Value::as_str).unwrap_or_default();
        let at_parent = |i: usize| block_arg(i) == format!("0x{:x}", BLOCK - 1);
        let address = || -> Result<Address, String> {
            serde_json::from_value(params[0].clone()).map_err(|e| e.to_string())
        };
        match method {
            "eth_chainId" => Ok(json!("0x10e6")),
            "eth_getTransactionByHash" => self.block.block["transactions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tx| tx["hash"] == params[0])
                .cloned()
                .ok_or_else(|| "unknown transaction".to_string()),
            "eth_getTransactionReceipt" => self
                .block
                .receipts
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["transactionHash"] == params[0])
                .cloned()
                .ok_or_else(|| "unknown receipt".to_string()),
            "eth_getBlockByNumber" if block_arg(0) == format!("0x{BLOCK:x}") => {
                let mut block = self.block.block.clone();
                if params.get(1) == Some(&json!(false)) {
                    let hashes: Vec<Value> = block["transactions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|tx| tx["hash"].clone())
                        .collect();
                    block["transactions"] = Value::Array(hashes);
                }
                Ok(block)
            }
            "eth_getBlockByNumber" if at_parent(0) => Ok(self.parent()),
            "eth_getBalance" | "eth_getTransactionCount" | "eth_getCode" if at_parent(1) => {
                let account = self.account(address()?).ok_or("account not recorded")?;
                Ok(match method {
                    "eth_getBalance" => json!(account.balance),
                    "eth_getTransactionCount" => json!(format!("0x{:x}", account.nonce)),
                    _ => json!(account
                        .code_hash
                        .map(|hash| alloy_primitives::hex::encode_prefixed(code_of(hash)))
                        .unwrap_or_else(|| "0x".to_string())),
                })
            }
            "eth_getStorageAt" if at_parent(2) => {
                let account = self.account(address()?).ok_or("account not recorded")?;
                let slot: U256 =
                    serde_json::from_value(params[1].clone()).map_err(|e| e.to_string())?;
                account
                    .storage
                    .get(&B256::from(slot))
                    .map(|value| json!(B256::from(*value)))
                    .ok_or_else(|| "slot not recorded".to_string())
            }
            _ => Err(format!("not recorded: {method} {params:?}")),
        }
    }
}

impl Respond for Recording {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let params = body["params"].as_array().cloned().unwrap_or_default();
        let reply = match self.result(body["method"].as_str().unwrap(), &params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": body["id"], "result": result }),
            Err(message) => json!({
                "jsonrpc": "2.0",
                "id": body["id"],
                "error": { "code": -32000, "message": message },
            }),
        };
        ResponseTemplate::new(200).set_body_json(reply)
    }
}

struct Server(Arc<MockServer>);

impl Server {
    async fn start(block: CachedBlock) -> Self {
        Self::with_parent_at(block, None).await
    }

    /// A server whose parent block is at `parent_timestamp`, or at the block's own timestamp.
    async fn with_parent_at(block: CachedBlock, parent_timestamp: Option<u64>) -> Self {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .respond_with(Recording { block, parent_timestamp })
            .mount(&server)
            .await;
        Self(Arc::new(server))
    }
}

/// The block replay's transaction records, on `spec`'s engine, under `limits` when given.
fn block_rows(spec: Option<&str>, limits: Option<&str>) -> Vec<Value> {
    let cache = cache_copy_for_satin();
    let mut args = vec!["replay", "--block", "26400110", "--json", "--block-cache"];
    args.push(cache.path().to_str().unwrap());
    if let Some(spec) = spec {
        args.extend(["--override.spec", spec]);
    }
    if let Some(limits) = limits {
        args.extend(["--override.limits", limits]);
    }
    let run = run_evme(&args);
    assert_eq!(run.code, 0, "{}", run.stderr);
    run.records().into_iter().filter(|record| record["kind"] == "tx").collect()
}

/// The transactions checked: the first user transaction, the plain value transfer, one in the
/// middle and the last the block ran before it refused one.
///
/// A Satin replay can refuse a transaction the chain included, for the block gas it declared.
/// The selection stops there: a later transaction of the same block may itself be refused for
/// the gas it declared, and this comparison is about transactions the block included. A replay
/// of one transaction skips a predecessor the executor refuses and still runs, which
/// [`test_a_replay_skips_a_refused_predecessor_and_keeps_its_state`] holds.
fn checked(rows: &[Value]) -> Vec<usize> {
    let ran = rows.iter().position(|row| row["status"] == "refused").unwrap_or(rows.len());
    let rows = &rows[..ran];
    let transfer = rows.iter().position(|row| row["logs"] == 0 && row["index"] != 0).unwrap_or(1);
    let mut picks = vec![1, transfer, rows.len() / 2, rows.len() - 1];
    picks.sort_unstable();
    picks.dedup();
    picks
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_transaction_replayed_alone_is_the_one_the_block_replay_runs() {
    let recorded = read_block(fixtures(), BLOCK);
    let server = Server::start(recorded).await;
    let uri = server.0.uri();

    // The last leg holds both paths to a transaction data-size limit of the body alone, which
    // stops every transaction but the protocol's own.
    let stopping = r#"{"txRuntimeLimits":{"txDataSizeLimit":310}}"#;
    let mut stops = 0;
    for (spec, engine, limits) in [
        (None, "legacy", None),
        (Some("Satin"), "satin", None),
        (Some("Satin"), "satin", Some(stopping)),
    ] {
        let rows = block_rows(spec, limits);
        for index in checked(&rows) {
            let row = &rows[index];
            let mut args = vec!["replay", row["hash"].as_str().unwrap(), "--rpc", &uri];
            args.extend(["--rpc.no-cache-file", "--rpc.max-retries", "0", "--json"]);
            if let Some(spec) = spec {
                args.extend(["--override.spec", spec]);
            }
            if let Some(limits) = limits {
                args.extend(["--override.limits", limits]);
                stops += usize::from(!row["satin"]["limit_exceeded"].is_null());
            }
            let run = run_evme(&args);
            assert_eq!(run.code, 0, "{engine} tx {index}: {}{}", run.stderr, run.stdout);
            let mut single: Value = serde_json::from_str(&run.stdout).unwrap();
            // A transaction replayed alone reports the limits an override held it to; a block's
            // record reports them for its transactions.
            let overridden = single
                .get_mut("satin")
                .and_then(|satin| satin.as_object_mut().unwrap().remove("limits_override"));
            assert_eq!(overridden.is_some(), limits.is_some(), "{engine} tx {index}");
            assert_eq!(single["gas_used"], row["gas_used"], "{engine} tx {index}");
            assert_eq!(single["success"], row["status"] == "success", "{engine} tx {index}");
            assert_eq!(single["logs_count"], row["logs"], "{engine} tx {index}");
            assert_eq!(
                single["receipt"]["cumulativeGasUsed"],
                json!(format!("0x{:x}", row["cumulative_gas_used"].as_u64().unwrap())),
                "{engine} tx {index}"
            );
            if engine == "satin" {
                assert_eq!(single["satin"], row["satin"], "Satin tx {index}: the ledgers agree");
            } else {
                assert!(single.get("satin").is_none(), "the legacy output is 1.7.1's");
                assert_eq!(row["differs"], json!([]), "legacy tx {index} matches the chain");
            }
        }
    }
    assert!(stops > 0, "the limits leg checks a stopped transaction");
}

/// [S1.8] [S21.12] [S1] A transaction replay is told whether its block admits only deposits as a
/// node is, from the schedule and the parent block it forks. Under a genesis file that activates
/// Satin at the block's timestamp, with the parent a second earlier, the block is Satin's
/// activation block: a user transaction of it is refused for the rule, and its deposit replays.
/// With the parent at the block's own timestamp the user transaction replays.
///
/// Expected values: independent.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_transaction_replay_refuses_a_user_transaction_in_an_activation_block() {
    let recorded = read_block(fixtures(), BLOCK);
    let timestamp = timestamp_of(&recorded);
    assert!(is_deposit(&recorded, 0) && !is_deposit(&recorded, 1));
    let dir = tempfile::tempdir().unwrap();
    let genesis = satin_genesis(dir.path(), mega_evm::MAINNET_CHAIN_ID, timestamp);
    let hash = |index: usize| recorded.block["transactions"][index]["hash"].as_str().unwrap();

    for (parent_timestamp, activation) in [(timestamp - 1, true), (timestamp, false)] {
        let server =
            Server::with_parent_at(read_block(fixtures(), BLOCK), Some(parent_timestamp)).await;
        let uri = server.0.uri();
        let run = |index: usize| {
            run_evme(&[
                "replay",
                hash(index),
                "--rpc",
                &uri,
                "--rpc.no-cache-file",
                "--rpc.max-retries",
                "0",
                "--json",
                "--genesis",
                &genesis,
            ])
        };
        let deposit = run(0);
        assert_eq!(deposit.code, 0, "the deposit replays: {}{}", deposit.stderr, deposit.stdout);
        let user = run(1);
        if activation {
            assert_ne!(user.code, 0, "{}", user.stdout);
            let output = format!("{}{}", user.stdout, user.stderr);
            assert!(
                output.contains("non-deposit transaction in fork activation block"),
                "{output}"
            );
        } else {
            assert_eq!(user.code, 0, "the block after: {}{}", user.stderr, user.stdout);
        }
    }
}

/// A replay of one transaction runs it on the state the block's included predecessors left,
/// and skips a predecessor the block executor refuses.
///
/// The recorded block supplies the parent state and the header. Its transactions are replaced
/// by three calls: a value transfer, a call whose gas limit is below Satin's intrinsic cost, and
/// a later transfer that spends part of what the first one credited. The middle call is the
/// refusal. The last one succeeds only when the first one's credit is in the state it runs on,
/// and its receipt index counts the included predecessor alone.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_replay_skips_a_refused_predecessor_and_keeps_its_state() {
    const PAYER: Address = address!("0x1111111111111111111111111111111111111111");
    const REFUSED: Address = address!("0x2222222222222222222222222222222222222222");
    const RECIPIENT: Address = address!("0x3333333333333333333333333333333333333333");
    /// One ether, the value the included predecessor credits.
    const GIFT: u128 = 1_000_000_000_000_000_000;
    /// Room above a value transfer that creates an account, at the spec's byte prices.
    const GAS: u64 = 5_000_000;

    let mut recorded = read_block(fixtures(), BLOCK);
    let ether = U256::from(GIFT);
    recorded
        .prestate
        .accounts
        .insert(PAYER, PreAccount { balance: ether * U256::from(10), ..PreAccount::default() });
    recorded
        .prestate
        .accounts
        .insert(REFUSED, PreAccount { balance: ether, ..PreAccount::default() });
    recorded.prestate.accounts.insert(RECIPIENT, PreAccount::default());

    let block_hash = recorded.block["hash"].clone();
    let block_number = recorded.block["number"].clone();
    let tx = |hash: &str, index: u64, from: Address, to: Address, gas: u64, value: U256| {
        json!({
            "type": "0x2",
            "chainId": "0x10e6",
            "nonce": "0x0",
            "gas": format!("0x{gas:x}"),
            "maxFeePerGas": "0xf4240",
            "maxPriorityFeePerGas": "0x0",
            "to": to,
            "value": format!("0x{value:x}"),
            "accessList": [],
            "input": "0x",
            "r": "0x64c0524917585101b7ffb2286d4de676ca987554b33c6f9a37e7ad7508bfbb84",
            "s": "0xe5199cacc6e07c6a9f8c74d0b06f964ce36dd362199b1c19d866f1a6f52d510",
            "yParity": "0x1",
            "v": "0x1",
            "hash": hash,
            "blockHash": block_hash,
            "blockNumber": block_number,
            "transactionIndex": format!("0x{index:x}"),
            "from": from,
            "gasPrice": "0xf4240",
        })
    };
    let target = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    recorded.block["transactions"] = json!([
        tx(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            0,
            PAYER,
            RECIPIENT,
            GAS,
            ether,
        ),
        tx(
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            1,
            REFUSED,
            RECIPIENT,
            1,
            U256::ZERO,
        ),
        tx(target, 2, RECIPIENT, PAYER, GAS, U256::from(1)),
    ]);

    let server = Server::start(recorded).await;
    let uri = server.0.uri();
    let run = run_evme(&[
        "replay",
        target,
        "--rpc",
        &uri,
        "--rpc.no-cache-file",
        "--rpc.max-retries",
        "0",
        "--override.spec",
        "Satin",
        "--json",
        "--dump",
    ]);
    assert_eq!(run.code, 0, "{}{}", run.stderr, run.stdout);
    let summary: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(summary["success"], true, "{summary}");
    // The refused predecessor is not an included transaction. The value transfer is.
    assert_eq!(summary["receipt"]["transactionIndex"], "0x1", "{summary}");
    let cumulative = u64::from_str_radix(
        summary["receipt"]["cumulativeGasUsed"].as_str().unwrap().trim_start_matches("0x"),
        16,
    )
    .unwrap();
    assert!(cumulative > summary["gas_used"].as_u64().unwrap(), "{summary}");

    let state = &summary["state"];
    let account = |address: Address| &state[&address.to_string()];
    let balance = |address: Address| -> U256 {
        account(address)["balance"].as_str().unwrap().parse().unwrap()
    };
    let nonce = |address: Address| {
        u64::from_str_radix(
            account(address)["nonce"].as_str().unwrap().trim_start_matches("0x"),
            16,
        )
        .unwrap()
    };
    // The payer's nonce is the included predecessor's bump. The recipient spends one wei of the
    // ether that predecessor credited and keeps the rest.
    assert_eq!(nonce(PAYER), 1, "{state}");
    assert_eq!(nonce(RECIPIENT), 1, "{state}");
    let left = balance(RECIPIENT);
    assert!(left < ether && left > ether - U256::from(10u64.pow(16)), "{left} {state}");
}
