//! `mega-evme replay --block`: whole blocks on either engine, compared with the chain.
//!
//! The fixtures under `tests/fixtures/blocks` are three `MegaETH` mainnet blocks (Rex6) in the
//! block cache layout, recorded from the chain: each block with its transactions, the chain's
//! receipts, and the state its parent left for everything the block read. 26400001 is the most
//! varied block of its range (39 transactions to 19 distinct call targets), 26400007 holds a
//! transaction that failed on chain, 26400110 a plain value transfer.
#![cfg(feature = "legacy")]

use std::{path::Path, process::Command};

use alloy_primitives::{address, Address, B256, U256};
use mega_evme::block::{CachedBlock, PreAccount};
use serde_json::Value;

/// The recorded blocks.
const BLOCKS: [u64; 3] = [26_400_001, 26_400_007, 26_400_110];

/// The EIP-7997 factory, which Satin's pre-block changes deploy and the legacy engine never
/// reads: the one account a Satin replay reads that the recording does not hold.
const FACTORY: Address = address!("0x4e59b44847b379578588920cA78FbF26c0B4956C");

fn fixtures() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/blocks"))
}

/// A copy of the recorded block cache, so no test can write to the fixtures.
fn cache_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(fixtures(), dir.path());
    dir
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn block_path(dir: &Path, number: u64) -> std::path::PathBuf {
    dir.join("blocks").join((number / 10_000).to_string()).join(format!("{number}.json.zst"))
}

fn read_block(dir: &Path, number: u64) -> CachedBlock {
    let compressed = std::fs::read(block_path(dir, number)).unwrap();
    serde_json::from_slice(&zstd::decode_all(compressed.as_slice()).unwrap()).unwrap()
}

fn write_block(dir: &Path, number: u64, block: &CachedBlock) {
    let json = serde_json::to_vec(block).unwrap();
    std::fs::write(block_path(dir, number), zstd::encode_all(json.as_slice(), 3).unwrap()).unwrap();
}

/// Adds the factory to the recorded pre-state of every block, absent.
fn with_absent_factory(dir: &Path) {
    for number in BLOCKS {
        let mut block = read_block(dir, number);
        block.prestate.accounts.insert(FACTORY, PreAccount::default());
        write_block(dir, number, &block);
    }
}

/// Runs `mega-evme` with `args`, returning its exit code and its JSON lines.
fn run(args: &[&str]) -> (i32, Vec<Value>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme")).args(args).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let records =
        stdout.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).collect();
    (output.status.code().unwrap(), records, String::from_utf8(output.stderr).unwrap())
}

fn blocks_of(records: &[Value]) -> Vec<&Value> {
    records.iter().filter(|r| r["kind"] == "block").collect()
}

/// The legacy leg replays each recorded block exactly as the chain executed it: every
/// transaction's status, gas used, cumulative gas used and logs, and the receipts root.
/// Offline, a replay writes nothing to the cache.
#[test]
fn test_the_legacy_leg_replays_recorded_blocks_as_the_chain_did() {
    let cache = cache_copy();
    let dir = cache.path().to_str().unwrap();
    let before = std::fs::read(block_path(cache.path(), BLOCKS[0])).unwrap();
    for number in BLOCKS {
        let (code, records, stderr) = run(&[
            "replay",
            "--block",
            &number.to_string(),
            "--block-cache",
            dir,
            "--verify",
            "--json",
        ]);
        assert_eq!(code, 0, "block {number}: {stderr}");
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
    let (_, records, _) = run(&["replay", "--block", "26400007", "--block-cache", dir, "--json"]);
    assert!(records.iter().any(|r| r["kind"] == "tx" && r["chain"]["status"] == "failure"));
    assert_eq!(std::fs::read(block_path(cache.path(), BLOCKS[0])).unwrap(), before);
}

/// A Satin replay of a recorded block reads the EIP-7997 factory, which the recording lacks:
/// offline, the block fails and names the account, and the exit code is 1.
#[test]
fn test_a_read_the_recording_lacks_fails_the_block_offline() {
    let cache = cache_copy();
    let (code, records, _) = run(&[
        "replay",
        "--block",
        &BLOCKS[0].to_string(),
        "--block-cache",
        cache.path().to_str().unwrap(),
        "--override.spec",
        "Satin",
        "--json",
    ]);
    assert_eq!(code, 1);
    let error = records[0]["error"].as_str().unwrap();
    assert!(error.contains("no RPC to read it"), "{error}");
    assert!(error.to_lowercase().contains(&format!("{FACTORY:#x}")), "{error}");
}

/// The Satin counterfactual of the recorded blocks: every block runs, nothing is refused, every
/// record carries the legacy columns and adds `satin`, and `--verify` reports the difference
/// from the chain with exit code 2.
#[test]
fn test_the_satin_counterfactual_of_recorded_blocks() {
    let cache = cache_copy();
    with_absent_factory(cache.path());
    let dir = cache.path().to_str().unwrap();
    for number in BLOCKS {
        let (code, records, stderr) = run(&[
            "replay",
            "--block",
            &number.to_string(),
            "--block-cache",
            dir,
            "--override.spec",
            "Satin",
            "--json",
        ]);
        assert_eq!(code, 0, "{stderr}");
        let block = blocks_of(&records)[0];
        assert_eq!(block["engine"], "satin");
        assert_eq!(block["spec"], "Satin");
        assert_eq!(block["refused"], 0);
        assert_eq!(block["matches_chain"], false);
        assert!(block["satin"]["history_gas"].as_u64().unwrap() > 0);
    }

    let (_, legacy, _) =
        run(&["replay", "--block", &BLOCKS[0].to_string(), "--block-cache", dir, "--json"]);
    let (code, satin, _) = run(&[
        "replay",
        "--block",
        &BLOCKS[0].to_string(),
        "--block-cache",
        dir,
        "--override.spec",
        "Satin",
        "--verify",
        "--json",
    ]);
    assert_eq!(code, 2, "a counterfactual differs from the chain");
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
    let absent = cache_copy();
    with_absent_factory(absent.path());
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
        let replay = |dir: &Path| {
            let (code, records, stderr) = run(&[
                "replay",
                "--block",
                &number.to_string(),
                "--block-cache",
                dir.to_str().unwrap(),
                "--override.spec",
                "Satin",
                "--json",
            ]);
            assert_eq!(code, 0, "{stderr}");
            records
        };
        assert_eq!(replay(absent.path()), replay(present.path()), "block {number}");
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
            let hex = hash.to_string();
            let code = std::fs::read(
                fixtures().join("codes").join(&hex[2..4]).join(format!("{}.bin", &hex[2..])),
            )
            .unwrap();
            entry["code"] = Value::String(alloy_primitives::hex::encode_prefixed(code));
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
    let dir = cache.path().to_str().unwrap().to_string();
    let uri = server.uri();
    let block = number.to_string();
    let run_blocking = move |args: Vec<String>| {
        std::thread::spawn(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run(&args)
        })
        .join()
        .unwrap()
    };
    let args = |extra: &[&str]| -> Vec<String> {
        ["replay", "--block", &block, "--block-cache", &dir, "--json"]
            .iter()
            .chain(extra)
            .map(|s| s.to_string())
            .collect()
    };

    let (code, records, stderr) = run_blocking(args(&["--rpc", &uri, "--verify"]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(blocks_of(&records)[0]["matches_chain"], true);
    assert!(block_path(cache.path(), number).exists(), "the fetched block is cached");

    let (code, records, stderr) = run_blocking(args(&["--rpc", &uri, "--override.spec", "Satin"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(blocks_of(&records)[0]["rpc_reads"].as_u64().unwrap() > 0, "the factory came over RPC");

    // Offline now: the cache holds the block and the factory the Satin replay read.
    drop(server);
    let (code, records, stderr) = run_blocking(args(&["--verify"]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(blocks_of(&records)[0]["matches_chain"], true);
    let (code, records, stderr) = run_blocking(args(&["--override.spec", "Satin"]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(blocks_of(&records)[0]["rpc_reads"], 0);
    assert!(read_block(cache.path(), number).prestate.accounts.contains_key(&FACTORY));
}
