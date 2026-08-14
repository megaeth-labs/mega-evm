//! Shared helpers for the integration tests in `tests/*.rs`. Each test file
//! that wants these helpers declares `mod common;` at its top. cargo treats
//! `tests/common/` as a directory (not a top-level test binary), so this file
//! is not picked up as a standalone test target.
//!
//! Capture-envelope doctoring lives in [`doctor`]. Other helpers here are the
//! patterns shared across the provider, state, and replay test binaries.

#![allow(dead_code)] // Each test binary uses a different subset of helpers.

pub(crate) mod doctor;

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, OnceLock},
};

use clap::Parser;
use mega_evme::common::RpcArgs;
use tempfile::TempDir;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

/// Resolve a fixture in `tests/fixtures/` by name, extracting it if it is
/// stored compressed.
///
/// A fixture is either the file itself, or a `<name>.tar.gz` holding exactly
/// that one file. Compression is worth it only where the raw file would bloat a
/// pull-request diff — git already compresses blobs, so it buys little on its
/// own, and a compressed blob cannot delta against its previous revision.
///
/// Archives are extracted once per test binary into a temporary directory that
/// lives for the whole run. Extraction shells out to `tar` rather than linking a
/// decompressor: every platform that runs these tests has one, and this is the
/// only place that reads an archive.
pub(crate) fn fixture(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let plain = dir.join(name);
    if plain.is_file() {
        return plain;
    }

    let archive = dir.join(format!("{name}.tar.gz"));
    assert!(
        archive.is_file(),
        "no fixture named {name}: neither {} nor {} exists",
        plain.display(),
        archive.display(),
    );

    static ROOT: OnceLock<TempDir> = OnceLock::new();
    static EXTRACTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let root = ROOT.get_or_init(|| {
        tempfile::tempdir().expect("failed to create a temp dir for extracted fixtures")
    });
    let mut extracted =
        EXTRACTED.get_or_init(|| Mutex::new(HashSet::new())).lock().expect("fixture lock");

    let path = root.path().join(name);
    if extracted.insert(name.to_string()) {
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(root.path())
            .status()
            .expect("failed to run tar");
        assert!(status.success(), "failed to extract {}", archive.display());
        assert!(path.is_file(), "{} does not contain {name}", archive.display());
    }
    path
}

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

    /// Mount an unbounded mock that always returns a JSON-RPC error body (HTTP
    /// 200 + `"error": {...}`). Models transient endpoint failures that surface
    /// through the JSON-RPC envelope rather than an HTTP status, e.g.
    /// `-32000 rate limit`.
    pub(crate) async fn respond_jsonrpc_error(&self, code: i32, message: &str, priority: u8) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "error": { "code": code, "message": message },
        });
        Mock::given(matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// Mount an unbounded mock that always returns a successful JSON-RPC body
    /// with `"result": null` (HTTP 200). Models a transient not-found answer
    /// (e.g. `eth_getTransactionByHash` for a briefly-invisible transaction)
    /// that capture must not bake into the fixture.
    pub(crate) async fn respond_jsonrpc_null_result(&self, priority: u8) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": null,
        });
        Mock::given(matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// Mount an unbounded mock that answers every JSON-RPC request for
    /// `method` with the given hex `result`, regardless of params.
    pub(crate) async fn respond_method_result(&self, method: &str, hex_result: &str, priority: u8) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": hex_result,
        });
        Mock::given(matchers::method("POST"))
            .and(matchers::body_partial_json(serde_json::json!({ "method": method })))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// Mount an unbounded mock that answers every JSON-RPC request for `method`
    /// with the given JSON `result`, regardless of params.
    ///
    /// The result is any JSON value, so this serves structured answers (blocks,
    /// transactions) that [`Self::respond_method_result`]'s hex string cannot.
    pub(crate) async fn respond_method_json(
        &self,
        method: &str,
        result: serde_json::Value,
        priority: u8,
    ) {
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 0, "result": result });
        Mock::given(matchers::method("POST"))
            .and(matchers::body_partial_json(serde_json::json!({ "method": method })))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// Mount an unbounded mock that answers `method` calls whose params match
    /// `params` with the given JSON `result`.
    ///
    /// Needed where one method is called with different arguments in the same
    /// run and the answers must differ — `eth_getBlockByNumber` for a block and
    /// its parent, for instance.
    pub(crate) async fn respond_method_params_json(
        &self,
        method: &str,
        params: serde_json::Value,
        result: serde_json::Value,
        priority: u8,
    ) {
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 0, "result": result });
        Mock::given(matchers::method("POST"))
            .and(matchers::body_partial_json(
                serde_json::json!({ "method": method, "params": params }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// Mount a mock that answers the first `n` calls of `method` with matching
    /// `params` and then stops matching, so a lower-priority mock for the same
    /// request serves every later call.
    ///
    /// Models an endpoint whose answer to one repeated request changes
    /// mid-run — a reorg landing between two calls, or a load balancer moving
    /// the run to another backend.
    pub(crate) async fn respond_method_params_json_n_times(
        &self,
        method: &str,
        params: serde_json::Value,
        result: serde_json::Value,
        n: u64,
        priority: u8,
    ) {
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 0, "result": result });
        Mock::given(matchers::method("POST"))
            .and(matchers::body_partial_json(
                serde_json::json!({ "method": method, "params": params }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .up_to_n_times(n)
            .with_priority(priority)
            .mount(&self.server)
            .await;
    }

    /// How many single JSON-RPC requests for `method` the server has received.
    ///
    /// Batched requests (a JSON array body) are not counted: every call these
    /// tests make is a single request.
    pub(crate) async fn received_method_count(&self, method: &str) -> usize {
        let requests = self.server.received_requests().await.expect("received_requests");
        requests
            .iter()
            .filter(|request| {
                serde_json::from_slice::<serde_json::Value>(&request.body)
                    .ok()
                    .and_then(|body| {
                        body.get("method").and_then(serde_json::Value::as_str).map(str::to_string)
                    })
                    .as_deref() ==
                    Some(method)
            })
            .count()
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

    /// Accept every POST and delay the response far beyond any test timeout.
    ///
    /// Models a black-hole endpoint that accepts the TCP connection (and the
    /// HTTP request) but never answers in time — the failure mode that
    /// `--rpc.request-timeout` is meant to bound. The delay is 5 minutes so a
    /// client with a 1s timeout fires first while the mock still records the hit.
    pub(crate) async fn respond_black_hole(&self) {
        use std::time::Duration;

        Mock::given(matchers::method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(300)))
            .mount(&self.server)
            .await;
    }
}

/// Parse every top-level JSON value a run printed on stdout.
///
/// Streaming parse, so it covers both the pretty-printed single-transaction
/// summary and the compact NDJSON of a batch run — in either case followed by
/// the structured error object a failing `--json` run ends with.
pub(crate) fn json_values(stdout: &str) -> Vec<serde_json::Value> {
    serde_json::Deserializer::from_str(stdout)
        .into_iter::<serde_json::Value>()
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("stdout is not a JSON stream ({e}):\n{stdout}"))
}

/// Whether a printed value is the run-level error object of a failing `--json`
/// run (`{"error":{"code":…,"kind":…,"message":…}}`).
///
/// A per-target NDJSON error line carries its transaction hash alongside the
/// `error` key, so the single-key shape identifies the run-level object.
pub(crate) fn is_run_error(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|obj| obj.len() == 1 && obj.contains_key("error"))
}

/// Build [`RpcArgs`] for a test pointed at `url` with the on-disk cache disabled.
///
/// Defaults: `--rpc.no-cache-file` (in-memory LRU still applies; no disk persistence),
/// 1ms backoff, production rate limit. `build_provider` still calls
/// `eth_chainId`, so the caller must mount a mock for it. Pass `Some(n)` to
/// override `--rpc.max-retries`; `None` keeps the production default.
pub(crate) fn test_rpc_args(url: &str, max_retries: Option<u32>) -> RpcArgs {
    let mut argv: Vec<String> = vec![
        "mega-evme".into(),
        "--rpc".into(),
        url.into(),
        "--rpc.no-cache-file".into(),
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
/// Sets `--rpc.cache-max-entries 256` and an explicit `--rpc.cache-dir`. The caller
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
        "--rpc.cache-max-entries".into(),
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
