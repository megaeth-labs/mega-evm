//! `mega-evme replay --block`: whole blocks on either engine, compared with the chain, on the
//! recorded mainnet blocks of `tests/fixtures/blocks` (see `common/blocks.rs`).
#![cfg(feature = "legacy")]

mod common;

use std::path::Path;

use alloy_primitives::{Address, B256, U256};
use common::blocks::{
    block_path, cache_copy, cache_copy_with_absent_factory, code_of, fixtures, read_block,
    run_evme, write_block, BLOCKS, FACTORY,
};
use mega_evm::{system::MEGA_SYSTEM_ADDRESS, ProtocolLimits};
use mega_evme::block::PreAccount;
use serde_json::Value;

fn blocks_of(records: &[Value]) -> Vec<&Value> {
    records.iter().filter(|r| r["kind"] == "block").collect()
}

/// `replay --block N --block-cache DIR`, with `extra` arguments.
fn replay(number: u64, dir: &Path, extra: &[&str]) -> common::blocks::Run {
    let mut args = vec![
        "replay".to_string(),
        "--block".to_string(),
        number.to_string(),
        "--block-cache".to_string(),
        dir.to_str().unwrap().to_string(),
        "--json".to_string(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    run_evme(&args)
}

/// The legacy leg replays each recorded block exactly as the chain executed it: every
/// transaction's status, gas used, cumulative gas used and logs, and the receipts root.
/// Offline, a replay writes nothing to the cache.
#[test]
fn test_the_legacy_leg_replays_recorded_blocks_as_the_chain_did() {
    let cache = cache_copy();
    let before = std::fs::read(block_path(cache.path(), BLOCKS[0])).unwrap();
    for number in BLOCKS {
        let run = replay(number, cache.path(), &["--verify"]);
        assert_eq!(run.code, 0, "block {number}: {}", run.stderr);
        let records = run.records();
        let blocks = blocks_of(&records);
        assert_eq!(blocks.len(), 1);
        let block = blocks[0];
        assert_eq!(block["engine"], "legacy");
        assert_eq!(block["spec"], "Rex6");
        assert_eq!(block["matches_chain"], true, "block {number}: {block}");
        assert_eq!(block["receipts_root"], block["chain_receipts_root"]);
        assert_eq!(block["rpc_reads"], 0);
        let txs: Vec<_> = records.iter().filter(|r| r["kind"] == "tx").collect();
        assert_eq!(txs.len(), block["transactions"].as_u64().unwrap() as usize);
        assert!(txs.iter().all(|tx| tx["differs"] == Value::Array(vec![])), "block {number}");
        assert!(txs.iter().all(|tx| tx.get("satin").is_none()), "a legacy record has no satin");
    }
    // The failed transaction of 26400007 failed in the replay too.
    let records = replay(26_400_007, cache.path(), &[]).records();
    assert!(records.iter().any(|r| r["kind"] == "tx" && r["chain"]["status"] == "failure"));
    assert_eq!(std::fs::read(block_path(cache.path(), BLOCKS[0])).unwrap(), before);
}

/// A Satin replay of a recorded block reads the EIP-7997 factory, which the recording lacks:
/// offline, the block fails and names the account, and the exit code is 1.
#[test]
fn test_a_read_the_recording_lacks_fails_the_block_offline() {
    let cache = cache_copy();
    let run = replay(BLOCKS[0], cache.path(), &["--override.spec", "Satin"]);
    assert_eq!(run.code, 1);
    let records = run.records();
    let error = records[0]["error"].as_str().unwrap();
    assert!(error.contains("no RPC to read it"), "{error}");
    assert!(error.to_lowercase().contains(&format!("{FACTORY:#x}")), "{error}");
}

/// The Satin counterfactual of the recorded blocks: every block runs, nothing is refused, every
/// record carries the legacy columns and adds `satin`, and `--verify` reports the difference
/// from the chain with exit code 2.
#[test]
fn test_the_satin_counterfactual_of_recorded_blocks() {
    let cache = cache_copy_with_absent_factory();
    for number in BLOCKS {
        let run = replay(number, cache.path(), &["--override.spec", "Satin"]);
        assert_eq!(run.code, 0, "{}", run.stderr);
        let records = run.records();
        let block = blocks_of(&records)[0];
        assert_eq!(block["engine"], "satin");
        assert_eq!(block["spec"], "Satin");
        assert_eq!(block["refused"], 0);
        assert_eq!(block["matches_chain"], false);
        assert!(block["satin"]["history_gas"].as_u64().unwrap() > 0);
    }

    let legacy = replay(BLOCKS[0], cache.path(), &[]).records();
    let run = replay(BLOCKS[0], cache.path(), &["--override.spec", "Satin", "--verify"]);
    assert_eq!(run.code, 2, "a counterfactual differs from the chain");
    let satin = run.records();
    assert_eq!(legacy.len(), satin.len());
    for (legacy, satin) in legacy.iter().zip(&satin) {
        let (Value::Object(l), Value::Object(s)) = (legacy, satin) else { panic!() };
        for key in l.keys() {
            assert!(s.contains_key(key), "the Satin record lacks the legacy column {key}");
        }
        assert!(s.contains_key("satin"), "{satin}");
    }
}

/// Whichever state the factory had on chain, the Satin rows of the recorded blocks are the
/// same: no transaction reads the factory, and the pre-block deploy that reads it is no
/// transaction.
#[test]
fn test_the_factory_state_does_not_change_the_satin_rows() {
    let absent = cache_copy_with_absent_factory();
    let present = cache_copy();
    for number in BLOCKS {
        let mut block = read_block(present.path(), number);
        block.prestate.accounts.insert(
            FACTORY,
            PreAccount {
                balance: U256::ZERO,
                nonce: 1,
                code_hash: Some(mega_evm::system::CREATE2_FACTORY_CODE_HASH),
                storage: Default::default(),
            },
        );
        write_block(present.path(), number, &block);
    }
    let hash = mega_evm::system::CREATE2_FACTORY_CODE_HASH.to_string();
    let code_dir = present.path().join("codes").join(&hash[2..4]);
    std::fs::create_dir_all(&code_dir).unwrap();
    std::fs::write(
        code_dir.join(format!("{}.bin", &hash[2..])),
        mega_evm::system::CREATE2_FACTORY_CODE,
    )
    .unwrap();

    for number in BLOCKS {
        let rows = |dir: &Path| {
            let run = replay(number, dir, &["--override.spec", "Satin"]);
            assert_eq!(run.code, 0, "{}", run.stderr);
            run.records()
        };
        assert_eq!(rows(absent.path()), rows(present.path()), "block {number}");
    }
}

/// A block fetched over RPC replays as the chain did, is written to the block cache with what
/// its replay read beyond the trace, and then replays offline from the cache. The mock serves
/// the recorded responses, and the trace is the recorded pre-state as one transaction's
/// sighting.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_block_fetched_over_rpc_is_cached_and_replays_offline() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let number = BLOCKS[2];
    let recorded = read_block(fixtures(), number);
    let mut trace_accounts = serde_json::Map::new();
    for (address, account) in &recorded.prestate.accounts {
        let mut entry = serde_json::json!({ "balance": account.balance, "nonce": account.nonce });
        if let Some(hash) = account.code_hash {
            entry["code"] = Value::String(alloy_primitives::hex::encode_prefixed(code_of(hash)));
        }
        let storage: serde_json::Map<_, _> = account
            .storage
            .iter()
            .map(|(slot, value)| (slot.to_string(), Value::String(B256::from(*value).to_string())))
            .collect();
        entry["storage"] = Value::Object(storage);
        trace_accounts.insert(address.to_string(), entry);
    }
    let first_tx = recorded.block["transactions"][0]["hash"].clone();
    let trace = serde_json::json!([{ "txHash": first_tx, "result": trace_accounts }]);

    let server = MockServer::start().await;
    let respond = |method: &'static str, result: Value| {
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string_contains(format!("\"{method}\"")))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "jsonrpc": "2.0", "id": body["id"], "result": result }),
                )
            })
    };
    respond("eth_chainId", serde_json::json!("0x10e6")).mount(&server).await;
    respond("eth_getBlockByNumber", recorded.block.clone()).mount(&server).await;
    respond("eth_getBlockReceipts", recorded.receipts.clone()).mount(&server).await;
    respond("debug_traceBlockByNumber", trace).mount(&server).await;
    // The parent state of what the trace does not hold: only the factory, which does not exist.
    respond("eth_getBalance", serde_json::json!("0x0")).mount(&server).await;
    respond("eth_getTransactionCount", serde_json::json!("0x0")).mount(&server).await;
    respond("eth_getCode", serde_json::json!("0x")).mount(&server).await;

    let cache = tempfile::tempdir().unwrap();
    let uri = server.uri();

    let run = replay(number, cache.path(), &["--rpc", &uri, "--verify"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(blocks_of(&run.records())[0]["matches_chain"], true);
    assert!(block_path(cache.path(), number).exists(), "the fetched block is cached");

    let run = replay(number, cache.path(), &["--rpc", &uri, "--override.spec", "Satin"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        blocks_of(&run.records())[0]["rpc_reads"].as_u64().unwrap() > 0,
        "the factory came over RPC"
    );

    // Offline now: the cache holds the block and the factory the Satin replay read.
    drop(server);
    let run = replay(number, cache.path(), &["--verify"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(blocks_of(&run.records())[0]["matches_chain"], true);
    let run = replay(number, cache.path(), &["--override.spec", "Satin"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(blocks_of(&run.records())[0]["rpc_reads"], 0);
    assert!(read_block(cache.path(), number).prestate.accounts.contains_key(&FACTORY));
}

/// `--override.limits` replays a Satin block under other protocol limits: the fields it names
/// replace the chain's, and every other stays. At the protocol's defaults it changes nothing; a
/// transaction data-size limit of the body alone stops every transaction but the protocol's own,
/// which no per-transaction limit holds; a block data budget of one byte packs the first
/// transaction and refuses every later one.
#[test]
fn test_an_override_replays_a_satin_block_under_other_limits() {
    let cache = cache_copy_with_absent_factory();
    let number = BLOCKS[0];
    let replay_under = |limits: Option<&str>| -> (Vec<Value>, Value) {
        let mut args = vec!["--override.spec", "Satin"];
        if let Some(limits) = limits {
            args.extend(["--override.limits", limits]);
        }
        let run = replay(number, cache.path(), &args);
        assert_eq!(run.code, 0, "{}", run.stderr);
        let records = run.records();
        let block = blocks_of(&records)[0].clone();
        (records.into_iter().filter(|r| r["kind"] == "tx").collect(), block)
    };

    let (counterfactual, plain) = replay_under(None);
    assert!(plain["satin"].get("limits_override").is_none(), "{}", plain["satin"]);
    let defaults = serde_json::to_string(&ProtocolLimits::DEFAULT).unwrap();
    let (on_the_defaults, overridden) = replay_under(Some(&defaults));
    assert_eq!(on_the_defaults, counterfactual, "it runs on the defaults");
    assert_eq!(
        overridden["satin"]["limits_override"],
        serde_json::to_value(ProtocolLimits::DEFAULT).unwrap(),
        "the block's record says what it ran under"
    );
    let dir = cache.path().to_str().unwrap();
    let block = number.to_string();
    let args = ["replay", "--block", &block, "--block-cache", dir, "--override.spec", "Satin"];
    let human = run_evme(&[&args[..], &["--override.limits", &defaults]].concat());
    assert_eq!(human.code, 0, "{}", human.stderr);
    assert!(human.stdout.contains(", limits overridden]"), "{}", human.stdout);
    assert!(!run_evme(&args).stdout.contains("limits overridden"));
    assert!(counterfactual.iter().all(|tx| tx["satin"]["limit_exceeded"].is_null()));

    let recorded = read_block(cache.path(), number);
    let sent = recorded.block["transactions"].as_array().unwrap();
    let (stopped, stopped_block) =
        replay_under(Some(r#"{"txRuntimeLimits":{"txDataSizeLimit":310}}"#));
    assert_eq!(
        stopped_block["satin"]["limits_override"]["txRuntimeLimits"]["txDataSizeLimit"],
        310
    );
    let (mut protocols, mut others) = (0, 0);
    for (tx, sent) in stopped.iter().zip(sent) {
        let stop = &tx["satin"]["limit_exceeded"];
        let from: Address = sent["from"].as_str().unwrap().parse().unwrap();
        if from == MEGA_SYSTEM_ADDRESS {
            protocols += 1;
            assert!(
                stop.is_null(),
                "the protocol's transaction {} is held to no limit",
                tx["index"]
            );
            assert!(tx["satin"]["data_size"].as_u64().unwrap() > 310);
        } else {
            others += 1;
            assert_eq!(stop["kind"], "data_size", "{tx}");
            assert_eq!(stop["limit"], 310, "{tx}");
            assert_eq!(stop["used"], tx["satin"]["data_size"], "{tx}");
        }
    }
    assert!(protocols > 0 && others > 0, "{protocols} {others}");

    let (packed, block) = replay_under(Some(r#"{"blockTxsDataLimit":1}"#));
    assert_eq!(block["refused"], block["transactions"].as_u64().unwrap() - 1);
    assert_eq!(packed[0], counterfactual[0], "the transaction that reaches the budget is packed");
    for tx in &packed[1..] {
        assert_eq!(tx["status"], "refused", "{tx}");
        let reason = tx["reason"].as_str().unwrap();
        assert!(reason.contains("Block transactions data limit reached"), "{reason}");
    }
}

/// `--override.limits` is Satin's: a block the legacy engine replays is refused with it, and
/// the replay exits with code 1.
#[test]
fn test_an_override_refuses_a_legacy_block() {
    let cache = cache_copy();
    let run = replay(BLOCKS[0], cache.path(), &["--override.limits", "{}"]);
    assert_eq!(run.code, 1, "{}", run.stdout);
    let error = run.records()[0]["error"].as_str().unwrap().to_string();
    assert!(error.contains("--override.limits applies to Satin only"), "{error}");
}

/// A genesis file of mainnet activating Satin at `satin_time`, with the placeholder registry
/// seeds and the protocol's default limits but a transaction data-size limit of the body alone,
/// in `dir`.
fn mainnet_genesis(dir: &tempfile::TempDir, satin_time: u64) -> String {
    let limits = ProtocolLimits::DEFAULT.with_tx_runtime_limits(
        ProtocolLimits::DEFAULT.tx_runtime_limits.with_tx_data_size_limit(310),
    );
    let satin = mega_evm::SatinChainConfig {
        activation_time: satin_time,
        sequencer_registry: mega_evm::system::SequencerRegistryConfig::placeholder(),
        protocol_limits: limits,
    };
    let mut config = serde_json::to_value(satin).unwrap();
    config["chainId"] = mega_evm::MAINNET_CHAIN_ID.into();
    let path = dir.path().join(format!("genesis-{satin_time}.json"));
    std::fs::write(&path, serde_json::json!({ "config": config }).to_string()).unwrap();
    path.to_str().unwrap().to_string()
}

/// A genesis file decides which of its chain's blocks run Satin, and the schedule they run under:
/// a recorded mainnet block, legacy by the engine's table, runs on Satin from a file's `satinTime`
/// of zero, held to the file's limits as the chain's own rather than as an override; a file whose
/// `satinTime` is after the block leaves it on the legacy engine.
#[test]
fn test_a_genesis_file_runs_its_chains_blocks_on_its_schedule() {
    let cache = cache_copy_with_absent_factory();
    let dir = tempfile::tempdir().unwrap();

    let run = replay(BLOCKS[0], cache.path(), &["--genesis", &mainnet_genesis(&dir, 0)]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let records = run.records();
    let block = blocks_of(&records)[0];
    assert_eq!(block["engine"], "satin");
    assert!(block["satin"].get("limits_override").is_none(), "{}", block["satin"]);
    let stopped: Vec<_> = records
        .iter()
        .filter(|r| r["kind"] == "tx" && !r["satin"]["limit_exceeded"].is_null())
        .collect();
    assert!(!stopped.is_empty(), "the file's limit stops the users' transactions");
    for tx in stopped {
        assert_eq!(tx["satin"]["limit_exceeded"]["kind"], "data_size", "{tx}");
        assert_eq!(tx["satin"]["limit_exceeded"]["limit"], 310, "{tx}");
    }

    let run = replay(BLOCKS[0], cache.path(), &["--genesis", &mainnet_genesis(&dir, u64::MAX)]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(blocks_of(&run.records())[0]["engine"], "legacy");
}
