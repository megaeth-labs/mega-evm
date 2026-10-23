//! Shared helpers for the integration tests in `tests/*.rs`. Each test file
//! that wants these helpers declares `mod common;` at its top. cargo treats
//! `tests/common/` as a directory (not a top-level test binary), so this file
//! is not picked up as a standalone test target.
//!
//! Add helpers only when a third caller shows up; the current set is sized to
//! the patterns shared between `tests/provider.rs` and `tests/state.rs`, and
//! (`blocks`) between the tests that replay the recorded blocks.

#![allow(dead_code)] // Each test binary uses a different subset of helpers.

pub(crate) mod blocks;

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use clap::Parser;
use mega_evme::common::RpcArgs;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

/// Resolve an archive in `tests/fixtures/` holding many fixtures, extracting
/// all of its members, and return the directory they were extracted into.
///
/// `name` names the archive itself (`blocks.tar.xz`). Each member is then the
/// file of that relative path in the returned directory. The directory is
/// returned only once every member is in place; see [`install_archive`] for why
/// an extraction is safe to share across runs and concurrent test binaries.
pub(crate) fn fixture_archive(name: &str) -> PathBuf {
    let archive = fixtures_dir().join(name);
    assert!(
        archive.is_file(),
        "no fixture archive named {name}: {} does not exist",
        archive.display()
    );
    extracted(&archive)
}

/// The directory `archive` is extracted into, extracting it first unless an
/// earlier extraction already completed there.
fn extracted(archive: &Path) -> PathBuf {
    let destination = extraction_dir(archive);
    if !destination.join(EXTRACTION_COMPLETE).is_file() {
        install_archive(archive, &destination);
    }
    destination
}

/// Marker [`install_archive`] writes into an extraction directory once every
/// member of the archive is in place.
const EXTRACTION_COMPLETE: &str = ".extraction-complete";

/// Directory an archive is extracted into: under [`extracted_root`], named by
/// the keccak-256 of the archive's bytes.
fn extraction_dir(archive: &Path) -> PathBuf {
    let bytes =
        std::fs::read(archive).unwrap_or_else(|e| panic!("cannot read {}: {e}", archive.display()));
    extracted_root().join(format!("{:x}", alloy_primitives::keccak256(&bytes)))
}

/// Extract every member of `archive` into `destination`, its
/// [`extraction_dir`], and mark the directory complete.
///
/// Naming the directory by the archive's digest means:
///
/// - an extraction is reused by every later test binary and every later run for as long as its
///   archive is unchanged, so the directory does not grow from run to run;
/// - a changed archive (a recapture, or a branch switch) has a different digest, so an extraction
///   of an earlier version is never served for it; that earlier extraction stays until `cargo
///   clean`;
/// - concurrent extractions — parallel tests, or test binaries running at once — each unpack into a
///   private scratch directory and rename every file into place, which is atomic, so a reader sees
///   either no file or a complete one, and two writers write the same bytes. The completion marker
///   is renamed into place last, so a directory carrying it holds every member.
///
/// Extraction shells out to `tar -xJf` rather than linking a decompressor:
/// every platform that runs these tests has one, and this is the only place
/// that reads an archive.
pub(crate) fn install_archive(archive: &Path, destination: &Path) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let scratch = extracted_root().join(format!(
        ".extracting-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&scratch).expect("failed to create the extraction directory");
    let status = Command::new("tar")
        .arg("-xJf")
        .arg(archive)
        .arg("-C")
        .arg(&scratch)
        .status()
        .expect("failed to run tar");
    assert!(status.success(), "failed to extract {}", archive.display());

    move_files(&scratch, destination);
    let marker = scratch.join(EXTRACTION_COMPLETE);
    std::fs::write(&marker, b"").expect("failed to write the extraction marker");
    std::fs::rename(&marker, destination.join(EXTRACTION_COMPLETE))
        .expect("failed to mark the extraction complete");
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Move every file under `from` to the same path under `to`, creating the directories on the way.
///
/// Each file is renamed on its own, which is atomic and replaces a file of the same name, so a
/// second writer moving the same members succeeds where renaming a directory onto one already in
/// place would fail.
fn move_files(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("failed to create the extraction directory");
    for entry in std::fs::read_dir(from).expect("failed to read the extraction directory") {
        let entry = entry.expect("failed to read an extracted member");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("failed to read an extracted member's type").is_dir() {
            move_files(&entry.path(), &target);
        } else {
            std::fs::rename(entry.path(), &target)
                .expect("failed to move an extracted fixture into place");
        }
    }
}

/// Directory the committed fixtures live in.
pub(crate) fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Directory compressed fixtures are extracted into.
///
/// It lives under cargo's per-target scratch directory (`CARGO_TARGET_TMPDIR`),
/// which persists across test runs and is removed by `cargo clean`. Exposed so a
/// test that normalizes the paths a run prints can name it.
pub(crate) fn extracted_root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("mega-evme-fixtures")
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

    /// Drops every mounted mock and the recorded requests.
    pub(crate) async fn reset(&self) {
        self.server.reset().await;
    }

    /// Total number of POSTs the server has received since it started.
    pub(crate) async fn received_request_count(&self) -> usize {
        self.server.received_requests().await.expect("received_requests").len()
    }
}

/// The on-disk cache file `build_provider` uses for `url` on `chain_id` under `dir`.
pub(crate) fn rpc_cache_path(
    dir: &std::path::Path,
    chain_id: u64,
    url: &str,
) -> std::path::PathBuf {
    dir.join(mega_evme::rpc_cache_file_name(chain_id, url))
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
