//! `mega-evme replay --block`: whole blocks on either engine, compared with the chain, on the
//! recorded mainnet blocks of `tests/fixtures/blocks` (see `common/blocks.rs`).
#![cfg(feature = "legacy")]

mod common;

use std::path::Path;

use alloy_primitives::{Address, B256, U256};
use common::blocks::{
    block_path, cache_copy, cache_copy_for_satin, code_of, fixtures, is_deposit, overhead_slot,
    read_block, refused_for_block_gas, run_evme, satin_genesis, set_overhead, timestamp_of,
    write_block, BLOCKS, FACTORY, L1_BLOCK,
};
use mega_evm::{system::MEGA_SYSTEM_ADDRESS, MegaSpecId, ProtocolLimits};
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

/// A Satin replay of a recorded block reads the EIP-7997 factory and the L1 fee overhead, which
/// the recording lacks: offline, the block fails and names the first of them it met, and the
/// exit code is 1.
#[test]
fn test_a_read_the_recording_lacks_fails_the_block_offline() {
    let refusal = |dir: &Path| {
        let run = replay(BLOCKS[0], dir, &["--override.spec", "Satin"]);
        assert_eq!(run.code, 1);
        let error = run.records()[0]["error"].as_str().unwrap().to_lowercase();
        assert!(error.contains("no rpc to read it"), "{error}");
        error
    };

    let cache = cache_copy();
    let error = refusal(cache.path());
    assert!(error.contains(&format!("account {FACTORY:#x}")), "{error}");

    // With the factory in the pre-state, the pre-block read of the L1 block info is the miss:
    // it reads the overhead beside the fee scalars the recording holds set.
    let mut block = read_block(cache.path(), BLOCKS[0]);
    block.prestate.accounts.insert(FACTORY, PreAccount::default());
    write_block(cache.path(), BLOCKS[0], &block);
    let error = refusal(cache.path());
    assert!(error.contains(&format!("storage {L1_BLOCK:#x}[{}]", overhead_slot())), "{error}");
}

/// The Satin counterfactual of the recorded blocks: every block runs, a transaction is refused
/// only for the block gas it declared ([`refused_for_block_gas`]), every record carries the
/// legacy columns, every record of what ran adds `satin`, and `--verify` reports the difference
/// from the chain with exit code 2.
#[test]
fn test_the_satin_counterfactual_of_recorded_blocks() {
    let cache = cache_copy_for_satin();
    for number in BLOCKS {
        let run = replay(number, cache.path(), &["--override.spec", "Satin"]);
        assert_eq!(run.code, 0, "{}", run.stderr);
        let records = run.records();
        let block = blocks_of(&records)[0];
        assert_eq!(block["engine"], "satin");
        assert_eq!(block["spec"], "Satin");
        assert_eq!(block["refused"], refused_for_block_gas(&records));
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
        // A refused transaction ran nothing and has no ledgers.
        if satin["status"] != "refused" {
            assert!(s.contains_key("satin"), "{satin}");
        }
    }
}

/// Whichever state the factory had on chain, the Satin rows of the recorded blocks are the
/// same: no transaction reads the factory, and the pre-block deploy that reads it is no
/// transaction.
#[test]
fn test_the_factory_state_does_not_change_the_satin_rows() {
    let absent = cache_copy_for_satin();
    let present = cache_copy_for_satin();
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

/// Whatever the L1 fee overhead holds, the Satin rows of the recorded blocks are the same: the
/// blocks' fee scalars are set, so no transaction is priced with it, and the pre-block read that
/// reads it is no transaction.
#[test]
fn test_the_overhead_does_not_change_the_satin_rows() {
    let zero = cache_copy_for_satin();
    let set = cache_copy_for_satin();
    for number in BLOCKS {
        let mut block = read_block(set.path(), number);
        set_overhead(&mut block, U256::from(188));
        write_block(set.path(), number, &block);
    }
    for number in BLOCKS {
        let rows = |dir: &Path| {
            let run = replay(number, dir, &["--override.spec", "Satin"]);
            assert_eq!(run.code, 0, "{}", run.stderr);
            run.records()
        };
        assert_eq!(rows(zero.path()), rows(set.path()), "block {number}");
    }
}

/// [S18] A block fetched over RPC replays as the chain did, is written to the block cache with
/// what its replay read beyond the trace, and then replays offline from the cache. The mock
/// serves the recorded responses, and the trace is the recorded pre-state as one transaction's
/// sighting. The Satin replay reads two accounts' worth of state the recording lacks, the
/// EIP-7997 factory its pre-block deploy reads and the L1 fee overhead its pre-block read of the
/// L1 block info reads, and the cache keeps both.
///
/// Expected values: independent (the chain's own receipts, and the two pre-block reads).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_block_fetched_over_rpc_is_cached_and_replays_offline() {
    let number = BLOCKS[2];
    let server = recorded_chain(number, Parent::Block).await;
    let cache = tempfile::tempdir().unwrap();
    let uri = server.uri();

    let run = replay(number, cache.path(), &["--rpc", &uri, "--verify"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(blocks_of(&run.records())[0]["matches_chain"], true);
    assert!(block_path(cache.path(), number).exists(), "the fetched block is cached");

    let run = replay(number, cache.path(), &["--rpc", &uri, "--override.spec", "Satin"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        blocks_of(&run.records())[0]["rpc_reads"],
        2,
        "the factory and the L1 fee overhead came over RPC"
    );

    // Offline now: the cache holds the block, and the factory and the overhead the Satin replay
    // read.
    drop(server);
    let run = replay(number, cache.path(), &["--verify"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(blocks_of(&run.records())[0]["matches_chain"], true);
    let run = replay(number, cache.path(), &["--override.spec", "Satin"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(blocks_of(&run.records())[0]["rpc_reads"], 0);
    let cached = read_block(cache.path(), number).prestate;
    assert!(cached.accounts.contains_key(&FACTORY));
    assert_eq!(cached.accounts[&L1_BLOCK].storage.get(&overhead_slot()), Some(&U256::ZERO));
}

/// `--override.limits` replays a Satin block under other protocol limits: the fields it names
/// replace the spec's, and every other stays. At the spec's own values it changes nothing; under a
/// transaction data-size limit of what the block's L1 attributes deposit keeps, every transaction
/// that keeps more without the override is held to it — stopped where it crosses it, or refused at
/// validation when its body alone does — but the protocol's own, which no per-transaction limit
/// holds, and which run past it; a block data budget of one byte packs the first transaction and
/// refuses every later one.
#[test]
fn test_an_override_replays_a_satin_block_under_other_limits() {
    let cache = cache_copy_for_satin();
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
    let satin = ProtocolLimits::for_spec(MegaSpecId::SATIN);
    let defaults = serde_json::to_string(&satin).unwrap();
    let (on_the_defaults, overridden) = replay_under(Some(&defaults));
    assert_eq!(on_the_defaults, counterfactual, "it runs on the spec's limits");
    assert_eq!(
        overridden["satin"]["limits_override"],
        serde_json::to_value(satin).unwrap(),
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
    let limit = counterfactual[0]["satin"]["data_size"].as_u64().unwrap();
    let (stopped, stopped_block) =
        replay_under(Some(&format!(r#"{{"txRuntimeLimits":{{"txDataSizeLimit":{limit}}}}}"#)));
    assert_eq!(
        stopped_block["satin"]["limits_override"]["txRuntimeLimits"]["txDataSizeLimit"],
        limit
    );
    let (mut protocols, mut others) = (0, 0);
    for ((tx, sent), unlimited) in stopped.iter().zip(sent).zip(&counterfactual) {
        let from: Address = sent["from"].as_str().unwrap().parse().unwrap();
        let kept = unlimited["satin"]["data_size"].as_u64();
        if from == MEGA_SYSTEM_ADDRESS {
            // The protocol's transactions carry gas limits close to the block's, so the gas the
            // block has left may refuse one, as it may without the override; no limit does.
            if tx["status"] == "refused" {
                let reason = tx["reason"].as_str().unwrap();
                assert!(reason.contains("blocks available gas"), "{reason}");
                continue;
            }
            assert!(
                tx["satin"]["limit_exceeded"].is_null(),
                "the protocol's transaction {} is held to no limit",
                tx["index"]
            );
            if tx["satin"]["data_size"].as_u64().unwrap() > limit {
                protocols += 1;
            }
        } else if kept.is_some_and(|kept| kept > limit) {
            others += 1;
            if tx["status"] == "refused" {
                let reason = tx["reason"].as_str().unwrap();
                assert!(reason.contains(mega_evm::TX_BODY_OVER_DATA_SIZE_LIMIT), "{reason}");
                continue;
            }
            let stop = &tx["satin"]["limit_exceeded"];
            assert_eq!(stop["kind"], "data_size", "{tx}");
            assert_eq!(stop["limit"], limit, "{tx}");
            assert!(stop["used"].as_u64().unwrap() > limit, "{tx}");
        }
    }
    assert!(protocols > 0 && others > 0, "{protocols} {others}");

    // The budget refuses every transaction after the one that reaches it but the protocol's own:
    // a Mega System Transaction is never refused by it.
    let (packed, block) = replay_under(Some(r#"{"blockTxsDataLimit":1}"#));
    let is_protocols = |index: usize| {
        let from: Address = sent[index]["from"].as_str().unwrap().parse().unwrap();
        from == MEGA_SYSTEM_ADDRESS
    };
    let protocol_txs = (1..packed.len()).filter(|&index| is_protocols(index)).count();
    assert!(protocol_txs > 0, "the block holds Mega System Transactions");
    assert_eq!(block["refused"], block["transactions"].as_u64().unwrap() - 1 - protocol_txs as u64,);
    assert_eq!(packed[0], counterfactual[0], "the transaction that reaches the budget is packed");
    for (index, tx) in packed.iter().enumerate().skip(1) {
        if is_protocols(index) {
            assert_ne!(tx["status"], "refused", "the budget does not refuse the protocol's: {tx}");
            continue;
        }
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
/// seeds, in `dir`.
fn mainnet_genesis(dir: &tempfile::TempDir, satin_time: u64) -> String {
    genesis_of(dir, mega_evm::MAINNET_CHAIN_ID, satin_time)
}

/// [`mainnet_genesis`] for chain `chain_id`.
fn genesis_of(dir: &tempfile::TempDir, chain_id: u64, satin_time: u64) -> String {
    satin_genesis(dir.path(), chain_id, satin_time)
}

/// A genesis file decides which of its chain's blocks run Satin, and the schedule they run under:
/// a recorded mainnet block, legacy by the engine's table, runs on Satin from a file's `satinTime`
/// of zero, held to Satin's limits, which no file names, as the replay without the file is. A block
/// before the file's `satinTime` would run on the legacy engine, which runs a chain on its own
/// table and knows nothing of the file: it is refused, and so is one a forced legacy spec runs,
/// one a forced Satin runs before the file's `satinTime`, and a block of another chain.
#[test]
fn test_a_genesis_file_runs_its_chains_blocks_on_its_schedule() {
    let cache = cache_copy_for_satin();
    let dir = tempfile::tempdir().unwrap();

    let run = replay(BLOCKS[0], cache.path(), &["--genesis", &mainnet_genesis(&dir, 0)]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let records = run.records();
    let block = blocks_of(&records)[0];
    assert_eq!(block["engine"], "satin");
    assert!(block["satin"].get("limits_override").is_none(), "{}", block["satin"]);
    let txs: Vec<_> = records.iter().filter(|r| r["kind"] == "tx").collect();
    assert!(!txs.is_empty());
    assert!(
        txs.iter().all(|tx| tx["satin"]["limit_exceeded"].is_null()),
        "no transaction of the block comes near Satin's limits"
    );

    let later = mainnet_genesis(&dir, u64::MAX);
    let forced = mainnet_genesis(&dir, 0);
    for args in [&["--genesis", &later][..], &["--genesis", &forced, "--override.spec", "Rex6"]] {
        let run = replay(BLOCKS[0], cache.path(), args);
        assert_eq!(run.code, 1, "{args:?}: {}", run.stdout);
        let error = run.records()[0]["error"].as_str().unwrap().to_string();
        assert!(error.contains("--genesis applies to Satin blocks only"), "{args:?}: {error}");
    }

    let run = replay(BLOCKS[0], cache.path(), &["--genesis", &later, "--override.spec", "Satin"]);
    assert_eq!(run.code, 1, "{}", run.stdout);
    let error = run.records()[0]["error"].as_str().unwrap().to_string();
    assert!(
        error.contains(&format!("--genesis activates Satin at timestamp {}", u64::MAX)),
        "{error}"
    );

    // A block of another chain is refused for its chain, before the engine the table gives it.
    let run = replay(BLOCKS[0], cache.path(), &["--genesis", &genesis_of(&dir, 6342, 0)]);
    assert_eq!(run.code, 1, "{}", run.stdout);
    let error = run.records()[0]["error"].as_str().unwrap().to_string();
    let expected = format!(
        "--genesis configures chain 6342, and the run is on chain {}",
        mega_evm::MAINNET_CHAIN_ID
    );
    assert!(error.contains(&expected), "{error}");
}

/// The reason the Satin engine refuses a transaction of a block that admits only deposits.
const ACTIVATION_REFUSAL: &str = "non-deposit transaction in fork activation block";

/// Checks the transaction records of a replay of the recorded `block` as its activation block:
/// every deposit runs, and every other transaction — the protocol's legacy transactions
/// included — is refused for the rule.
fn assert_refused_as_activation_block(block: &mega_evme::block::CachedBlock, records: &[Value]) {
    let txs: Vec<_> = records.iter().filter(|r| r["kind"] == "tx").collect();
    let deposits = (0..txs.len()).filter(|&i| is_deposit(block, i)).count();
    assert!(
        deposits > 0 && deposits < txs.len(),
        "the block holds deposits and other transactions"
    );
    for (index, tx) in txs.iter().enumerate() {
        if is_deposit(block, index) {
            assert_ne!(tx["status"], "refused", "deposit {index} runs: {tx}");
        } else {
            assert_eq!(tx["status"], "refused", "transaction {index}: {tx}");
            let reason = tx["reason"].as_str().unwrap();
            assert!(reason.contains(ACTIVATION_REFUSAL), "transaction {index}: {reason}");
        }
    }
    assert_eq!(blocks_of(records)[0]["refused"], txs.len() - deposits);
}

/// [S1.8] [S21.12] [S1] A replayed block is told whether it admits only deposits as a node is:
/// from its schedule and its parent's timestamp, which the block cache keeps beside the block.
/// Under a genesis file that activates Satin at the recorded block's own timestamp, the block
/// whose parent came a second earlier is Satin's activation block: its deposits run and every
/// other transaction is refused. With its parent at the block's own timestamp Satin is active at
/// both, and nothing is refused for the activation, only for the block gas a transaction declared
/// ([`refused_for_block_gas`]). Without the parent's timestamp and without `--rpc` the tool
/// cannot tell, and the block fails saying why.
///
/// Expected values: independent.
#[test]
fn test_a_replayed_activation_block_refuses_its_user_transactions() {
    let cache = cache_copy_for_satin();
    let dir = tempfile::tempdir().unwrap();
    let number = BLOCKS[0];
    let recorded = read_block(cache.path(), number);
    let timestamp = timestamp_of(&recorded);
    let genesis = satin_genesis(dir.path(), mega_evm::MAINNET_CHAIN_ID, timestamp);
    let with_parent_at = |parent_timestamp: Option<u64>| {
        let mut block = read_block(cache.path(), number);
        block.parent_timestamp = parent_timestamp;
        write_block(cache.path(), number, &block);
        replay(number, cache.path(), &["--genesis", &genesis])
    };

    let run = with_parent_at(Some(timestamp - 1));
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_refused_as_activation_block(&recorded, &run.records());

    let run = with_parent_at(Some(timestamp));
    assert_eq!(run.code, 0, "{}", run.stderr);
    let records = run.records();
    assert_eq!(
        blocks_of(&records)[0]["refused"],
        refused_for_block_gas(&records),
        "the block after admits every transaction its gas fits"
    );

    let run = with_parent_at(None);
    assert_eq!(run.code, 1, "{}", run.stdout);
    let error = run.records()[0]["error"].as_str().unwrap().to_string();
    assert!(error.contains("depends on its parent's timestamp"), "{error}");
    assert!(error.contains("replay it once with '--rpc'"), "it says how to recover: {error}");
}

/// What a [`recorded_chain`] answers for the parent block's header.
enum Parent {
    /// The recorded block's own header, at the block's own timestamp.
    Block,
    /// A header at this timestamp.
    At(u64),
    /// No block: the endpoint does not have the parent.
    Missing,
}

/// A mock of the chain serving the recorded `number` block, its receipts, its pre-state as a
/// trace, and the reads Satin's pre-block changes make beyond it; the parent block's header is
/// what `parent` says.
async fn recorded_chain(number: u64, parent: Parent) -> wiremock::MockServer {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

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
    let parent = match parent {
        Parent::Block => None,
        Parent::At(timestamp) => {
            let mut header = recorded.block.clone();
            header["number"] = format!("0x{:x}", number - 1).into();
            header["timestamp"] = format!("0x{timestamp:x}").into();
            Some(header)
        }
        Parent::Missing => Some(Value::Null),
    };
    if let Some(parent) = parent {
        Mock::given(matchers::method("POST"))
            .and(matchers::body_string_contains("\"eth_getBlockByNumber\""))
            .and(matchers::body_string_contains(format!("\"0x{:x}\"", number - 1)))
            .respond_with(move |req: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "jsonrpc": "2.0", "id": body["id"], "result": parent }),
                )
            })
            .with_priority(1)
            .mount(&server)
            .await;
    }
    respond("eth_chainId", serde_json::json!("0x10e6")).mount(&server).await;
    respond("eth_getBlockByNumber", recorded.block.clone()).mount(&server).await;
    respond("eth_getBlockReceipts", recorded.receipts.clone()).mount(&server).await;
    respond("debug_traceBlockByNumber", trace).mount(&server).await;
    // The parent state of what the trace does not hold: the factory, which does not exist, and
    // the L1 fee overhead, which is zero.
    respond("eth_getBalance", serde_json::json!("0x0")).mount(&server).await;
    respond("eth_getTransactionCount", serde_json::json!("0x0")).mount(&server).await;
    respond("eth_getCode", serde_json::json!("0x")).mount(&server).await;
    respond("eth_getStorageAt", serde_json::json!(B256::ZERO)).mount(&server).await;
    server
}

/// [S1.8] [S1] A block fetched over RPC is fetched with its parent's timestamp, and the cache
/// keeps it: under a genesis file that activates Satin at the block's timestamp, with the parent
/// a second earlier, the replay refuses the block's user transactions online, writes the
/// parent's timestamp to the cache, and refuses them again offline. A cached entry that lacks the
/// timestamp gets it over RPC and is written back with it.
///
/// Expected values: independent.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_block_fetched_over_rpc_keeps_its_parents_timestamp() {
    let number = BLOCKS[2];
    let recorded = read_block(fixtures(), number);
    let timestamp = timestamp_of(&recorded);
    let server = recorded_chain(number, Parent::At(timestamp - 1)).await;
    let uri = server.uri();
    let cache = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let genesis = satin_genesis(dir.path(), mega_evm::MAINNET_CHAIN_ID, timestamp);

    let run = replay(number, cache.path(), &["--rpc", &uri, "--genesis", &genesis]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_refused_as_activation_block(&recorded, &run.records());
    assert_eq!(read_block(cache.path(), number).parent_timestamp, Some(timestamp - 1));

    // An entry written without it gets it over RPC, and keeps it.
    let mut entry = read_block(cache.path(), number);
    entry.parent_timestamp = None;
    write_block(cache.path(), number, &entry);
    let run = replay(number, cache.path(), &["--rpc", &uri, "--genesis", &genesis]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_refused_as_activation_block(&recorded, &run.records());
    assert_eq!(read_block(cache.path(), number).parent_timestamp, Some(timestamp - 1));

    drop(server);
    let run = replay(number, cache.path(), &["--genesis", &genesis]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_refused_as_activation_block(&recorded, &run.records());
}

/// How many times `server` was asked for the header of block `number`'s parent.
async fn parent_requests(server: &wiremock::MockServer, number: u64) -> usize {
    let tag = format!("\"0x{:x}\"", number - 1);
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains("\"eth_getBlockByNumber\"") && body.contains(&tag)
        })
        .count()
}

/// [S1] A cached block asks for its parent's header only when the activation-block rule needs
/// the parent's timestamp. With the recorded entries, written before the cache kept it, and an
/// endpoint that does not have the parent, the legacy replay matches the chain and the Satin
/// counterfactual, whose schedule activates every fork at genesis, runs, refusing a transaction
/// only for the block gas it declared: neither asks for the parent. Under a genesis file that
/// activates Satin at the block's timestamp the answer depends on the parent, which the endpoint
/// does not have, and the block fails naming it.
///
/// Expected values: independent.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_cached_block_asks_for_its_parent_only_when_the_rule_needs_it() {
    let number = BLOCKS[2];
    let server = recorded_chain(number, Parent::Missing).await;
    let uri = server.uri();
    let cache = cache_copy();
    assert_eq!(read_block(cache.path(), number).parent_timestamp, None, "an entry without it");

    let run = replay(number, cache.path(), &["--rpc", &uri, "--verify"]);
    assert_eq!(run.code, 0, "{}{}", run.stderr, run.stdout);
    assert_eq!(blocks_of(&run.records())[0]["matches_chain"], true, "the legacy replay");
    let run = replay(number, cache.path(), &["--rpc", &uri, "--override.spec", "Satin"]);
    assert_eq!(run.code, 0, "{}{}", run.stderr, run.stdout);
    let records = run.records();
    assert_eq!(
        blocks_of(&records)[0]["refused"],
        refused_for_block_gas(&records),
        "the Satin counterfactual"
    );
    assert_eq!(parent_requests(&server, number).await, 0, "neither asked for the parent");

    let dir = tempfile::tempdir().unwrap();
    let timestamp = timestamp_of(&read_block(cache.path(), number));
    let genesis = satin_genesis(dir.path(), mega_evm::MAINNET_CHAIN_ID, timestamp);
    let run = replay(number, cache.path(), &["--rpc", &uri, "--genesis", &genesis]);
    assert_eq!(run.code, 1, "{}", run.stdout);
    let error = run.records()[0]["error"].as_str().unwrap().to_string();
    assert!(error.contains(&format!("Block not found: {}", number - 1)), "{error}");
    assert_eq!(parent_requests(&server, number).await, 1, "the rule asked for it once");
}
