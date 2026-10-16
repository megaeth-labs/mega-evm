//! A transaction replayed alone, over RPC, is the same transaction the block replay runs.
//!
//! `replay <TX_HASH>` forks the parent block's state lazily over RPC and runs the block's
//! earlier transactions first; `replay --block` runs the whole block on the pre-state recorded
//! with it. The two paths share no state source, so their agreeing on every transaction checked
//! here is evidence for both: on the legacy engine (the 1.7.1 CLI against the chain's receipts)
//! and on Satin (the in-tree port against the block driver).
//!
//! The RPC is a mock that answers only from the recorded block: a read the recording does not
//! hold is an error, not a guess. The one exception is the EIP-7997 factory, which Satin's
//! pre-block changes read and no recording holds; it is served as absent, which
//! `block_replay.rs` shows changes no transaction row.
#![cfg(feature = "legacy")]

mod common;

use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use common::blocks::{
    cache_copy_with_absent_factory, code_of, fixtures, read_block, run_evme, FACTORY,
};
use mega_evme::block::{CachedBlock, PreAccount};
use serde_json::{json, Value};
use wiremock::{matchers, Mock, MockServer, Respond, ResponseTemplate};

const BLOCK: u64 = 26_400_110;

/// Answers JSON-RPC from the recorded block alone.
struct Recording {
    block: CachedBlock,
}

impl Recording {
    fn account(&self, address: Address) -> Option<PreAccount> {
        match self.block.prestate.accounts.get(&address) {
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
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .respond_with(Recording { block })
            .mount(&server)
            .await;
        Self(Arc::new(server))
    }
}

/// The block replay's transaction records, on `spec`'s engine, under `limits` when given.
fn block_rows(spec: Option<&str>, limits: Option<&str>) -> Vec<Value> {
    let cache = cache_copy_with_absent_factory();
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
/// middle and the last.
fn checked(rows: &[Value]) -> Vec<usize> {
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
