//! Integration tests for `InitialStateSession` wiring across the fork and
//! non-fork branches of `create_initial_state`.
//!
//! Synthetic tests use `--fork.block N` to skip real RPC traffic during
//! construction (`EvmeState::new_forked` only contacts the network when
//! `fork_block` is `None`). Tests gated by `#[ignore]` exercise a live
//! Optimism mainnet endpoint and are skipped by default.

use std::str::FromStr;

use alloy_primitives::{Address, B256, U256};
use clap::Parser;
use mega_evm::revm::DatabaseRef;
use mega_evme::common::{PreStateArgs, RpcArgs};
use tempfile::tempdir;

mod common;
use common::MockRpcServer;

// ─── Synthetic branch wiring tests (no network required) ─────────────────────

/// Non-fork must produce a no-op finalizer regardless of the cache flags
/// `RpcArgs` was given — non-fork doesn't touch RPC, so there is nothing to
/// persist. The cache file must NOT be created even though it was configured.
#[tokio::test]
async fn test_create_initial_state_non_fork_finalize_is_noop() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("must-not-be-created.json");

    let prestate_args = PreStateArgs::parse_from(["mega-evme"]);
    let rpc_args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "16",
        "--rpc.cache-file",
        cache_path.to_str().unwrap(),
    ]);
    let sender = Address::ZERO;

    let session =
        prestate_args.create_initial_state(&sender, &rpc_args).await.expect("create_initial_state");

    assert!(session.is_noop(), "non-fork must wire a no-op finalizer");
    session.finalize();
    assert!(!cache_path.exists(), "non-fork finalize must not produce any cache file");
}

/// Fork mode with `--fork.block N` constructs the state without hitting the
/// network, but still wires a real finalizer that writes the configured file
/// on `finalize()`.
///
/// Uses the multi-thread runtime because `WrapDatabaseAsync::new()` requires a
/// blocking-capable tokio handle even though no RPC call is actually issued.
#[tokio::test(flavor = "multi_thread")]
async fn test_create_initial_state_fork_finalize_persists_cache() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("fork-cache.json");

    let prestate_args =
        PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", "1000000"]);
    let rpc_args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "16",
        "--rpc.cache-file",
        cache_path.to_str().unwrap(),
    ]);
    let sender = Address::ZERO;

    let session =
        prestate_args.create_initial_state(&sender, &rpc_args).await.expect("create_initial_state");

    assert!(!session.is_noop(), "fork + cache_file must wire a real finalizer");
    session.finalize();
    assert!(cache_path.exists(), "fork finalize must persist the cache file");
}

/// End-to-end cache reload across two fresh session builds, with cache hit
/// proven by observing zero outbound requests in phase 2.
///
/// Phase 1 mock returns a real `eth_getStorageAt` response, the value is
/// persisted via `finalize()`. Phase 2 uses a *separate* empty mock against
/// the same cache file and asserts it received zero requests — the storage
/// query must be served entirely from the loaded file.
///
/// This is the deterministic, default-CI counterpart of the `#[ignore]`d
/// real-RPC cache hit test below.
#[tokio::test(flavor = "multi_thread")]
async fn test_storage_cache_hit_round_trip_via_mock() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("mock-cache.json");
    let block_number: u64 = 1_234_567;
    let block_number_str = block_number.to_string();

    let weth =
        Address::from_str("0x4200000000000000000000000000000000000006").expect("address parse");
    let slot = U256::ZERO;
    // 32-byte hex, the JSON-RPC shape of an `eth_getStorageAt` result.
    let storage_hex = "0x000000000000000000000000000000000000000000000000000000000000002a";
    let expected_value = U256::from(0x2a);

    // ── Phase 1: mock returns the storage value, populate cache, finalize. ─
    let phase1_value = {
        let server = MockRpcServer::start().await;
        server.respond_jsonrpc_result(storage_hex, 1).await;

        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = RpcArgs::parse_from([
            "mega-evme",
            "--rpc",
            &server.uri(),
            "--rpc.cache-size",
            "256",
            "--rpc.cache-file",
            cache_path.to_str().unwrap(),
        ]);
        let sender = Address::ZERO;

        let mut session = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 1");
        let value = session.state_mut().storage_ref(weth, slot).expect("storage_ref phase 1");
        session.finalize();

        assert!(
            server.received_request_count().await > 0,
            "phase 1 must reach the mock to populate the cache"
        );

        value
    };

    assert_eq!(phase1_value, expected_value, "phase 1 must parse the mock's hex result");
    assert!(cache_path.exists(), "phase 1 finalize must produce the cache file");

    // ── Phase 2: empty mock, same cache file. ─────────────────────────────
    let phase2_value = {
        // No mock mounted: any outbound request would be answered by
        // wiremock's default 404 handler, which `storage_ref` would then
        // surface as an error. The zero-request assertion below catches that.
        let server = MockRpcServer::start().await;

        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = RpcArgs::parse_from([
            "mega-evme",
            "--rpc",
            &server.uri(),
            "--rpc.cache-size",
            "256",
            "--rpc.cache-file",
            cache_path.to_str().unwrap(),
        ]);
        let sender = Address::ZERO;

        let mut session = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 2");
        let value = session
            .state_mut()
            .storage_ref(weth, slot)
            .expect("storage_ref phase 2 — must hit cache");

        assert_eq!(
            server.received_request_count().await,
            0,
            "cache hit: storage_ref must not reach the mock at all"
        );

        value
    };

    assert_eq!(phase1_value, phase2_value, "cache reload must return the same value");
}

// ─── Real-RPC tests (#[ignore], skipped by default) ─────────────────────────

/// End-to-end fork smoke test: build a fork session against a real OP RPC,
/// query an OP system contract via `basic_ref`, finalize, and verify the
/// cache file was written with content.
///
/// Defaults to `https://mainnet.optimism.io`. Override via the
/// `MEGA_EVME_TEST_RPC_URL` environment variable. Run with:
///
/// ```text
/// cargo test -p mega-evme test_create_initial_state_fork_real_rpc_smoke -- --ignored
/// ```
///
/// Stability: WETH9 has lived at the OP predeploy address `0x4200...0006`
/// since Bedrock; the chosen historical block is far enough in the past that
/// no public archive RPC will return different state for it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access; run with cargo test -- --ignored"]
async fn test_create_initial_state_fork_real_rpc_smoke() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("smoke-cache.json");
    let block_number: u64 = 110_000_000;

    let rpc_url = std::env::var("MEGA_EVME_TEST_RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.optimism.io".to_string());
    let block_number_str = block_number.to_string();

    let prestate_args =
        PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
    let rpc_args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &rpc_url,
        "--rpc.cache-size",
        "256",
        "--rpc.cache-file",
        cache_path.to_str().unwrap(),
    ]);
    let sender = Address::ZERO;

    let mut session =
        prestate_args.create_initial_state(&sender, &rpc_args).await.expect("create_initial_state");
    assert!(!session.is_noop(), "fork session must own a real finalizer");

    let weth =
        Address::from_str("0x4200000000000000000000000000000000000006").expect("address parse");
    let account = session
        .state_mut()
        .basic_ref(weth)
        .expect("basic_ref against real RPC")
        .expect("WETH9 must exist on Optimism mainnet at this block");
    assert!(
        account.code_hash != B256::from(alloy_primitives::KECCAK256_EMPTY),
        "WETH9 must be a contract account, not an EOA"
    );

    session.finalize();

    assert!(cache_path.exists(), "cache file must exist after finalize");
    let metadata = std::fs::metadata(&cache_path).expect("metadata");
    assert!(metadata.len() > 0, "cache file must be non-empty after a real query");
}

/// Real-RPC twin of `test_storage_cache_hit_round_trip_via_mock`. Phase 1
/// queries a WETH9 storage slot against a public Optimism endpoint and
/// finalizes; phase 2 rebuilds against a definitely-closed local port and
/// relies on the alloy cache to serve the same slot without touching the
/// network. Useful as a manual smoke test for the full live stack.
///
/// Probes through `storage_ref` rather than `basic_ref` because alloy's
/// `CacheLayer` (at the pinned version) only intercepts `eth_getStorageAt` /
/// `eth_getCode` / `eth_getProof`; `basic_ref` fans out to `eth_getBalance`
/// and `eth_getTransactionCount` too, neither of which is cached.
///
/// Defaults to `https://mainnet.optimism.io`. Override via the
/// `MEGA_EVME_TEST_RPC_URL` environment variable. Run with:
///
/// ```text
/// cargo test -p mega-evme test_create_initial_state_fork_real_rpc_storage_cache_hit -- --ignored
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access for phase 1; disable HTTP proxies before running"]
async fn test_create_initial_state_fork_real_rpc_storage_cache_hit() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("storage-cache.json");
    let block_number: u64 = 110_000_000;
    let block_number_str = block_number.to_string();

    let rpc_url = std::env::var("MEGA_EVME_TEST_RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.optimism.io".to_string());

    // Definitely-closed local port: bind to port 0, capture the OS-assigned
    // port, drop the listener. Connection attempts to it now ECONNREFUSED.
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        port
    };
    let unreachable_url = format!("http://127.0.0.1:{}", closed_port);

    let weth =
        Address::from_str("0x4200000000000000000000000000000000000006").expect("address parse");
    let slot = U256::ZERO;

    // ── Phase 1: real RPC, populate cache via storage_ref, finalize. ─────
    let phase1_value = {
        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = RpcArgs::parse_from([
            "mega-evme",
            "--rpc",
            &rpc_url,
            "--rpc.cache-size",
            "256",
            "--rpc.cache-file",
            cache_path.to_str().unwrap(),
        ]);
        let sender = Address::ZERO;
        let mut session = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 1");
        let value = session
            .state_mut()
            .storage_ref(weth, slot)
            .expect("storage_ref phase 1 — real RPC must succeed");
        session.finalize();
        value
    };

    assert!(cache_path.exists(), "cache file must exist after phase 1 finalize");
    let metadata = std::fs::metadata(&cache_path).expect("metadata");
    assert!(metadata.len() > 0, "cache file must be non-empty after phase 1");

    // ── Phase 2: unreachable URL + same cache file, same storage query. ──
    let phase2_value = {
        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = RpcArgs::parse_from([
            "mega-evme",
            "--rpc",
            &unreachable_url,
            "--rpc.cache-size",
            "256",
            "--rpc.cache-file",
            cache_path.to_str().unwrap(),
            "--rpc.max-retries",
            "0",
        ]);
        let sender = Address::ZERO;
        let mut session = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 2");
        let value = session
            .state_mut()
            .storage_ref(weth, slot)
            .expect("storage_ref phase 2 — must hit cache, not network");
        value
    };

    assert_eq!(phase1_value, phase2_value, "cache reload must return the same storage value");
}
