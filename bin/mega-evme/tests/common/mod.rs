//! Shared helpers for the integration tests in `tests/*.rs`. Each test file
//! that wants these helpers declares `mod common;` at its top. cargo treats
//! `tests/common/` as a directory (not a top-level test binary), so this file
//! is not picked up as a standalone test target.
//!
//! Add helpers only when a third caller shows up; the current set is sized to
//! the patterns shared between `tests/provider.rs` and `tests/state.rs`.

#![allow(dead_code)] // Each test binary uses a different subset of helpers.

use clap::Parser;
use mega_evme::common::RpcArgs;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

/// A mock JSON-RPC server tuned for mega-evme integration tests.
///
/// All mounted mocks match `POST` (the JSON-RPC verb) and use priorities so
/// "fail N times then succeed" sequences compose correctly.
pub(crate) struct MockRpcServer {
    server: MockServer,
}

impl MockRpcServer {
    /// Start a fresh mock server bound to a random local port.
    pub(crate) async fn start() -> Self {
        Self { server: MockServer::start().await }
    }

    /// HTTP URL the mock is listening on.
    pub(crate) fn uri(&self) -> String {
        self.server.uri()
    }

    /// Mount a mock that responds with `status` for the next `n` POSTs at
    /// `priority` (lower number = higher priority, checked first).
    pub(crate) async fn respond_status_n_times(&self, status: u16, n: u64, priority: u8) {
        Mock::given(matchers::method("POST"))
            .respond_with(ResponseTemplate::new(status))
            .up_to_n_times(n)
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// Mount an unbounded mock that always responds with `status`.
    pub(crate) async fn respond_status_always(&self, status: u16) {
        Mock::given(matchers::method("POST"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&self.server)
            .await;
    }

    /// Mount an unbounded mock that always returns a successful JSON-RPC
    /// response with the given hex `result`. Use a higher `priority` number
    /// (= lower precedence) than companion failure mocks so this serves as
    /// the fallthrough.
    pub(crate) async fn respond_jsonrpc_result(&self, hex_result: &str, priority: u8) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": hex_result,
        });
        Mock::given(matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// Mount a mock that returns `eth_chainId` with the given chain id.
    pub(crate) async fn respond_eth_chain_id(&self, chain_id: u64, priority: u8) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": format!("0x{:x}", chain_id),
        });
        Mock::given(matchers::method("POST"))
            .and(matchers::body_partial_json(serde_json::json!({
                "method": "eth_chainId"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// Total number of POSTs the server has received since it started.
    pub(crate) async fn received_request_count(&self) -> usize {
        self.server.received_requests().await.expect("received_requests").len()
    }
}

/// Build [`RpcArgs`] for a test pointed at `url` with the on-disk cache disabled.
///
/// Defaults: `--rpc.cache-size 0` (no cache layer, no disk persistence),
/// 1ms backoff, production rate limit. `build_provider` still calls
/// `eth_chainId`, so the caller must mount a mock for it. Pass `Some(n)` to
/// override `--rpc.max-retries`; `None` keeps the production default.
pub(crate) fn test_rpc_args(url: &str, max_retries: Option<u32>) -> RpcArgs {
    let mut argv: Vec<String> = vec![
        "mega-evme".into(),
        "--rpc".into(),
        url.into(),
        "--rpc.cache-size".into(),
        "0".into(),
        "--rpc.backoff-ms".into(),
        "1".into(),
        "--rpc.rate-limit".into(),
        "660".into(),
    ];
    if let Some(n) = max_retries {
        argv.push("--rpc.max-retries".into());
        argv.push(n.to_string());
    }
    RpcArgs::parse_from(argv)
}

/// Build [`RpcArgs`] for a test that exercises the on-disk cache path.
///
/// Sets `--rpc.cache-size 256` and an explicit `--rpc.cache-dir`. The caller
/// must mount a mock `eth_chainId` response on the server so that
/// `build_provider`'s `resolve_chain_id` call succeeds — use
/// [`MockRpcServer::respond_eth_chain_id`] for this.
pub(crate) fn test_rpc_args_cached(
    url: &str,
    cache_dir: &std::path::Path,
    max_retries: Option<u32>,
) -> RpcArgs {
    let mut argv: Vec<String> = vec![
        "mega-evme".into(),
        "--rpc".into(),
        url.into(),
        "--rpc.cache-size".into(),
        "256".into(),
        "--rpc.cache-dir".into(),
        cache_dir.to_str().expect("cache_dir utf-8").to_string(),
        "--rpc.backoff-ms".into(),
        "1".into(),
        "--rpc.rate-limit".into(),
        "660".into(),
    ];
    if let Some(n) = max_retries {
        argv.push("--rpc.max-retries".into());
        argv.push(n.to_string());
    }
    RpcArgs::parse_from(argv)
}
