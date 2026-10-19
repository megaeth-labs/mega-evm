//! Integration tests for `create_initial_state` wiring across the fork and
//! non-fork branches.
//!
//! Synthetic tests use `--fork.block N` to skip real RPC traffic during
//! construction (`EvmeState::new_forked` only contacts the network when
//! `fork_block` is `None`). Tests gated by `#[ignore]` exercise a live
//! `MegaETH` mainnet endpoint and are skipped by default.

use std::str::FromStr;

use alloy_primitives::{Address, B256, U256};
use clap::Parser;
use mega_evm::revm::DatabaseRef;
use mega_evme::common::{PreStateArgs, RpcArgs};
use tempfile::tempdir;

mod common;
use common::{rpc_cache_path, test_rpc_args, test_rpc_args_cached, MockRpcServer};

// ─── Synthetic branch wiring tests (no network required) ─────────────────────

/// Non-fork must produce a no-op cache store regardless of the flags
/// `RpcArgs` was given — non-fork doesn't touch RPC, so there is nothing to
/// persist. The cache file must NOT be created even though it was configured.
#[tokio::test]
async fn test_create_initial_state_non_fork_store_is_noop() {
    let dir = tempdir().expect("tempdir");

    let prestate_args = PreStateArgs::parse_from(["mega-evme"]);
    // 4326 = MegaETH mainnet.
    let rpc_args = test_rpc_args_cached("http://localhost:8545", dir.path(), None);
    let sender = Address::ZERO;

    let (_state, cache_store) =
        prestate_args.create_initial_state(&sender, &rpc_args).await.expect("create_initial_state");

    assert!(cache_store.is_noop(), "non-fork must wire a no-op cache store");
    cache_store.persist().expect("persist");
    let left: Vec<_> = std::fs::read_dir(dir.path()).expect("read_dir").collect();
    assert!(left.is_empty(), "non-fork persist must not produce any cache file");
}

/// Fork mode with `--fork.block N` constructs the state without hitting the
/// network for storage queries, but still wires a real store that writes the
/// configured file on `persist()`. A mock server supplies `eth_chainId` so
/// the chain-id resolution succeeds without a live node.
///
/// Uses the multi-thread runtime because `WrapDatabaseAsync::new()` requires a
/// blocking-capable tokio handle even though no RPC call is actually issued.
#[tokio::test(flavor = "multi_thread")]
async fn test_create_initial_state_fork_store_persists_cache() {
    let dir = tempdir().expect("tempdir");

    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;

    let prestate_args =
        PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", "1000000"]);
    // 4326 = MegaETH mainnet.
    let rpc_args = test_rpc_args_cached(&server.uri(), dir.path(), None);
    let sender = Address::ZERO;

    let (_state, cache_store) =
        prestate_args.create_initial_state(&sender, &rpc_args).await.expect("create_initial_state");

    assert!(!cache_store.is_noop(), "fork + cache_dir must wire a real store");
    cache_store.persist().expect("persist");
    assert!(
        rpc_cache_path(dir.path(), 4326, &server.uri()).exists(),
        "fork persist must write the cache file",
    );
}

/// End-to-end cache reload across two fresh session builds, with cache hit
/// proven by observing minimal outbound requests in phase 2.
///
/// Phase 1 mock returns a real `eth_getStorageAt` response, the value is
/// persisted via `persist()`. Phase 2 uses a *separate* mock against the
/// same cache directory and asserts it received only the `eth_chainId`
/// request — the storage query must be served entirely from the loaded
/// file.
///
/// This is the deterministic, default-CI counterpart of the `#[ignore]`d
/// real-RPC cache hit test below.
#[tokio::test(flavor = "multi_thread")]
async fn test_storage_cache_hit_round_trip_via_mock() {
    let dir = tempdir().expect("tempdir");
    let block_number: u64 = 1_234_567;
    let block_number_str = block_number.to_string();
    // 4326 = MegaETH mainnet.
    let chain_id: u64 = 4326;

    // MegaETH Oracle system contract — predeployed from genesis. The
    // concrete address doesn't matter here (the mock answers every request
    // the same way), but using a real MegaETH address keeps the test
    // internally consistent with the non-synthetic #[ignore]d tests below.
    let oracle =
        Address::from_str("0x6342000000000000000000000000000000000001").expect("address parse");
    let slot = U256::ZERO;
    // 32-byte hex, the JSON-RPC shape of an `eth_getStorageAt` result.
    let storage_hex = "0x000000000000000000000000000000000000000000000000000000000000002a";
    let expected_value = U256::from(0x2a);

    // One endpoint for both phases: the cache file is named from the URL, so a second
    // mock would be a different file.
    let server = MockRpcServer::start().await;
    let rpc_url = server.uri();
    server.respond_eth_chain_id(chain_id, 1).await;
    server.respond_jsonrpc_result(storage_hex, 2).await;

    // ── Phase 1: mock returns the storage value, populate cache, persist. ──
    let phase1_value = {
        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = test_rpc_args_cached(&rpc_url, dir.path(), None);
        let sender = Address::ZERO;

        let (state, cache_store) = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 1");
        let value = state.storage_ref(oracle, slot).expect("storage_ref phase 1");
        cache_store.persist().expect("persist");

        assert!(
            server.received_request_count().await > 0,
            "phase 1 must reach the mock to populate the cache",
        );

        value
    };

    assert_eq!(phase1_value, expected_value, "phase 1 must parse the mock's hex result");
    assert!(
        rpc_cache_path(dir.path(), chain_id, &rpc_url).exists(),
        "phase 1 persist must produce the cache file",
    );

    // ── Phase 2: only eth_chainId left mounted, same endpoint and directory. ──
    let phase2_value = {
        // Any request other than `eth_chainId` is answered by wiremock's default 404,
        // which `storage_ref` would surface as an error.
        server.reset().await;
        server.respond_eth_chain_id(chain_id, 1).await;

        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = test_rpc_args_cached(&rpc_url, dir.path(), None);
        let sender = Address::ZERO;

        let (state, _cache_store) = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 2");
        let value = state.storage_ref(oracle, slot).expect("storage_ref phase 2 — must hit cache");

        assert_eq!(
            server.received_request_count().await,
            1,
            "cache hit: only the eth_chainId request must reach the mock",
        );

        value
    };

    assert_eq!(phase1_value, phase2_value, "cache reload must return the same value");
}

// ─── Real-RPC tests (#[ignore], skipped by default) ─────────────────────────

/// End-to-end fork smoke test: build a fork session against `MegaETH` mainnet,
/// query the `MegaETH` Oracle system contract via `basic_ref`, persist, and
/// verify the cache file was written with content.
///
/// Defaults to `https://mainnet.megaeth.com/rpc`. Override via the
/// `MEGA_EVME_TEST_RPC_URL` environment variable. Run with:
///
/// ```text
/// cargo test --manifest-path bin/mega-evme/Cargo.toml --test state \
///     test_create_initial_state_fork_real_rpc_smoke -- --ignored
/// ```
///
/// Stability: the Oracle is a `MegaETH` system contract predeployed at
/// `0x6342...0001` from genesis (see `crates/mega-evm/src/system/oracle.rs`),
/// so its code is guaranteed to be present at any block number >= 1. The
/// chosen historical block is far enough in the past that no public archive
/// RPC will return different state for it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access; run with cargo test -- --ignored"]
async fn test_create_initial_state_fork_real_rpc_smoke() {
    let dir = tempdir().expect("tempdir");
    let block_number: u64 = 100_000;

    let rpc_url = std::env::var("MEGA_EVME_TEST_RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.megaeth.com/rpc".to_string());
    let block_number_str = block_number.to_string();

    let prestate_args =
        PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
    let rpc_args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &rpc_url,
        "--rpc.cache-size",
        "256",
        "--rpc.cache-dir",
        dir.path().to_str().unwrap(),
    ]);
    let sender = Address::ZERO;

    let (state, cache_store) =
        prestate_args.create_initial_state(&sender, &rpc_args).await.expect("create_initial_state");
    assert!(!cache_store.is_noop(), "fork session must own a real cache store");

    let oracle =
        Address::from_str("0x6342000000000000000000000000000000000001").expect("address parse");
    let account = state
        .basic_ref(oracle)
        .expect("basic_ref against real RPC")
        .expect("MegaETH Oracle must exist on MegaETH mainnet at this block");
    assert!(
        account.code_hash != B256::from(alloy_primitives::KECCAK256_EMPTY),
        "MegaETH Oracle must be a contract account, not an EOA",
    );

    cache_store.persist().expect("persist");

    // MegaETH mainnet chain id is 4326, so the cache filename is fixed.
    let cache_file = rpc_cache_path(dir.path(), 4326, &rpc_url);
    assert!(cache_file.exists(), "cache file must exist after persist");
    let metadata = std::fs::metadata(&cache_file).expect("metadata");
    assert!(metadata.len() > 0, "cache file must be non-empty after a real query");
}

/// Real-RPC twin of `test_storage_cache_hit_round_trip_via_mock`. Phase 1
/// queries a `MegaETH` Oracle storage slot against a public mainnet endpoint
/// and persists; phase 2 rebuilds against the same RPC URL (needed for the
/// `eth_chainId` resolution) and relies on the alloy cache to serve the
/// same slot without an additional `eth_getStorageAt` round-trip.
/// Useful as a manual smoke test for the full live stack.
///
/// Probes through `storage_ref` rather than `basic_ref` because alloy's
/// `CacheLayer` (at the pinned version) only intercepts `eth_getStorageAt` /
/// `eth_getCode` / `eth_getProof`; `basic_ref` fans out to `eth_getBalance`
/// and `eth_getTransactionCount` too, neither of which is cached.
///
/// Defaults to `https://mainnet.megaeth.com/rpc`. Override via the
/// `MEGA_EVME_TEST_RPC_URL` environment variable.
///
/// ```text
/// cargo test --manifest-path bin/mega-evme/Cargo.toml --test state \
///     test_create_initial_state_fork_real_rpc_storage_cache_hit -- --ignored
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access for phase 1"]
async fn test_create_initial_state_fork_real_rpc_storage_cache_hit() {
    let dir = tempdir().expect("tempdir");
    let block_number: u64 = 100_000;
    let block_number_str = block_number.to_string();

    let rpc_url = std::env::var("MEGA_EVME_TEST_RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.megaeth.com/rpc".to_string());

    // MegaETH Oracle system contract — predeployed from genesis.
    let oracle =
        Address::from_str("0x6342000000000000000000000000000000000001").expect("address parse");
    let slot = U256::ZERO;

    // ── Phase 1: real RPC, populate cache via storage_ref, persist. ──────
    let phase1_value = {
        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = RpcArgs::parse_from([
            "mega-evme",
            "--rpc",
            &rpc_url,
            "--rpc.cache-size",
            "256",
            "--rpc.cache-dir",
            dir.path().to_str().unwrap(),
        ]);
        let sender = Address::ZERO;
        let (state, cache_store) = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 1");
        let value =
            state.storage_ref(oracle, slot).expect("storage_ref phase 1 — real RPC must succeed");
        cache_store.persist().expect("persist");
        value
    };

    let cache_file = rpc_cache_path(dir.path(), 4326, &rpc_url);
    assert!(cache_file.exists(), "cache file must exist after phase 1 persist");
    let metadata = std::fs::metadata(&cache_file).expect("metadata");
    assert!(metadata.len() > 0, "cache file must be non-empty after phase 1");

    // ── Phase 2: same RPC URL + same cache dir, storage served from cache. ──
    let phase2_value = {
        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        // Uses the same live RPC URL so the `eth_chainId` resolution
        // succeeds. The storage query itself must be served from the
        // persisted cache without an additional network round-trip.
        let rpc_args = RpcArgs::parse_from([
            "mega-evme",
            "--rpc",
            &rpc_url,
            "--rpc.cache-size",
            "256",
            "--rpc.cache-dir",
            dir.path().to_str().unwrap(),
        ]);
        let sender = Address::ZERO;
        let (state, _cache_store) = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 2");
        state.storage_ref(oracle, slot).expect("storage_ref phase 2 — must hit cache, not network")
    };

    assert_eq!(phase1_value, phase2_value, "cache reload must return the same storage value");
}

/// A forked state reads an account the RPC reports as all zeros as absent, and an account with a
/// balance as present: an RPC answers zeros for an account that does not exist.
#[tokio::test(flavor = "multi_thread")]
async fn test_forked_state_reads_an_all_zero_account_as_absent() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let absent = Address::from_str("0x00000000000000000000000000000000000000aa").unwrap();
    let funded = Address::from_str("0x00000000000000000000000000000000000000bb").unwrap();
    let server = MockServer::start().await;
    let respond = |method: &'static str, needle: Option<String>, result: &'static str| {
        let mut mock = Mock::given(matchers::method("POST"))
            .and(matchers::body_string_contains(format!("\"{method}\"")));
        if let Some(needle) = needle {
            mock = mock.and(matchers::body_string_contains(needle));
        }
        mock.respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "jsonrpc": "2.0", "id": body["id"], "result": result }),
            )
        })
    };
    respond("eth_chainId", None, "0x10e6").mount(&server).await;
    respond("eth_getBalance", Some(format!("{funded:#x}")), "0x64")
        .with_priority(1)
        .mount(&server)
        .await;
    respond("eth_getBalance", None, "0x0").with_priority(2).mount(&server).await;
    respond("eth_getTransactionCount", None, "0x0").mount(&server).await;
    respond("eth_getCode", None, "0x").mount(&server).await;

    let prestate_args = PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", "100"]);
    let rpc_args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.no-cache-file",
        "--rpc.max-retries",
        "0",
    ]);
    let (state, _store) =
        prestate_args.create_initial_state(&Address::ZERO, &rpc_args).await.expect("fork");

    assert_eq!(state.basic_ref(absent).expect("absent account"), None);
    let present = state.basic_ref(funded).expect("funded account").expect("present");
    assert_eq!(present.balance, U256::from(100));
}

const FORK_ACCOUNT: &str = "0x00000000000000000000000000000000000000aa";

/// Answers `eth_chainId` and one account: balance 100, nonce 7, code `0x6000`, slot 0 = `0x11`,
/// any other slot = `0x22`.
async fn mount_fork_account(server: &wiremock::MockServer) {
    use wiremock::{matchers, Mock, ResponseTemplate};

    Mock::given(matchers::method("POST"))
        .respond_with(|req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let method = body["method"].as_str().unwrap_or("");
            let params = body["params"].as_array();
            let result = match method {
                "eth_chainId" => "0x10e6".to_string(),
                "eth_getBalance" => "0x64".to_string(),
                "eth_getTransactionCount" => "0x7".to_string(),
                "eth_getCode" => "0x6000".to_string(),
                "eth_getStorageAt" => {
                    let slot = params
                        .and_then(|params| params.get(1))
                        .and_then(|slot| slot.as_str())
                        .and_then(|slot| U256::from_str(slot).ok())
                        .unwrap_or(U256::ZERO);
                    if slot.is_zero() {
                        "0x11".to_string()
                    } else {
                        "0x22".to_string()
                    }
                }
                _ => "0x0".to_string(),
            };
            let id = body["id"].clone();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }))
        })
        .mount(server)
        .await;
}

fn fork_rpc_args(url: &str) -> RpcArgs {
    RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        url,
        "--rpc.no-cache-file",
        "--rpc.max-retries",
        "0",
    ])
}

/// A balance, faucet, or storage override names one field of a forked account. The account's
/// nonce, code and every other slot stay the values the fork returned.
#[tokio::test(flavor = "multi_thread")]
async fn test_field_override_keeps_the_rest_of_a_forked_account() {
    let code_hash = alloy_primitives::keccak256([0x60, 0x00]);
    let address = Address::from_str(FORK_ACCOUNT).unwrap();

    let server = wiremock::MockServer::start().await;
    mount_fork_account(&server).await;
    let rpc_args = fork_rpc_args(&server.uri());

    let storage_only = PreStateArgs::parse_from([
        "mega-evme",
        "--fork",
        "--fork.block",
        "100",
        "--storage",
        &format!("{FORK_ACCOUNT}:0x0=0x99"),
    ]);
    let (state, _) = storage_only
        .create_initial_state(&Address::ZERO, &rpc_args)
        .await
        .expect("storage override");
    let info = state.basic_ref(address).expect("basic").expect("present");
    assert_eq!(info.balance, U256::from(100), "a storage override must not replace the balance");
    assert_eq!(info.nonce, 7, "a storage override must not replace the nonce");
    assert_eq!(info.code_hash, code_hash, "a storage override must not replace the code");
    assert_eq!(state.storage_ref(address, U256::ZERO).expect("slot 0"), U256::from(0x99));
    assert_eq!(
        state.storage_ref(address, U256::from(1)).expect("slot 1"),
        U256::from(0x22),
        "a storage override must not hide the account's other slots",
    );

    let balance_only = PreStateArgs::parse_from([
        "mega-evme",
        "--fork",
        "--fork.block",
        "100",
        "--balance",
        &format!("{FORK_ACCOUNT}=5"),
    ]);
    let (state, _) = balance_only
        .create_initial_state(&Address::ZERO, &rpc_args)
        .await
        .expect("balance override");
    let info = state.basic_ref(address).expect("basic").expect("present");
    assert_eq!(info.balance, U256::from(5));
    assert_eq!(info.nonce, 7);
    assert_eq!(info.code_hash, code_hash);
    assert_eq!(state.storage_ref(address, U256::from(1)).expect("slot 1"), U256::from(0x22));

    let faucet = PreStateArgs::parse_from([
        "mega-evme",
        "--fork",
        "--fork.block",
        "100",
        "--faucet",
        &format!("{FORK_ACCOUNT}+=50"),
    ]);
    let (state, _) = faucet.create_initial_state(&address, &rpc_args).await.expect("faucet");
    let info = state.basic_ref(address).expect("basic").expect("present");
    assert_eq!(info.balance, U256::from(150));
    assert_eq!(info.nonce, 7);
    assert_eq!(info.code_hash, code_hash);

    let sender_balance = PreStateArgs::parse_from([
        "mega-evme",
        "--fork",
        "--fork.block",
        "100",
        "--sender.balance",
        "9",
    ]);
    let (state, _) =
        sender_balance.create_initial_state(&address, &rpc_args).await.expect("sender balance");
    let info = state.basic_ref(address).expect("basic").expect("present");
    assert_eq!(info.balance, U256::from(9));
    assert_eq!(info.nonce, 7);
    assert_eq!(info.code_hash, code_hash);
}

/// On an empty state the same flags still set exactly the fields they name.
#[tokio::test]
async fn test_field_override_on_empty_state_sets_the_named_field() {
    let address = Address::from_str(FORK_ACCOUNT).unwrap();
    let rpc_args = test_rpc_args("http://127.0.0.1:1", None);
    let args = PreStateArgs::parse_from([
        "mega-evme",
        "--balance",
        &format!("{FORK_ACCOUNT}=10"),
        "--faucet",
        &format!("{FORK_ACCOUNT}+=5"),
        "--storage",
        &format!("{FORK_ACCOUNT}:0x0=0x9"),
    ]);
    let (state, store) =
        args.create_initial_state(&Address::ZERO, &rpc_args).await.expect("local overrides");
    assert!(store.is_noop());
    let info = state.basic_ref(address).expect("basic").expect("the balance override creates it");
    assert_eq!(info.balance, U256::from(15));
    assert_eq!(info.nonce, 0);
    assert_eq!(state.storage_ref(address, U256::ZERO).expect("slot 0"), U256::from(9));
    assert_eq!(state.storage_ref(address, U256::from(1)).expect("slot 1"), U256::ZERO);
}

/// A faucet that does not fit in the balance is refused, whether the balance comes from the
/// prestate file, from the sum of the faucet entries, or from the account the override lands on.
#[tokio::test]
async fn test_faucet_overflow_is_invalid_input() {
    use mega_evme::EvmeError;

    let rpc_args = test_rpc_args("http://127.0.0.1:1", None);
    let max = U256::MAX.to_string();
    let overflow = |err: EvmeError| matches!(err, EvmeError::InvalidInput(msg) if msg.contains("Faucet overflows"));

    let summed = PreStateArgs::parse_from([
        "mega-evme",
        "--faucet",
        &format!("{FORK_ACCOUNT}+={max}"),
        "--faucet",
        &format!("{FORK_ACCOUNT}+=1"),
    ]);
    let err = summed
        .create_initial_state(&Address::ZERO, &rpc_args)
        .await
        .expect_err("two faucet entries past the maximum");
    assert!(overflow(err));

    let replaced = PreStateArgs::parse_from([
        "mega-evme",
        "--balance",
        &format!("{FORK_ACCOUNT}={max}"),
        "--faucet",
        &format!("{FORK_ACCOUNT}+=1"),
    ]);
    let (state, _) = replaced.create_initial_state(&Address::ZERO, &rpc_args).await.expect("build");
    let err = state
        .basic_ref(Address::from_str(FORK_ACCOUNT).unwrap())
        .expect_err("balance override plus faucet past the maximum");
    assert!(overflow(err));

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("prestate.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"{FORK_ACCOUNT}":{{"balance":"0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","nonce":"0x4","code":"0x","codeHash":"0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470","storage":{{"0x1":"0x3"}}}}}}"#
        ),
    )
    .expect("write prestate");
    let from_file = PreStateArgs::parse_from([
        "mega-evme",
        "--prestate",
        path.to_str().unwrap(),
        "--faucet",
        &format!("{FORK_ACCOUNT}+=1"),
    ]);
    let err = from_file
        .create_initial_state(&Address::ZERO, &rpc_args)
        .await
        .expect_err("prestate balance plus faucet past the maximum");
    assert!(overflow(err));
}

/// Overrides of an account the prestate file already holds change that field and leave the
/// account's other fields, including its other slots.
#[tokio::test]
async fn test_field_override_keeps_prestate_account_fields() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("prestate.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"{FORK_ACCOUNT}":{{"balance":"0x8","nonce":"0x4","code":"0x","codeHash":"0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470","storage":{{"0x1":"0x3"}}}}}}"#
        ),
    )
    .expect("write prestate");
    let args = PreStateArgs::parse_from([
        "mega-evme",
        "--prestate",
        path.to_str().unwrap(),
        "--balance",
        &format!("{FORK_ACCOUNT}=1"),
        "--faucet",
        &format!("{FORK_ACCOUNT}+=5"),
        "--storage",
        &format!("{FORK_ACCOUNT}:0x0=0x9"),
    ]);
    let rpc_args = test_rpc_args("http://127.0.0.1:1", None);
    let (state, _) = args.create_initial_state(&Address::ZERO, &rpc_args).await.expect("prestate");
    let address = Address::from_str(FORK_ACCOUNT).unwrap();
    let info = state.basic_ref(address).expect("basic").expect("present");
    assert_eq!(info.balance, U256::from(6));
    assert_eq!(info.nonce, 4);
    assert_eq!(state.storage_ref(address, U256::ZERO).expect("slot 0"), U256::from(9));
    assert_eq!(state.storage_ref(address, U256::from(1)).expect("slot 1"), U256::from(3));
}
