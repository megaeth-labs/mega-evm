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

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

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
    /// Code a test added to the recording, by hash.
    codes: HashMap<B256, Vec<u8>>,
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
                        .map(|hash| {
                            let code =
                                self.codes.get(&hash).cloned().unwrap_or_else(|| code_of(hash));
                            alloy_primitives::hex::encode_prefixed(code)
                        })
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
        Self::with_codes(block, parent_timestamp, HashMap::default()).await
    }

    /// A server that also serves `codes`, by hash, for accounts a test added to the block.
    async fn with_codes(
        block: CachedBlock,
        parent_timestamp: Option<u64>,
        codes: HashMap<B256, Vec<u8>>,
    ) -> Self {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .respond_with(Recording { block, parent_timestamp, codes })
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
/// middle, the first a limit stopped, the one at `held`, and the last the block ran before it
/// refused one.
///
/// A Satin replay can refuse a transaction the chain included, for the block gas it declared.
/// The selection stops there: a later transaction of the same block may itself be refused for
/// the gas it declared, and this comparison is about transactions the block included. A replay
/// of one transaction skips a predecessor the executor refuses and still runs, which
/// [`test_a_replay_skips_a_refused_predecessor_and_keeps_its_state`] holds.
fn checked(rows: &[Value], held: Option<usize>) -> Vec<usize> {
    let ran = rows.iter().position(|row| row["status"] == "refused").unwrap_or(rows.len());
    let rows = &rows[..ran];
    let transfer = rows.iter().position(|row| row["logs"] == 0 && row["index"] != 0).unwrap_or(1);
    let stopped = rows.iter().position(|row| !row["satin"]["limit_exceeded"].is_null());
    let mut picks = vec![1, transfer, rows.len() / 2, rows.len() - 1];
    picks.extend(stopped);
    picks.extend(held.filter(|held| *held < ran));
    picks.sort_unstable();
    picks.dedup();
    picks
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_transaction_replayed_alone_is_the_one_the_block_replay_runs() {
    let recorded = read_block(fixtures(), BLOCK);
    let server = Server::start(recorded).await;
    let uri = server.0.uri();

    // The last leg holds both paths to a transaction data-size limit one byte below the most a
    // transaction of the block keeps, which stops the frame of that transaction that crosses it:
    // the transaction keeps less than it did, whether that frame was its own or a caller caught
    // the stop.
    let unlimited = block_rows(Some("Satin"), None);
    let kept = |row: &Value| row["satin"]["data_size"].as_u64();
    let most = unlimited.iter().filter_map(kept).max().expect("the block keeps data");
    let heaviest = unlimited.iter().position(|row| kept(row) == Some(most));
    let holding = format!(r#"{{"txRuntimeLimits":{{"txDataSizeLimit":{}}}}}"#, most - 1);
    let mut held = 0;
    for (spec, engine, limits) in [
        (None, "legacy", None),
        (Some("Satin"), "satin", None),
        (Some("Satin"), "satin", Some(holding.as_str())),
    ] {
        let rows = block_rows(spec, limits);
        let heaviest = heaviest.filter(|_| limits.is_some());
        for index in checked(&rows, heaviest) {
            let row = &rows[index];
            let mut args = vec!["replay", row["hash"].as_str().unwrap(), "--rpc", &uri];
            args.extend(["--rpc.no-cache-file", "--rpc.max-retries", "0", "--json"]);
            if let Some(spec) = spec {
                args.extend(["--override.spec", spec]);
            }
            if let Some(limits) = limits {
                args.extend(["--override.limits", limits]);
                if heaviest == Some(index) {
                    assert!(kept(row).is_some_and(|kept| kept < most), "the limit held tx {index}");
                    held += 1;
                }
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
    assert_eq!(held, 1, "the limits leg checks the transaction the limit held");
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

/// A predecessor whose account the RPC cache cannot serve is a database failure.
///
/// The mock answers an unrecorded account with an error. That is not a validation refusal, so
/// the replay stops instead of running the target on the parent state.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_replay_stops_when_a_predecessor_account_cannot_be_read() {
    const PAYER: Address = address!("0x1111111111111111111111111111111111111111");
    const MISSING: Address = address!("0x4444444444444444444444444444444444444444");
    const RECIPIENT: Address = address!("0x3333333333333333333333333333333333333333");
    const GAS: u64 = 5_000_000;

    let mut recorded = read_block(fixtures(), BLOCK);
    assert!(
        !recorded.prestate.accounts.contains_key(&MISSING) && MISSING != FACTORY,
        "the predecessor's sender has to be an account the cache cannot serve"
    );
    recorded
        .prestate
        .accounts
        .insert(PAYER, PreAccount { balance: U256::from(10u128.pow(18)), ..PreAccount::default() });
    recorded.prestate.accounts.insert(RECIPIENT, PreAccount::default());

    let block_hash = recorded.block["hash"].clone();
    let block_number = recorded.block["number"].clone();
    let tx = |hash: &str, index: u64, from: Address, to: Address| {
        json!({
            "type": "0x2",
            "chainId": "0x10e6",
            "nonce": "0x0",
            "gas": format!("0x{GAS:x}"),
            "maxFeePerGas": "0xf4240",
            "maxPriorityFeePerGas": "0x0",
            "to": to,
            "value": "0x0",
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
    let target = "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    recorded.block["transactions"] = json!([
        tx(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            0,
            MISSING,
            RECIPIENT,
        ),
        tx(target, 1, PAYER, RECIPIENT),
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
    ]);
    assert_ne!(run.code, 0, "a failed predecessor read is not skipped: {}", run.stdout);
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(output.contains("Block execution error"), "{output}");
    assert!(output.contains("account not recorded"), "{output}");
}

/// A block of calls from one payer to contracts the test adds to the recorded block's parent
/// state, and the server that serves it. Each call is `(to, gas)`; the payer's nonce counts up.
struct SyntheticBlock {
    server: Server,
    /// Hash of the `index`-th call.
    hashes: Vec<String>,
}

const PAYER: Address = address!("0x1111111111111111111111111111111111111111");

impl SyntheticBlock {
    /// `contracts` are `(address, code, storage)` added to the parent state.
    async fn start(
        contracts: &[(Address, Vec<u8>, BTreeMap<B256, U256>)],
        calls: &[Address],
    ) -> Self {
        const GAS: u64 = 5_000_000;
        let mut recorded = read_block(fixtures(), BLOCK);
        recorded.prestate.accounts.insert(
            PAYER,
            PreAccount { balance: U256::from(10u128.pow(18)), ..PreAccount::default() },
        );
        let mut codes = HashMap::new();
        for (address, code, storage) in contracts {
            let code_hash = alloy_primitives::keccak256(code);
            codes.insert(code_hash, code.clone());
            recorded.prestate.accounts.insert(
                *address,
                PreAccount {
                    code_hash: Some(code_hash),
                    storage: storage.clone(),
                    ..Default::default()
                },
            );
        }
        let block_hash = recorded.block["hash"].clone();
        let block_number = recorded.block["number"].clone();
        let hashes: Vec<String> =
            (0..calls.len()).map(|i| format!("0x{:064x}", 0xa0 + i)).collect();
        recorded.block["transactions"] = Value::Array(
            calls
                .iter()
                .enumerate()
                .map(|(index, to)| {
                    json!({
                        "type": "0x2",
                        "chainId": "0x10e6",
                        "nonce": format!("0x{index:x}"),
                        "gas": format!("0x{GAS:x}"),
                        "maxFeePerGas": "0xf4240",
                        "maxPriorityFeePerGas": "0x0",
                        "to": to,
                        "value": "0x0",
                        "accessList": [],
                        "input": "0x",
                        "r": "0x64c0524917585101b7ffb2286d4de676ca987554b33c6f9a37e7ad7508bfbb84",
                        "s": "0xe5199cacc6e07c6a9f8c74d0b06f964ce36dd362199b1c19d866f1a6f52d510",
                        "yParity": "0x1",
                        "v": "0x1",
                        "hash": hashes[index],
                        "blockHash": block_hash,
                        "blockNumber": block_number,
                        "transactionIndex": format!("0x{index:x}"),
                        "from": PAYER,
                        "gasPrice": "0xf4240",
                    })
                })
                .collect(),
        );
        Self { server: Server::with_codes(recorded, None, codes).await, hashes }
    }

    /// Replays the `index`-th call alone on Satin, with `--dump`.
    fn replay(&self, index: usize) -> Value {
        let uri = self.server.0.uri();
        let run = run_evme(&[
            "replay",
            &self.hashes[index],
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
        serde_json::from_str(&run.stdout).unwrap()
    }
}

/// A replay of one transaction numbers its logs in the block: after the logs of the included
/// predecessors it ran first, and its receipt carries the transaction's index and hash.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_replayed_log_carries_the_block_level_index() {
    // PUSH0 PUSH0 LOG0 PUSH0 PUSH0 LOG0 STOP: two logs per call.
    let logger = address!("0x5555555555555555555555555555555555555555");
    let code = vec![0x5f, 0x5f, 0xa0, 0x5f, 0x5f, 0xa0, 0x00];
    let block =
        SyntheticBlock::start(&[(logger, code, BTreeMap::new())], &[logger, logger, logger]).await;

    for (index, first_log) in [(0usize, 0u64), (1, 2), (2, 4)] {
        let summary = block.replay(index);
        let logs = summary["receipt"]["logs"].as_array().unwrap();
        assert_eq!(logs.len(), 2, "{summary}");
        for (i, log) in logs.iter().enumerate() {
            assert_eq!(log["logIndex"], format!("0x{:x}", first_log + i as u64), "tx {index}");
            assert_eq!(log["transactionIndex"], format!("0x{index:x}"), "tx {index}");
            assert_eq!(log["transactionHash"], block.hashes[index], "tx {index}");
        }
    }
}

/// A state dump of a replay carries the code of an account an included predecessor wrote and the
/// target only touched: the block's state keeps the code of every account it holds.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_replay_dump_carries_the_code_of_a_predecessors_account() {
    // PUSH1 1 PUSH0 SSTORE STOP: writes slot 0.
    let storer = address!("0x6666666666666666666666666666666666666666");
    let storer_code = vec![0x60, 0x01, 0x5f, 0x55, 0x00];
    // PUSH20 <storer> BALANCE POP STOP: touches the storer's account and loads no code.
    let toucher = address!("0x7777777777777777777777777777777777777777");
    let mut toucher_code = vec![0x73];
    toucher_code.extend_from_slice(storer.as_slice());
    toucher_code.extend_from_slice(&[0x31, 0x50, 0x00]);
    let slots = BTreeMap::from([(B256::ZERO, U256::ZERO)]);
    let block = SyntheticBlock::start(
        &[(storer, storer_code.clone(), slots), (toucher, toucher_code, BTreeMap::new())],
        &[storer, toucher],
    )
    .await;

    let summary = block.replay(1);
    assert_eq!(
        summary["state"][storer.to_string()]["code"],
        alloy_primitives::hex::encode_prefixed(&storer_code),
        "{summary}"
    );
}
