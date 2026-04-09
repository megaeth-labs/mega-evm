//! Integration tests for the public RPC session API and the retry layer.
//!
//! Covers: CLI parsing of all `--rpc.*` flags, `build_session` shape across
//! cache configurations, atomic save round-trip via the test-utils
//! `cache()` accessor, `RpcSession::into_parts`, and the retry policy on
//! both branches of its coverage (HTTP 429/503 via wiremock; transport
//! failures via a closed local port). Tests for the private
//! `temp_path_for` helper stay inline in `src/common/provider.rs`.

use std::path::PathBuf;

use alloy_primitives::B256;
use alloy_provider::Provider;
use clap::Parser;
use mega_evme::common::{EvmeError, RpcArgs};
use tempfile::tempdir;

mod common;
use common::{test_rpc_args, MockRpcServer};

// ─── CLI parsing ─────────────────────────────────────────────────────────────

#[test]
fn test_rpc_args_parses_all_new_flags() {
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "https://example.test/rpc",
        "--rpc.cache-size",
        "256",
        "--rpc.cache-file",
        "/tmp/example-cache.json",
        "--rpc.max-retries",
        "7",
        "--rpc.backoff-ms",
        "250",
        "--rpc.rate-limit",
        "1234",
    ]);
    assert_eq!(args.rpc_url, "https://example.test/rpc");
    assert_eq!(args.cache_size, 256);
    assert_eq!(args.cache_file, Some(PathBuf::from("/tmp/example-cache.json")));
    assert_eq!(args.max_retries, 7);
    assert_eq!(args.backoff_ms, 250);
    assert_eq!(args.compute_units_per_sec, 1234);
}

/// Pin the default values so that any future change to a `default_value_t` in
/// `RpcArgs` shows up as a clean test failure rather than as silent behavior
/// drift in the retry / cache layers.
#[test]
fn test_rpc_args_default_values() {
    let args = RpcArgs::parse_from(["mega-evme"]);
    assert_eq!(args.rpc_url, "http://localhost:8545");
    assert_eq!(args.cache_size, 10_000);
    assert_eq!(args.cache_file, None);
    assert_eq!(args.max_retries, 5);
    assert_eq!(args.backoff_ms, 1_000);
    assert_eq!(args.compute_units_per_sec, 660);
}

// ─── build_session shape variants ────────────────────────────────────────────

#[test]
fn test_build_session_without_cache() {
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "0",
    ]);
    let session = args.build_session().expect("build_session");
    assert!(session.is_noop(), "cache_size == 0 must produce a no-op finalizer");
    session.finalize();
}

/// `cache_size > 0` without `cache_file` keeps the in-memory cache active but
/// has nothing to persist on exit, so the finalizer is still no-op.
#[test]
fn test_build_session_with_cache_but_no_file() {
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "100",
    ]);
    let session = args.build_session().expect("build_session");
    assert!(session.is_noop(), "no cache_file means nothing to persist");
    session.finalize();
}

#[test]
fn test_build_session_with_cache_and_file() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("cache.json");
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "100",
        "--rpc.cache-file",
        path.to_str().unwrap(),
    ]);
    let session = args.build_session().expect("build_session");
    assert!(!session.is_noop(), "cache_size > 0 + cache_file must produce a real finalizer");

    let (_provider, finalizer) = session.into_parts();
    assert_eq!(finalizer.cache_file(), Some(path.as_path()));
}

#[test]
fn test_build_session_invalid_url() {
    let args = RpcArgs::parse_from(["mega-evme", "--rpc", "not a url", "--rpc.cache-size", "0"]);
    let err = args.build_session().expect_err("build_session should fail");
    match err {
        EvmeError::RpcError(msg) => {
            assert!(msg.contains("not a url"), "error must echo the original input, got: {msg}");
        }
        other => panic!("expected EvmeError::RpcError, got {other:?}"),
    }
}

// ─── Cache file round-trip ───────────────────────────────────────────────────

/// Seed an entry through the cache, finalize, build a fresh session against
/// the same path, and confirm the entry comes back via the load path.
#[test]
fn test_atomic_save_round_trip_via_session() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("rpc-cache.json");

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "16",
        "--rpc.cache-file",
        cache_path.to_str().unwrap(),
    ]);

    let key = B256::repeat_byte(0xAB);
    let value = r#"{"seeded":"value"}"#.to_string();

    let (_provider, finalizer) = args.build_session().expect("build_session").into_parts();
    finalizer.cache().expect("real finalizer").put(key, value.clone()).expect("seed put");
    finalizer.finalize();

    assert!(cache_path.exists(), "save must produce the target file");

    // Atomic save uses a temp file in the same parent dir; nothing should remain.
    let stale_temps: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".mega-evme-tmp."))
        .collect();
    assert!(stale_temps.is_empty(), "no temp files left after a successful save");

    let (_provider, reloaded) = args.build_session().expect("build_session").into_parts();
    let got = reloaded.cache().expect("real finalizer").get(&key).expect("entry must reload");
    assert_eq!(got, value);
}

/// `finalize` must warn-and-swallow on I/O failure, never propagate.
#[test]
fn test_finalize_with_missing_parent_dir() {
    let dir = tempdir().expect("tempdir");
    let missing_parent = dir.path().join("does/not/exist");
    let cache_path = missing_parent.join("cache.json");

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "16",
        "--rpc.cache-file",
        cache_path.to_str().unwrap(),
    ]);
    let (_provider, finalizer) = args.build_session().expect("build_session").into_parts();
    finalizer
        .cache()
        .expect("real finalizer")
        .put(B256::repeat_byte(0x01), "{}".to_string())
        .expect("seed put");

    finalizer.finalize();

    assert!(!missing_parent.exists(), "finalize must not auto-create the parent dir");
    assert!(!cache_path.exists(), "target file must not exist after a failed save");

    let leaked: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".mega-evme-tmp."))
        .collect();
    assert!(leaked.is_empty(), "no leaked temp files in the controlled tempdir root");
}

#[test]
fn test_build_session_load_from_missing_file() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("does-not-exist.json");
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "16",
        "--rpc.cache-file",
        cache_path.to_str().unwrap(),
    ]);
    let (_provider, finalizer) = args.build_session().expect("build_session").into_parts();
    assert!(finalizer.cache().expect("real finalizer").get(&B256::ZERO).is_none());
}

#[test]
fn test_build_session_load_from_corrupt_file() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("corrupt.json");
    std::fs::write(&cache_path, b"not json").expect("write corrupt file");

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "16",
        "--rpc.cache-file",
        cache_path.to_str().unwrap(),
    ]);
    let (_provider, finalizer) = args.build_session().expect("build_session").into_parts();
    // Corrupt content is discarded; cache is empty but usable.
    assert!(finalizer.cache().expect("real finalizer").get(&B256::ZERO).is_none());
}

// ─── Owner API: into_parts ───────────────────────────────────────────────────

/// The detached finalizer remains usable after splitting away from the session.
#[test]
fn test_rpc_session_into_parts() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("split-cache.json");
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "http://localhost:8545",
        "--rpc.cache-size",
        "16",
        "--rpc.cache-file",
        cache_path.to_str().unwrap(),
    ]);
    let (_provider, finalizer) = args.build_session().expect("build_session").into_parts();
    finalizer
        .cache()
        .expect("real finalizer")
        .put(B256::repeat_byte(0x42), r#"{"split":true}"#.to_string())
        .expect("seed put");
    finalizer.finalize();
    assert!(cache_path.exists(), "detached finalizer should still write the cache");
}

// ─── Retry layer behavior tests ──────────────────────────────────────────────

/// Server fails 503 twice then succeeds; `max-retries=3` must reach success.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_retries_on_503_then_succeeds() {
    let server = MockRpcServer::start().await;
    server.respond_status_n_times(503, 2, 1).await;
    server.respond_jsonrpc_result("0xa", 2).await;

    let session = test_rpc_args(&server.uri(), Some(3)).build_session().expect("build_session");
    let chain_id = session.provider().get_chain_id().await.expect("must succeed after 2 retries");
    assert_eq!(chain_id, 10);

    assert_eq!(server.received_request_count().await, 3, "1 initial + 2 retries before success");
}

/// `max-retries=0` must surface the first 503 immediately, with no retries.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_max_retries_zero_fails_fast() {
    let server = MockRpcServer::start().await;
    server.respond_status_always(503).await;

    let session = test_rpc_args(&server.uri(), Some(0)).build_session().expect("build_session");
    let result = session.provider().get_chain_id().await;
    assert!(result.is_err(), "max-retries=0 must surface the 503, not retry it");

    assert_eq!(server.received_request_count().await, 1, "no retries");
}

/// Pins the runtime behavior of the production default `--rpc.max-retries`:
/// a permanent 503 must give up after exactly `1 + default_max_retries` attempts.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_uses_default_max_retries() {
    let server = MockRpcServer::start().await;
    server.respond_status_always(503).await;

    // Pass `None` to keep the production default.
    let args = test_rpc_args(&server.uri(), None);
    assert_eq!(args.max_retries, 5, "guard against the default drifting underneath this test");

    let session = args.build_session().expect("build_session");
    let result = session.provider().get_chain_id().await;
    assert!(result.is_err(), "all attempts return 503");

    assert_eq!(
        server.received_request_count().await,
        6,
        "default max-retries=5 → 1 initial + 5 retries"
    );
}

/// `max-retries=2` against a permanent 503: must give up after exactly 3 attempts.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_max_retries_exhausted() {
    let server = MockRpcServer::start().await;
    server.respond_status_always(503).await;

    let session = test_rpc_args(&server.uri(), Some(2)).build_session().expect("build_session");
    let result = session.provider().get_chain_id().await;
    assert!(result.is_err(), "all attempts return 503");

    assert_eq!(server.received_request_count().await, 3, "1 initial + 2 retries");
}

/// HTTP 500 arrives as `HttpError` and is not in either the default or the
/// extended retryable set; even a generous retry budget must produce exactly
/// one attempt.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_does_not_retry_non_retryable_status() {
    let server = MockRpcServer::start().await;
    server.respond_status_always(500).await;

    let session = test_rpc_args(&server.uri(), Some(5)).build_session().expect("build_session");
    let result = session.provider().get_chain_id().await;
    assert!(result.is_err(), "500 must surface as an error");

    assert_eq!(
        server.received_request_count().await,
        1,
        "500 is not retryable, so exactly 1 attempt"
    );
}

/// Transport-level errors (connection refused, DNS failure, TLS handshake)
/// arrive as `TransportErrorKind::Custom`, which the extended policy treats
/// as retryable. We probe by binding a TCP listener and immediately dropping
/// it so the port is definitely closed, then assert the request fails with
/// the wrapped "Max retries exceeded" error that only appears when retry was
/// actually attempted.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_retries_on_unreachable_endpoint() {
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        port
    };
    let unreachable_url = format!("http://127.0.0.1:{}", closed_port);

    let session = test_rpc_args(&unreachable_url, Some(3)).build_session().expect("build_session");
    let result = session.provider().get_chain_id().await;
    let err = result.expect_err("unreachable endpoint must surface as an error");
    let err_str = err.to_string();

    // alloy prepends "Max retries exceeded" only when the policy judged the
    // error retryable and the counter was exhausted — so the prefix's presence
    // is proof that retry was attempted.
    assert!(
        err_str.contains("Max retries exceeded"),
        "expected retry to be attempted, but got raw error: {err_str}"
    );
}
