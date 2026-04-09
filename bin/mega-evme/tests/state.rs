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
use common::{test_rpc_args_cached, MockRpcServer};

// ─── Synthetic branch wiring tests (no network required) ─────────────────────

/// Non-fork must produce a no-op cache store regardless of the flags
/// `RpcArgs` was given — non-fork doesn't touch RPC, so there is nothing to
/// persist. The cache file must NOT be created even though it was configured.
#[tokio::test]
async fn test_create_initial_state_non_fork_store_is_noop() {
    let dir = tempdir().expect("tempdir");

    let prestate_args = PreStateArgs::parse_from(["mega-evme"]);
    // 4326 = MegaETH mainnet.
    let rpc_args = test_rpc_args_cached("http://localhost:8545", dir.path(), 4326, None);
    let sender = Address::ZERO;

    let (_state, cache_store) =
        prestate_args.create_initial_state(&sender, &rpc_args).await.expect("create_initial_state");

    assert!(cache_store.is_noop(), "non-fork must wire a no-op cache store");
    cache_store.persist();
    assert!(
        !dir.path().join("rpc-cache-4326.json").exists(),
        "non-fork persist must not produce any cache file",
    );
}

/// Fork mode with `--fork.block N` constructs the state without hitting the
/// network, but still wires a real store that writes the configured file on
/// `persist()`. The `--rpc.chain-id` override keeps the whole flow hermetic.
///
/// Uses the multi-thread runtime because `WrapDatabaseAsync::new()` requires a
/// blocking-capable tokio handle even though no RPC call is actually issued.
#[tokio::test(flavor = "multi_thread")]
async fn test_create_initial_state_fork_store_persists_cache() {
    let dir = tempdir().expect("tempdir");

    let prestate_args =
        PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", "1000000"]);
    // 4326 = MegaETH mainnet.
    let rpc_args = test_rpc_args_cached("http://localhost:8545", dir.path(), 4326, None);
    let sender = Address::ZERO;

    let (_state, cache_store) =
        prestate_args.create_initial_state(&sender, &rpc_args).await.expect("create_initial_state");

    assert!(!cache_store.is_noop(), "fork + cache_dir must wire a real store");
    cache_store.persist();
    assert!(
        dir.path().join("rpc-cache-4326.json").exists(),
        "fork persist must write the cache file",
    );
}

/// End-to-end cache reload across two fresh session builds, with cache hit
/// proven by observing zero outbound requests in phase 2.
///
/// Phase 1 mock returns a real `eth_getStorageAt` response, the value is
/// persisted via `persist()`. Phase 2 uses a *separate* empty mock against
/// the same cache directory and asserts it received zero requests — the
/// storage query must be served entirely from the loaded file. Both phases
/// use `--rpc.chain-id` to skip the `eth_chainId` fetch.
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

    // ── Phase 1: mock returns the storage value, populate cache, persist. ──
    let phase1_value = {
        let server = MockRpcServer::start().await;
        server.respond_jsonrpc_result(storage_hex, 1).await;

        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = test_rpc_args_cached(&server.uri(), dir.path(), chain_id, None);
        let sender = Address::ZERO;

        let (state, cache_store) = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 1");
        let value = state.storage_ref(oracle, slot).expect("storage_ref phase 1");
        cache_store.persist();

        assert!(
            server.received_request_count().await > 0,
            "phase 1 must reach the mock to populate the cache",
        );

        value
    };

    assert_eq!(phase1_value, expected_value, "phase 1 must parse the mock's hex result");
    assert!(
        dir.path().join(format!("rpc-cache-{chain_id}.json")).exists(),
        "phase 1 persist must produce the cache file",
    );

    // ── Phase 2: empty mock, same cache directory. ────────────────────────
    let phase2_value = {
        // No mock mounted: any outbound request would be answered by
        // wiremock's default 404 handler, which `storage_ref` would then
        // surface as an error. The zero-request assertion below catches that.
        let server = MockRpcServer::start().await;

        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        let rpc_args = test_rpc_args_cached(&server.uri(), dir.path(), chain_id, None);
        let sender = Address::ZERO;

        let (state, _cache_store) = prestate_args
            .create_initial_state(&sender, &rpc_args)
            .await
            .expect("create_initial_state phase 2");
        let value = state.storage_ref(oracle, slot).expect("storage_ref phase 2 — must hit cache");

        assert_eq!(
            server.received_request_count().await,
            0,
            "cache hit: storage_ref must not reach the mock at all",
        );

        value
    };

    assert_eq!(phase1_value, phase2_value, "cache reload must return the same value");
}

// ─── Real-RPC tests (#[ignore], skipped by default) ─────────────────────────

/// End-to-end fork smoke test: build a fork session against `MegaETH` mainnet,
/// query the `MegaETH` Oracle system contract via `basic_ref`, persist, and
/// verify the cache file was written with content. This test exercises the
/// real `eth_chainId` fetch path (no `--rpc.chain-id` override).
///
/// Defaults to `https://mainnet.megaeth.com/rpc`. Override via the
/// `MEGA_EVME_TEST_RPC_URL` environment variable. Run with:
///
/// ```text
/// cargo test -p mega-evme test_create_initial_state_fork_real_rpc_smoke -- --ignored
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
    // No --rpc.chain-id: exercise the real eth_chainId resolution path.
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

    cache_store.persist();

    // MegaETH mainnet chain id is 4326, so the cache filename is fixed.
    let cache_file = dir.path().join("rpc-cache-4326.json");
    assert!(cache_file.exists(), "cache file must exist after persist");
    let metadata = std::fs::metadata(&cache_file).expect("metadata");
    assert!(metadata.len() > 0, "cache file must be non-empty after a real query");
}

/// Real-RPC twin of `test_storage_cache_hit_round_trip_via_mock`. Phase 1
/// queries a `MegaETH` Oracle storage slot against a public mainnet endpoint
/// and persists; phase 2 rebuilds against a definitely-closed local port
/// and relies on the alloy cache to serve the same slot without touching
/// the network. Useful as a manual smoke test for the full live stack.
///
/// Probes through `storage_ref` rather than `basic_ref` because alloy's
/// `CacheLayer` (at the pinned version) only intercepts `eth_getStorageAt` /
/// `eth_getCode` / `eth_getProof`; `basic_ref` fans out to `eth_getBalance`
/// and `eth_getTransactionCount` too, neither of which is cached.
///
/// Defaults to `https://mainnet.megaeth.com/rpc`. Override via the
/// `MEGA_EVME_TEST_RPC_URL` environment variable. Phase 2 passes
/// `--rpc.chain-id 4326` so the `eth_chainId` call doesn't attempt to
/// reach the closed port.
///
/// ```text
/// cargo test -p mega-evme test_create_initial_state_fork_real_rpc_storage_cache_hit -- --ignored
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access for phase 1"]
async fn test_create_initial_state_fork_real_rpc_storage_cache_hit() {
    let dir = tempdir().expect("tempdir");
    let block_number: u64 = 100_000;
    let block_number_str = block_number.to_string();

    let rpc_url = std::env::var("MEGA_EVME_TEST_RPC_URL")
        .unwrap_or_else(|_| "https://mainnet.megaeth.com/rpc".to_string());

    // Definitely-closed local port: bind to port 0, capture the OS-assigned
    // port, drop the listener. Connection attempts to it now ECONNREFUSED.
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        port
    };
    let unreachable_url = format!("http://127.0.0.1:{}", closed_port);

    // MegaETH Oracle system contract — predeployed from genesis.
    let oracle =
        Address::from_str("0x6342000000000000000000000000000000000001").expect("address parse");
    let slot = U256::ZERO;

    // ── Phase 1: real RPC, populate cache via storage_ref, persist. ──────
    let phase1_value = {
        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        // No --rpc.chain-id: exercise the real eth_chainId resolution path.
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
        cache_store.persist();
        value
    };

    let cache_file = dir.path().join("rpc-cache-4326.json");
    assert!(cache_file.exists(), "cache file must exist after phase 1 persist");
    let metadata = std::fs::metadata(&cache_file).expect("metadata");
    assert!(metadata.len() > 0, "cache file must be non-empty after phase 1");

    // ── Phase 2: unreachable URL + same cache dir + explicit chain-id. ──
    let phase2_value = {
        let prestate_args =
            PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", &block_number_str]);
        // `--rpc.chain-id 4326` bypasses the eth_chainId call that would
        // otherwise hit the unreachable URL before we get to the cached
        // storage query.
        let rpc_args = RpcArgs::parse_from([
            "mega-evme",
            "--rpc",
            &unreachable_url,
            "--rpc.cache-size",
            "256",
            "--rpc.cache-dir",
            dir.path().to_str().unwrap(),
            "--rpc.chain-id",
            "4326",
            "--rpc.max-retries",
            "0",
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
