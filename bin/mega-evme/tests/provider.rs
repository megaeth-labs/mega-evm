//! Integration tests for the public RPC provider API, cache store, and
//! retry layer.
//!
//! Covers: CLI parsing of all `--rpc.*` flags, `build_provider` shape across
//! cache configurations, on-disk cache round-trip via the test-utils
//! `put_cache_entry` / `cache_entry` accessors, what `build_provider` does with
//! each shape of file it can find at the cache path, chain-id resolution (RPC
//! fetch vs. failure), `--rpc.clear-cache` behaviour, and the retry policy on
//! both branches of its coverage (HTTP 429/503 via wiremock; transport failures
//! via a closed local port). Tests for the private helpers `temp_path_for` and
//! `resolve_cache_path` stay inline in `src/common/provider.rs`.

use std::path::{Path, PathBuf};

use alloy_primitives::B256;
use alloy_provider::Provider;
use clap::Parser;
use mega_evme::common::{BuildProviderOutput, EvmeError, ExitCode, RpcArgs};
use tempfile::tempdir;

mod common;
use common::{test_rpc_args, test_rpc_args_cached, MockRpcServer};

// ─── CLI parsing ─────────────────────────────────────────────────────────────

#[test]
fn test_rpc_args_parses_all_new_flags() {
    // Keep `--rpc.rate-limit` here so the visible alias stays pin-tested.
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "https://example.test/rpc",
        "--rpc.cache-max-entries",
        "256",
        "--rpc.cache-dir",
        "/tmp/example-cache",
        "--rpc.no-cache-file",
        "--rpc.clear-cache",
        "--rpc.max-retries",
        "7",
        "--rpc.backoff-ms",
        "250",
        "--rpc.rate-limit",
        "1234",
        "--rpc.request-timeout",
        "45",
    ]);
    assert_eq!(args.rpc_url, Some("https://example.test/rpc".to_string()));
    assert_eq!(args.cache_max_entries, 256);
    assert_eq!(args.cache_dir, Some(PathBuf::from("/tmp/example-cache")));
    assert!(args.no_cache_file);
    assert!(args.clear_cache);
    assert_eq!(args.max_retries, 7);
    assert_eq!(args.backoff_ms, 250);
    assert_eq!(args.compute_units_per_sec, 1234);
    assert_eq!(args.request_timeout, 45);
}

/// Explicit `--rpc.request-timeout` values parse, including the disable sentinel `0`.
#[test]
fn test_rpc_args_parses_request_timeout() {
    let defaulted = RpcArgs::parse_from(["mega-evme"]);
    assert_eq!(defaulted.request_timeout, 30, "default request timeout is 30s");

    let explicit = RpcArgs::parse_from(["mega-evme", "--rpc.request-timeout", "12"]);
    assert_eq!(explicit.request_timeout, 12);

    let disabled = RpcArgs::parse_from(["mega-evme", "--rpc.request-timeout", "0"]);
    assert_eq!(disabled.request_timeout, 0, "0 disables the per-request timeout");
}

/// The removed `--rpc.cache-size` flag must fail to parse (pin the deletion).
#[test]
fn test_rpc_args_rejects_removed_cache_size_flag() {
    let err = RpcArgs::try_parse_from([
        "mega-evme",
        "--rpc",
        "https://example.test/rpc",
        "--rpc.cache-size",
        "100",
    ])
    .expect_err("removed --rpc.cache-size must not parse");
    let msg = err.to_string();
    assert!(
        msg.contains("unexpected argument") ||
            msg.contains("unknown") ||
            msg.contains("cache-size"),
        "error must reject the removed flag, got: {msg}",
    );
}

/// Canonical flag name `--rpc.cu-per-sec` parses into the same field.
#[test]
fn test_rpc_args_parses_cu_per_sec_flag() {
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        "https://example.test/rpc",
        "--rpc.cu-per-sec",
        "1234",
    ]);
    assert_eq!(args.compute_units_per_sec, 1234);
}

/// `--rpc.cache-dir ""` (and whitespace-only) must be rejected at parse
/// time. The alternative — silently landing in `PathBuf::from("")` and
/// writing the cache to CWD — is a footgun since the same command run
/// from different directories would produce different files.
#[test]
fn test_rpc_args_rejects_empty_cache_dir() {
    for empty in ["", "   ", "\t"] {
        let err = RpcArgs::try_parse_from([
            "mega-evme",
            "--rpc",
            "http://localhost:8545",
            "--rpc.cache-dir",
            empty,
        ])
        .expect_err("empty cache-dir must be rejected");
        assert!(
            err.to_string().contains("path must not be empty"),
            "error must explain the problem for input {empty:?}, got: {err}",
        );
    }
}

/// Pin the default values so that any future change to a `default_value_t` in
/// `RpcArgs` shows up as a clean test failure rather than as silent behavior
/// drift in the retry / cache layers.
#[test]
fn test_rpc_args_default_values() {
    let args = RpcArgs::parse_from(["mega-evme"]);
    assert_eq!(args.rpc_url, None);
    assert_eq!(args.cache_max_entries, 0, "default is unlimited (never evict)");
    assert_eq!(args.cache_dir, None);
    assert!(!args.no_cache_file);
    assert!(!args.clear_cache);
    assert_eq!(args.max_retries, 5);
    assert_eq!(args.backoff_ms, 1_000);
    assert_eq!(args.compute_units_per_sec, 660);
    assert_eq!(args.request_timeout, 30);
}

// ─── build_provider shape variants ───────────────────────────────────────────

/// Default `--rpc.cache-max-entries 0` (unlimited) still resolves `chain_id`
/// and installs the in-memory cache layer; with `--rpc.no-cache-file` the
/// disk store is a no-op.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_default_unlimited_with_no_cache_file() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    let args = RpcArgs::parse_from(["mega-evme", "--rpc", &server.uri(), "--rpc.no-cache-file"]);
    let BuildProviderOutput { cache_store, chain_id, .. } =
        args.build_provider().await.expect("build_provider");
    assert!(cache_store.is_noop(), "--rpc.no-cache-file must produce a no-op store");
    assert_eq!(chain_id, 4326, "chain_id must be resolved with unlimited cache default");
    cache_store.persist().expect("persist");
}

/// `--rpc.no-cache-file` keeps the in-memory cache layer active but skips
/// on-disk persistence.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_no_cache_file_skips_persistence() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.cache-max-entries",
        "100",
        "--rpc.no-cache-file",
    ]);
    let BuildProviderOutput { cache_store, .. } =
        args.build_provider().await.expect("build_provider");
    assert!(cache_store.is_noop(), "--rpc.no-cache-file must produce a no-op store");
    cache_store.persist().expect("persist");
}

/// With the on-disk cache enabled, `build_provider` fetches the chain id from
/// the RPC endpoint and names the cache file after it.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_with_cache_names_file_from_fetched_chain_id() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;

    let dir = tempdir().expect("tempdir");
    let args = test_rpc_args_cached(&server.uri(), dir.path(), None);

    let BuildProviderOutput { cache_store, .. } =
        args.build_provider().await.expect("build_provider");
    assert!(
        !cache_store.is_noop(),
        "cache_dir without --rpc.no-cache-file must produce a real store"
    );
    assert_eq!(cache_store.cache_path(), Some(dir.path().join("rpc-cache-4326.json").as_path()));
}

/// Malformed `--rpc` is bad input (exit 1), not an RPC transport failure (exit 3).
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_invalid_url() {
    let args = RpcArgs::parse_from(["mega-evme", "--rpc", "not a url", "--rpc.no-cache-file"]);
    let err = args.build_provider().await.expect_err("build_provider should fail");
    match &err {
        EvmeError::InvalidInput(msg) => {
            assert!(msg.contains("not a url"), "error must echo the original input, got: {msg}");
            assert!(msg.contains("Invalid RPC URL"), "msg={msg}");
        }
        other => panic!("expected EvmeError::InvalidInput, got {other:?}"),
    }
    // Exit-code taxonomy: InvalidInput → execution-error (code 1), never rpc-failure (3).
    assert_eq!(
        ExitCode::from_evme_error(&err).code(),
        1,
        "malformed --rpc must exit 1 (bad input), not 3 (rpc-failure)",
    );
}

// ─── Chain-id resolution ─────────────────────────────────────────────────────

/// `build_provider` must call `eth_chainId` against the endpoint, name the
/// cache file after the returned value, and propagate the same id via the
/// hint so downstream code (e.g. `replay`'s hardfork selection) does not
/// re-fetch.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_fetches_chain_id_from_rpc() {
    let server = MockRpcServer::start().await;
    // 0x10e6 = 4326 = MegaETH mainnet.
    server.respond_jsonrpc_result("0x10e6", 1).await;

    let dir = tempdir().expect("tempdir");
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.cache-max-entries",
        "256",
        "--rpc.cache-dir",
        dir.path().to_str().unwrap(),
    ]);

    let BuildProviderOutput { cache_store, chain_id, .. } =
        args.build_provider().await.expect("build_provider");
    assert_eq!(
        cache_store.cache_path(),
        Some(dir.path().join("rpc-cache-4326.json").as_path()),
        "cache file must be named after the fetched chain id",
    );
    assert_eq!(chain_id, 4326, "chain_id must match the fetched value");
    assert!(
        server.received_request_count().await >= 1,
        "build_provider must reach the mock to fetch eth_chainId",
    );
}

/// The `eth_chainId` call goes through the retry layer. A permanent 500
/// (non-retryable) must surface as an `EvmeError::RpcError` hard error.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_chain_id_rpc_failure_is_hard_error() {
    let server = MockRpcServer::start().await;
    server.respond_status_always(500).await;

    let dir = tempdir().expect("tempdir");
    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.cache-max-entries",
        "256",
        "--rpc.cache-dir",
        dir.path().to_str().unwrap(),
        "--rpc.max-retries",
        "0",
    ]);

    let err = args.build_provider().await.expect_err("build_provider should hard-error");
    match err {
        EvmeError::RpcError(msg) => {
            assert!(
                msg.contains("Failed to fetch chain ID"),
                "error must name the chain-id fetch, got: {msg}",
            );
        }
        other => panic!("expected EvmeError::RpcError, got {other:?}"),
    }
}

// ─── Cache file round-trip ───────────────────────────────────────────────────

/// Read the JSON at `path`, failing with the raw bytes when it is not JSON.
fn read_json(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path).expect("read cache file");
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("cache file is not JSON ({e}):\n{raw}"))
}

/// Seed an entry through the cache, persist, build a fresh session against
/// the same directory + chain id, and confirm the entry comes back via the
/// load path. The written file is the cache envelope, stamped with the `kind`
/// that tells it apart from a capture fixture.
#[tokio::test(flavor = "multi_thread")]
async fn test_atomic_save_round_trip() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;

    let dir = tempdir().expect("tempdir");
    let args = test_rpc_args_cached(&server.uri(), dir.path(), None);

    let key = B256::repeat_byte(0xAB);
    let value = r#"{"seeded":"value"}"#.to_string();

    let BuildProviderOutput { cache_store, .. } =
        args.build_provider().await.expect("build_provider #1");
    assert!(cache_store.put_cache_entry(key, value.clone()), "seed put");
    cache_store.persist().expect("persist");

    let cache_file = dir.path().join("rpc-cache-4326.json");
    assert!(cache_file.exists(), "save must produce the target file");

    let written = read_json(&cache_file);
    assert_eq!(written["version"], serde_json::json!(1));
    assert_eq!(written["kind"], serde_json::json!("cache"), "written = {written}");
    assert_eq!(written["chain_id"], serde_json::json!(4326));
    assert!(
        written["external_env"].is_null(),
        "the online cache never writes a snapshot: {written}",
    );

    // Atomic save uses a temp file in the same parent dir; nothing should remain.
    let stale_temps: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".mega-evme-tmp."))
        .collect();
    assert!(stale_temps.is_empty(), "no temp files left after a successful save");

    let BuildProviderOutput { cache_store: reloaded, .. } =
        args.build_provider().await.expect("build_provider #2");
    let got = reloaded.cache_entry(&key).expect("entry must reload");
    assert_eq!(got, value);
}

/// Missing or corrupt cache file: `build_provider` starts with an empty cache
/// rather than failing.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_tolerates_missing_or_corrupt_cache_file() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(42, 1).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("rpc-cache-42.json");
    let args = test_rpc_args_cached(&server.uri(), dir.path(), None);

    // Missing file — load skipped.
    let BuildProviderOutput { cache_store, .. } =
        args.build_provider().await.expect("missing file");
    assert!(!cache_store.is_noop());
    assert!(cache_store.cache_entry(&B256::ZERO).is_none());

    // Corrupt file — unusable, so it is replaced and the cache starts empty.
    std::fs::write(&cache_file, b"not json").expect("write corrupt");
    let BuildProviderOutput { cache_store, .. } =
        args.build_provider().await.expect("corrupt file");
    assert!(cache_store.cache_entry(&B256::ZERO).is_none());
    assert!(!cache_file.exists(), "an unusable cache file is removed, not carried forward");
}

/// A cache file written by a retired build (the bare `[{key, value}]` array) is
/// replaced rather than reported: its entries cannot be converted — they are
/// hashed request keys this build hashes differently — and the cache is a
/// regenerable artifact, so failing every first run after an upgrade would be a
/// worse answer than starting fresh.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_replaces_a_retired_cache_file_format() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("rpc-cache-4326.json");
    let legacy = serde_json::json!([{ "key": B256::repeat_byte(0x11), "value": "legacy" }]);
    std::fs::write(&cache_file, serde_json::to_string(&legacy).unwrap()).expect("seed legacy");

    let args = test_rpc_args_cached(&server.uri(), dir.path(), None);
    let BuildProviderOutput { cache_store, .. } =
        args.build_provider().await.expect("a retired format must not fail the run");
    assert!(
        cache_store.cache_entry(&B256::repeat_byte(0x11)).is_none(),
        "no entry of the retired file may be adopted",
    );
    assert!(!cache_file.exists(), "the retired file is removed at load");

    // The run persists into the freed path, in the format this build reads.
    assert!(cache_store.put_cache_entry(B256::repeat_byte(0x22), "fresh".to_string()));
    cache_store.persist().expect("persist");
    assert_eq!(read_json(&cache_file)["kind"], serde_json::json!("cache"));
}

/// A capture fixture that happens to sit at the per-chain cache path is not
/// ours: it is never adopted (its entries answer the run it was recorded for,
/// including reads that are only true for that moment) and never rewritten (the
/// online cap would silently truncate it).
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_leaves_a_capture_fixture_at_the_cache_path_intact() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("rpc-cache-4326.json");
    let fixture = serde_json::json!({
        "version": 1,
        "chain_id": 4326,
        "cache": [{ "key": B256::repeat_byte(0x33), "value": "captured" }],
        "external_env": { "bucket_capacities": [[1, 64]] },
    });
    let before = serde_json::to_string_pretty(&fixture).unwrap();
    std::fs::write(&cache_file, &before).expect("seed fixture");

    let args = test_rpc_args_cached(&server.uri(), dir.path(), None);
    let BuildProviderOutput { cache_store, .. } =
        args.build_provider().await.expect("a foreign envelope must not fail the run");
    assert!(
        cache_store.cache_entry(&B256::repeat_byte(0x33)).is_none(),
        "a fixture's entries must not be adopted by the online cache",
    );

    assert!(cache_store.put_cache_entry(B256::repeat_byte(0x44), "ours".to_string()));
    cache_store.persist().expect("the online persist stays best-effort");
    assert_eq!(
        std::fs::read_to_string(&cache_file).expect("fixture still readable"),
        before,
        "the fixture must survive byte-identical",
    );
}

/// A cache file whose body claims another chain is a hard error, not a silent
/// restart: the per-chain file name already separates chains, so disagreement
/// inside means the file was swapped or the directory is shared across chains.
/// The persist that would follow swallows its own failures, so continuing would
/// make every later run re-fetch everything and never say why.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_hard_errors_when_the_cache_claims_another_chain() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("rpc-cache-4326.json");
    let foreign_chain = serde_json::json!({
        "version": 1,
        "kind": "cache",
        "chain_id": 6342,
        "cache": [],
        "external_env": null,
    });
    let before = serde_json::to_string_pretty(&foreign_chain).unwrap();
    std::fs::write(&cache_file, &before).expect("seed cross-chain cache");

    let args = test_rpc_args_cached(&server.uri(), dir.path(), None);
    let err = args.build_provider().await.expect_err("a cross-chain cache must fail the run");
    assert_eq!(
        ExitCode::from_evme_error(&err),
        ExitCode::ExecutionError,
        "a mispointed cache directory is bad input, not the endpoint failing to answer",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("6342") && msg.contains("4326"),
        "the error must name both chains, got: {msg}",
    );
    assert_eq!(
        std::fs::read_to_string(&cache_file).expect("file still readable"),
        before,
        "a hard error must not also delete the file",
    );
}

/// The chain-id probe runs on a bare provider, so a cache entry claiming another
/// chain cannot authenticate the cache that holds it. The cache below is a
/// well-formed one for chain 4326 whose `eth_chainId` entry answers 999; the
/// build must still resolve 4326 from the endpoint.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_chain_id_probe_ignores_a_tampered_cache_entry() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("rpc-cache-4326.json");
    let key = alloy_primitives::keccak256("eth_chainId\x00null");
    let seeded = serde_json::json!({
        "version": 1,
        "kind": "cache",
        "chain_id": 4326,
        "cache": [{
            "key": format!("{key:?}"),
            "value": r#"{"jsonrpc":"2.0","id":0,"result":"0x3e7"}"#,
        }],
        "external_env": null,
    });
    std::fs::write(&cache_file, serde_json::to_string_pretty(&seeded).unwrap()).expect("seed");

    let args = test_rpc_args_cached(&server.uri(), dir.path(), None);
    let BuildProviderOutput { chain_id, cache_store, .. } =
        args.build_provider().await.expect("build_provider");
    assert_eq!(chain_id, 4326, "the probe must come from the endpoint, not from the cache");
    assert_eq!(
        cache_store.cache_path(),
        Some(cache_file.as_path()),
        "the file is still named after the probed chain",
    );
}

/// `--rpc.clear-cache` must remove the file before load so the new session
/// starts empty even when a previous file existed.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_clear_cache_deletes_file_before_load() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(99, 1).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("rpc-cache-99.json");

    // Phase 1: populate and persist to disk.
    let seed_args = test_rpc_args_cached(&server.uri(), dir.path(), None);
    let key = B256::repeat_byte(0xCC);
    let BuildProviderOutput { cache_store: store, .. } =
        seed_args.build_provider().await.expect("seed build_provider");
    assert!(store.put_cache_entry(key, r#"{"v":1}"#.to_string()), "seed put");
    store.persist().expect("persist");
    assert!(cache_file.exists(), "seed must produce the cache file");

    // Phase 2: same dir with --rpc.clear-cache.
    let clear_args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.cache-max-entries",
        "256",
        "--rpc.cache-dir",
        dir.path().to_str().unwrap(),
        "--rpc.clear-cache",
    ]);
    let BuildProviderOutput { cache_store: store, .. } =
        clear_args.build_provider().await.expect("cleared build_provider");
    assert!(store.cache_entry(&key).is_none(), "clear-cache must wipe previously-seeded entries",);
    assert!(
        !cache_file.exists(),
        "clear-cache must delete the file (it is only recreated on persist)",
    );
}

/// `--rpc.clear-cache` fails closed when the sidecar lock cannot be acquired:
/// the user asked for a deletion that is not safe to do unlocked, so the file
/// must remain and the build must hard-error rather than unlink without the lock.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_clear_cache_fails_closed_when_lock_unacquirable() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(77, 1).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("rpc-cache-77.json");
    let seed = r#"[{"key":"0x0000000000000000000000000000000000000000000000000000000000000001","value":"seed"}]"#;
    std::fs::write(&cache_file, seed).expect("seed cache");
    // A directory in the sidecar's place makes the lock un-acquirable.
    std::fs::create_dir(format!("{}.lock", cache_file.display())).expect("occupy sidecar");

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.cache-max-entries",
        "256",
        "--rpc.cache-dir",
        dir.path().to_str().unwrap(),
        "--rpc.clear-cache",
    ]);

    let err = args.build_provider().await.expect_err("clear-cache must fail closed on lock");
    assert_eq!(
        ExitCode::from_evme_error(&err),
        ExitCode::ExecutionError,
        "a local lock failure is not the endpoint's fault: retrying or switching \
         the RPC cannot fix it, so it must not classify as an rpc failure",
    );
    match err {
        EvmeError::InvalidInput(msg) => {
            assert!(msg.contains("lock"), "error must name the lock failure, got: {msg}");
            assert!(
                msg.contains("rpc-cache-77.json.lock") || msg.contains(".lock"),
                "error must name the sidecar, got: {msg}",
            );
            assert!(
                msg.contains("Refusing to clear") || msg.contains("clear"),
                "error must state the clear was refused, got: {msg}",
            );
        }
        other => panic!("expected EvmeError::InvalidInput, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&cache_file).expect("file still readable"),
        seed,
        "no unlocked unlink happened",
    );
}

/// `--rpc.clear-cache` must hard-error when the file exists but cannot be
/// unlinked, rather than warn-and-continue. Silent fallback would reload
/// exactly the content the user asked to wipe, defeating the recovery path.
///
/// POSIX-only: we trigger the failure by revoking write permission on the
/// parent directory, so `unlink` returns EACCES/EPERM. The file itself stays
/// readable, so without the hard error the next `load_cache` call would
/// happily pick it back up.
#[tokio::test(flavor = "multi_thread")]
#[cfg(unix)]
async fn test_build_provider_clear_cache_hard_errors_on_unlink_failure() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(88, 1).await;

    let dir = tempdir().expect("tempdir");
    let chain_id: u64 = 88;
    // Seed a "polluted" cache file the user would want to wipe.
    let cache_file = dir.path().join(format!("rpc-cache-{chain_id}.json"));
    std::fs::write(&cache_file, r#"{"polluted":"content"}"#).expect("write seed");
    // Pre-create the lock sidecar while the directory is still writable.
    // Clear acquires that sidecar before unlinking; without it, a read-only
    // parent would fail lock creation first and never exercise the unlink path.
    std::fs::write(format!("{}.lock", cache_file.display()), b"").expect("seed sidecar");

    // Revoke write permission on the parent dir so `remove_file` fails.
    // Read/execute stays on so the file is still visible to `path.exists()`
    // and `load_cache`, which is exactly the silent-fallback scenario.
    let orig_perms = std::fs::metadata(dir.path()).expect("metadata").permissions();
    let mut ro_perms = orig_perms.clone();
    ro_perms.set_mode(0o555);
    std::fs::set_permissions(dir.path(), ro_perms).expect("chmod ro");

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.cache-max-entries",
        "256",
        "--rpc.cache-dir",
        dir.path().to_str().unwrap(),
        "--rpc.clear-cache",
    ]);

    let result = args.build_provider().await;

    // Restore permissions before asserting, so tempdir's Drop can clean up
    // regardless of whether the assertion below passes.
    std::fs::set_permissions(dir.path(), orig_perms).expect("chmod restore");

    let err = result.expect_err("clear-cache must hard-error on unlink failure");
    assert_eq!(
        ExitCode::from_evme_error(&err),
        ExitCode::ExecutionError,
        "a local unlink failure is not the endpoint's fault: retrying or switching \
         the RPC cannot fix it, so it must not classify as an rpc failure",
    );
    match err {
        EvmeError::InvalidInput(msg) => {
            assert!(
                msg.contains("Failed to clear RPC cache"),
                "error must name the failed operation, got: {msg}",
            );
        }
        other => panic!("expected EvmeError::InvalidInput, got {other:?}"),
    }
    assert!(cache_file.exists(), "the cache file should still be on disk — unlink failed");
}

/// `build_provider` auto-creates the cache directory under `--rpc.cache-dir`
/// when it doesn't exist yet. This is the default-path scenario where the
/// user has never run mega-evme before.
#[tokio::test(flavor = "multi_thread")]
async fn test_build_provider_auto_creates_cache_dir() {
    let server = MockRpcServer::start().await;
    // 4326 = MegaETH mainnet.
    server.respond_eth_chain_id(4326, 1).await;

    let root = tempdir().expect("tempdir");
    let fresh_dir = root.path().join("brand").join("new").join("dir");
    assert!(!fresh_dir.exists(), "precondition: dir must not exist yet");

    let args = test_rpc_args_cached(&server.uri(), &fresh_dir, None);
    let BuildProviderOutput { cache_store: store, .. } =
        args.build_provider().await.expect("build_provider");
    assert!(fresh_dir.exists(), "build_provider must create the cache directory");
    store.persist().expect("persist");
    assert!(
        fresh_dir.join("rpc-cache-4326.json").exists(),
        "persist must write the per-chain file",
    );
}

// ─── Retry layer behavior tests ──────────────────────────────────────────────

/// Server fails 503 twice then succeeds; `max-retries=3` must reach success.
/// `eth_chainId` mock at highest priority ensures `build_provider` succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_retries_on_503_then_succeeds() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    server.respond_status_n_times(503, 2, 2).await;
    server.respond_jsonrpc_result("0x10e6", 3).await;

    let BuildProviderOutput { provider, .. } =
        test_rpc_args(&server.uri(), Some(3)).build_provider().await.expect("build_provider");
    let block = provider.get_block_number().await.expect("must succeed after 2 retries");
    assert_eq!(block, 0x10e6);
}

/// `max-retries=0` must surface the first 503 immediately, with no retries.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_max_retries_zero_fails_fast() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    server.respond_status_always(503).await;

    let BuildProviderOutput { provider, .. } =
        test_rpc_args(&server.uri(), Some(0)).build_provider().await.expect("build_provider");
    let baseline = server.received_request_count().await;
    let result = provider.get_block_number().await;
    assert!(result.is_err(), "max-retries=0 must surface the 503, not retry it");
    assert_eq!(
        server.received_request_count().await - baseline,
        1,
        "max-retries=0 → exactly 1 attempt, no retries",
    );
}

/// Pins the runtime behavior of the production default `--rpc.max-retries`:
/// a permanent 503 must give up after `1 + default_max_retries` attempts.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_uses_default_max_retries() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    server.respond_status_always(503).await;

    let args = test_rpc_args(&server.uri(), None);
    assert_eq!(args.max_retries, 5, "guard against the default drifting underneath this test");

    let BuildProviderOutput { provider, .. } = args.build_provider().await.expect("build_provider");
    let baseline = server.received_request_count().await;
    let result = provider.get_block_number().await;
    assert!(result.is_err(), "all attempts return 503");
    assert_eq!(
        server.received_request_count().await - baseline,
        6,
        "default max-retries=5 → 1 initial + 5 retries",
    );
}

/// `max-retries=2` against a permanent 503: must give up after exactly 3 attempts.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_max_retries_exhausted() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    server.respond_status_always(503).await;

    let BuildProviderOutput { provider, .. } =
        test_rpc_args(&server.uri(), Some(2)).build_provider().await.expect("build_provider");
    let baseline = server.received_request_count().await;
    let result = provider.get_block_number().await;
    assert!(result.is_err(), "all attempts return 503");
    assert_eq!(
        server.received_request_count().await - baseline,
        3,
        "max-retries=2 → 1 initial + 2 retries",
    );
}

/// HTTP 500 is not retryable; even a generous retry budget must fail immediately.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_does_not_retry_non_retryable_status() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    server.respond_status_always(500).await;

    let BuildProviderOutput { provider, .. } =
        test_rpc_args(&server.uri(), Some(5)).build_provider().await.expect("build_provider");
    let baseline = server.received_request_count().await;
    let result = provider.get_block_number().await;
    assert!(result.is_err(), "500 must surface as an error");
    assert_eq!(
        server.received_request_count().await - baseline,
        1,
        "500 is not retryable → exactly 1 attempt",
    );
}

/// Transport-level errors (connection refused) are retryable. An unreachable
/// endpoint causes `build_provider` to fail at `resolve_chain_id`, proving
/// the retry layer retried and eventually gave up.
#[tokio::test(flavor = "multi_thread")]
async fn test_retry_layer_retries_on_unreachable_endpoint() {
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        port
    };
    let unreachable_url = format!("http://127.0.0.1:{}", closed_port);

    let err = test_rpc_args(&unreachable_url, Some(3))
        .build_provider()
        .await
        .expect_err("unreachable endpoint must fail at resolve_chain_id");
    let err_str = format!("{err}");
    assert!(
        err_str.contains("Failed to fetch chain ID"),
        "error must surface chain-id resolution failure, got: {err_str}",
    );
}

// ─── Request-timeout behavior ────────────────────────────────────────────────

/// A hung endpoint (accepts TCP, never responds) with
/// `--rpc.request-timeout 1` and `--rpc.max-retries 0` fails quickly at
/// chain-id resolution as `EvmeError::RpcError` (exit 3), instead of hanging.
#[tokio::test(flavor = "multi_thread")]
async fn test_request_timeout_fails_black_hole_within_bound() {
    let server = MockRpcServer::start().await;
    server.respond_black_hole().await;

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.no-cache-file",
        "--rpc.request-timeout",
        "1",
        "--rpc.max-retries",
        "0",
        "--rpc.backoff-ms",
        "1",
    ]);

    let started = std::time::Instant::now();
    let err = args.build_provider().await.expect_err("black-hole must time out");
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "must fail within a few seconds (timeout=1s, retries=0), took {elapsed:?}",
    );
    // Floor: a real 1s timeout should not return in sub-millisecond time.
    assert!(
        elapsed >= std::time::Duration::from_millis(500),
        "must wait for the request timeout, took {elapsed:?}",
    );

    match &err {
        EvmeError::RpcError(msg) => {
            assert!(
                msg.contains("Failed to fetch chain ID"),
                "timeout must surface via chain-id resolution, got: {msg}",
            );
        }
        other => panic!("expected EvmeError::RpcError, got {other:?}"),
    }
    assert_eq!(
        ExitCode::from_evme_error(&err).code(),
        3,
        "exhausted timeout must exit 3 (rpc-failure)",
    );

    // Spawned-binary assertion: the same flags on `replay` must exit 3 within
    // the wall-time bound (chain-id fetch is the first networked call).
    let started = std::time::Instant::now();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args([
            "replay",
            "--rpc",
            &server.uri(),
            "--rpc.no-cache-file",
            "--rpc.request-timeout",
            "1",
            "--rpc.max-retries",
            "0",
            "--rpc.backoff-ms",
            "1",
            "0x0000000000000000000000000000000000000000000000000000000000000001",
        ])
        .output()
        .expect("spawn mega-evme");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "spawned binary must fail within a few seconds, took {elapsed:?}",
    );
    assert_eq!(
        output.status.code(),
        Some(3),
        "spawned binary must exit 3 on timeout.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A black-hole endpoint with `--rpc.max-retries 2` makes three attempts
/// (1 initial + 2 retries) before giving up — proving reqwest timeouts are
/// classified as retryable `TransportErrorKind::Custom` errors.
#[tokio::test(flavor = "multi_thread")]
async fn test_request_timeout_is_retried_up_to_max_retries() {
    let server = MockRpcServer::start().await;
    server.respond_black_hole().await;

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.no-cache-file",
        "--rpc.request-timeout",
        "1",
        "--rpc.max-retries",
        "2",
        "--rpc.backoff-ms",
        "1",
    ]);

    let started = std::time::Instant::now();
    let err = args.build_provider().await.expect_err("black-hole must exhaust retries");
    let elapsed = started.elapsed();

    // 3 attempts × ~1s timeout + small backoffs; keep a generous upper bound.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "3×1s timeouts must finish well under 30s, took {elapsed:?}",
    );
    assert_eq!(
        server.received_request_count().await,
        3,
        "max-retries=2 → 1 initial + 2 retries against the black-hole",
    );
    assert_eq!(
        ExitCode::from_evme_error(&err).code(),
        3,
        "exhausted timeout retries must exit 3 (rpc-failure)",
    );
}

// ─── Contract regression guards (fixture-file modes) ───────────────────────

/// The `env = "RPC_URL"` attribute was removed from `--rpc`, so parsing
/// without `--rpc` must yield `rpc_url = None` regardless of environment.
#[test]
fn test_rpc_url_env_does_not_enable_capture_mode() {
    let args = RpcArgs::parse_from(["mega-evme", "--rpc.replay-file", "foo.json"]);
    assert!(args.rpc_url.is_none(), "parsing without --rpc must yield rpc_url = None");
}

/// Protects: replay path bypasses retry layer. Cache miss must return in <100ms.
#[tokio::test(flavor = "multi_thread")]
async fn test_replay_mode_cache_miss_fails_immediately() {
    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("empty.cache.json");

    // Create an envelope with an empty cache.
    let content = serde_json::json!({
        "version": 1,
        "chain_id": 6342,
        "cache": []
    });
    std::fs::write(&cache_file, serde_json::to_string(&content).unwrap()).unwrap();

    let args =
        RpcArgs::parse_from(["mega-evme", "--rpc.replay-file", cache_file.to_str().unwrap()]);
    let output = args.build_replay_provider().await.expect("build_provider");

    // Issue a request that will miss the empty cache and hit ReplayTransport.
    let start = std::time::Instant::now();
    let result = output.provider.get_chain_id().await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "cache miss must error");
    // 1s threshold: generous enough for cold CI runners under parallel load,
    // still far below the 5s retry floor so a failure here still means the
    // replay path accidentally went through the retry layer.
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "cache miss took {elapsed:?}, expected well under the 5s retry interval",
    );
}

/// Without `--rpc`, `--rpc.fixture-file` with a valid envelope enters replay mode.
/// `chain_id` is loaded from the envelope and the store is noop (read-only).
#[tokio::test(flavor = "multi_thread")]
async fn test_cache_file_mode_replay_loads_envelope() {
    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("mode_test.cache.json");
    let content = serde_json::json!({
        "version": 1,
        "chain_id": 12345,
        "cache": []
    });
    std::fs::write(&cache_file, serde_json::to_string(&content).unwrap()).unwrap();

    // --rpc.replay-file without --rpc => replay mode
    let args =
        RpcArgs::parse_from(["mega-evme", "--rpc.replay-file", cache_file.to_str().unwrap()]);
    let output = args.build_replay_provider().await.expect("replay mode should succeed");

    // In replay mode, chain_id comes from envelope
    assert_eq!(output.chain_id, 12345);
    // No network calls were made
    assert!(output.cache_store.is_noop(), "replay mode should produce noop store");
}

/// No `--rpc.capture-file`, no `--rpc.replay-file`, and no `--rpc` means
/// neither replay nor capture.
#[test]
fn test_cache_file_mode_not_used_without_flag() {
    let args = RpcArgs::parse_from(["mega-evme"]);
    assert!(args.rpc_url.is_none());
    assert!(args.capture_file.is_none());
    assert!(args.replay_file.is_none());
}

/// Verify the envelope file is not modified after replay-mode usage.
#[tokio::test(flavor = "multi_thread")]
async fn test_replay_mode_read_only() {
    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("readonly.cache.json");
    let content = serde_json::json!({
        "version": 1,
        "chain_id": 6342,
        "cache": []
    });
    std::fs::write(&cache_file, serde_json::to_string(&content).unwrap()).unwrap();

    let mtime_before = std::fs::metadata(&cache_file).unwrap().modified().unwrap();

    let args =
        RpcArgs::parse_from(["mega-evme", "--rpc.replay-file", cache_file.to_str().unwrap()]);
    let output = args.build_replay_provider().await.expect("build_provider");

    // Persist (should be noop in replay mode)
    output.cache_store.persist().expect("persist");

    let mtime_after = std::fs::metadata(&cache_file).unwrap().modified().unwrap();
    assert_eq!(mtime_before, mtime_after, "replay mode must not modify the file");
}

/// Verify that `external_env.bucket_capacities` survive a roundtrip through
/// the envelope in replay mode.
#[tokio::test(flavor = "multi_thread")]
async fn test_replay_mode_uses_envelope_external_env() {
    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("extenv.cache.json");
    let content = serde_json::json!({
        "version": 1,
        "chain_id": 6342,
        "cache": [],
        "external_env": {
            "bucket_capacities": [[123, 512], [456, 1024]]
        }
    });
    std::fs::write(&cache_file, serde_json::to_string(&content).unwrap()).unwrap();

    let args =
        RpcArgs::parse_from(["mega-evme", "--rpc.replay-file", cache_file.to_str().unwrap()]);
    let output = args.build_replay_provider().await.expect("build_provider");

    // Verify external_env is returned from envelope
    let ext = output.external_env.expect("replay mode should return external_env");
    assert_eq!(ext.bucket_capacities, vec![(123, 512), (456, 1024)]);
}

/// Capture → replay round-trip: build a capture provider backed by a mock,
/// issue a request so the transport cache records a response, persist the
/// envelope, then build a replay provider from the same file and verify the
/// cached response is served without network access.
#[tokio::test(flavor = "multi_thread")]
async fn test_capture_replay_round_trip() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;
    // Fallback: answer any other RPC call with a block number.
    server.respond_jsonrpc_result("0x42", 2).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("roundtrip.cache.json");

    // Phase 1: capture — build provider, issue a request, persist.
    let capture_args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.capture-file",
        cache_file.to_str().unwrap(),
    ]);
    let output = capture_args.build_capture_provider().await.expect("capture build");
    let block = output.provider.get_block_number().await.expect("capture get_block_number");
    assert_eq!(block, 0x42);
    output.cache_store.persist().expect("capture persist");
    assert!(cache_file.exists(), "capture must write the envelope file");

    // Phase 2: replay — no mock server, pure offline.
    let replay_args =
        RpcArgs::parse_from(["mega-evme", "--rpc.replay-file", cache_file.to_str().unwrap()]);
    let output = replay_args.build_replay_provider().await.expect("replay build");
    assert_eq!(output.chain_id, 4326, "chain_id must come from envelope");

    // The same get_block_number call must succeed from cache.
    let block = output.provider.get_block_number().await.expect("replay get_block_number");
    assert_eq!(block, 0x42, "replay must serve the cached response");
}

/// Transient JSON-RPC error bodies (e.g. `-32000 rate limit` returned with
/// HTTP 200) must not be persisted into the envelope. If they were, every
/// subsequent offline replay would bake in the same transient failure.
#[tokio::test(flavor = "multi_thread")]
async fn test_capture_does_not_cache_jsonrpc_error_response() {
    let server = MockRpcServer::start().await;
    // eth_chainId must succeed so build_capture_provider can complete.
    server.respond_eth_chain_id(4326, 1).await;
    // Any other call resolves to a JSON-RPC error body at HTTP 200.
    server.respond_jsonrpc_error(-32000, "rate limit", 2).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("err.cache.json");

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.capture-file",
        cache_file.to_str().unwrap(),
    ]);
    let output = args.build_capture_provider().await.expect("capture build");

    // The error bubbles up as a provider error; we just need the transport to
    // have observed the response.
    let _ = output.provider.get_block_number().await;
    output.cache_store.persist().expect("persist should still succeed");

    // Inspect the raw envelope: the cache array must hold only the successful
    // eth_chainId entry, not the failing call.
    let raw = std::fs::read_to_string(&cache_file).expect("read envelope");
    let envelope: serde_json::Value = serde_json::from_str(&raw).expect("parse envelope");
    let entries = envelope["cache"].as_array().expect("cache is a JSON array");
    assert_eq!(
        entries.len(),
        1,
        "only eth_chainId should be cached; error responses must be skipped. entries = {entries:#?}",
    );
    let cached: serde_json::Value =
        serde_json::from_str(entries[0]["value"].as_str().expect("value is a JSON string"))
            .expect("cached response is valid JSON");
    assert!(cached.get("result").is_some(), "cached entry must be a success response");
    assert!(cached.get("error").is_none(), "cached entry must not be an error response");
}

/// A success with `"result": null` must be served to the caller but must not
/// be baked into the capture fixture. Offline replay of that fixture then
/// fails with a cache-miss error naming the request, not a silent not-found
/// from a frozen null.
#[tokio::test(flavor = "multi_thread")]
async fn test_capture_does_not_cache_null_result_and_offline_misses() {
    let server = MockRpcServer::start().await;
    // eth_chainId must succeed so build_capture_provider can complete.
    server.respond_eth_chain_id(4326, 1).await;
    // Any other call resolves to a null result at HTTP 200.
    server.respond_jsonrpc_null_result(2).await;

    let dir = tempdir().expect("tempdir");
    let cache_file = dir.path().join("null.cache.json");

    let capture_args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.capture-file",
        cache_file.to_str().unwrap(),
    ]);
    let output = capture_args.build_capture_provider().await.expect("capture build");

    // Null is a transport-level success; the provider fails deserializing it
    // into a block number. Capture must still observe the response and skip it.
    let _ = output.provider.get_block_number().await;
    output.cache_store.persist().expect("persist should still succeed");

    // The envelope must hold only eth_chainId — no entry for the null call.
    let raw = std::fs::read_to_string(&cache_file).expect("read envelope");
    let envelope: serde_json::Value = serde_json::from_str(&raw).expect("parse envelope");
    let entries = envelope["cache"].as_array().expect("cache is a JSON array");
    assert_eq!(
        entries.len(),
        1,
        "only eth_chainId should be cached; null results must be skipped. entries = {entries:#?}",
    );
    let cached: serde_json::Value =
        serde_json::from_str(entries[0]["value"].as_str().expect("value is a JSON string"))
            .expect("cached response is valid JSON");
    assert!(
        cached.get("result").is_some_and(|r| !r.is_null()),
        "sole cached entry must be a non-null success, got {cached}",
    );

    // Offline replay: the null was never captured, so the same request is a
    // cache miss that names the method — not a silent null / not_found.
    let replay_args =
        RpcArgs::parse_from(["mega-evme", "--rpc.replay-file", cache_file.to_str().unwrap()]);
    let replay = replay_args.build_replay_provider().await.expect("replay build");
    let err = replay
        .provider
        .get_block_number()
        .await
        .expect_err("missing null entry must surface as cache miss");
    let msg = format!("{err}");
    assert!(msg.contains("cache miss"), "offline error must say 'cache miss', got: {msg}",);
    assert!(
        msg.contains("eth_blockNumber"),
        "offline error must name the missing method, got: {msg}",
    );
}

/// Cross-chain contamination guard: an existing envelope claiming chain X
/// combined with an endpoint returning chain Y must hard-error, not silently
/// mix responses from two chains.
#[tokio::test(flavor = "multi_thread")]
async fn test_capture_chain_id_mismatch_is_hard_error() {
    let server = MockRpcServer::start().await;
    // Endpoint reports chain 4326 (MegaETH mainnet).
    server.respond_eth_chain_id(4326, 1).await;

    let dir = tempdir().expect("tempdir");
    let fixture = dir.path().join("mismatch.cache.json");

    // Pre-seed an envelope that claims chain 1 (Ethereum mainnet).
    let seed = serde_json::json!({
        "version": 1,
        "chain_id": 1,
        "cache": [],
    });
    std::fs::write(&fixture, serde_json::to_string(&seed).unwrap()).unwrap();

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.capture-file",
        fixture.to_str().unwrap(),
    ]);
    let err = args.build_capture_provider().await.expect_err("chain id mismatch must hard-error");
    let msg = format!("{err}");
    assert!(
        msg.contains("Chain ID mismatch") && msg.contains('1') && msg.contains("4326"),
        "error must name both chain ids, got: {msg}",
    );
}

/// Freshness guard: the empty-seed-then-resolve ordering in
/// `build_capture_provider` means a stale `eth_chainId` entry in a pre-seeded
/// envelope must not short-circuit cross-chain validation. A seeded envelope
/// claiming chain 999 with a cached `eth_chainId=0x3e7` must still fail if the
/// live endpoint returns a different chain id.
#[tokio::test(flavor = "multi_thread")]
async fn test_capture_fresh_eth_chain_id_wins_over_stale_cache() {
    let server = MockRpcServer::start().await;
    // Live endpoint reports chain 4326.
    server.respond_eth_chain_id(4326, 1).await;

    let dir = tempdir().expect("tempdir");
    let fixture = dir.path().join("stale.cache.json");

    // Build an envelope claiming chain 999 with a matching cached eth_chainId
    // response. If build_capture_provider seeded the transport cache first and
    // resolved chain_id from it, the validation would be trivially satisfied.
    let stale_resp = r#"{"jsonrpc":"2.0","id":0,"result":"0x3e7"}"#;
    let key = alloy_primitives::keccak256("eth_chainId\x00null");
    let seed = serde_json::json!({
        "version": 1,
        "chain_id": 999,
        "cache": [
            { "key": format!("{key:?}"), "value": stale_resp }
        ],
    });
    std::fs::write(&fixture, serde_json::to_string(&seed).unwrap()).unwrap();

    let args = RpcArgs::parse_from([
        "mega-evme",
        "--rpc",
        &server.uri(),
        "--rpc.capture-file",
        fixture.to_str().unwrap(),
    ]);
    let err = args
        .build_capture_provider()
        .await
        .expect_err("fresh eth_chainId must win over stale cache entry");
    let msg = format!("{err}");
    assert!(
        msg.contains("Chain ID mismatch") && msg.contains("999") && msg.contains("4326"),
        "error must reflect fresh chain-id mismatch, got: {msg}",
    );
}

// ─── --rpc.capture-file / --rpc.replay-file clap mutex tests ────────────────

/// `--rpc.capture-file` is mutually exclusive with `--rpc.replay-file` and all
/// other cache flags. Since capture requires `--rpc`, every argv includes it.
#[test]
fn test_capture_file_mutex_with_other_cache_flags() {
    let cases: &[(&[&str], &str)] = &[
        (&["--rpc.replay-file", "/tmp/replay.json"], "--rpc.replay-file"),
        (&["--rpc.cache-dir", "/tmp/cache"], "--rpc.cache-dir"),
        (&["--rpc.clear-cache"], "--rpc.clear-cache"),
        (&["--rpc.no-cache-file"], "--rpc.no-cache-file"),
        (&["--rpc.cache-max-entries", "256"], "--rpc.cache-max-entries"),
    ];
    for (extra_flags, label) in cases {
        let mut argv =
            vec!["mega-evme", "--rpc", "http://x", "--rpc.capture-file", "/tmp/fixture.json"];
        argv.extend_from_slice(extra_flags);
        let err = RpcArgs::try_parse_from(argv)
            .expect_err(&format!("--rpc.capture-file should conflict with {label}"));
        assert!(
            err.to_string().contains("cannot be used with"),
            "{label}: error must explain the conflict, got: {err}",
        );
    }
}

/// `--rpc.replay-file` is mutually exclusive with `--rpc`, `--rpc.capture-file`,
/// and all other cache flags.
#[test]
fn test_replay_file_mutex_with_rpc_and_cache_flags() {
    let cases: &[(&[&str], &str)] = &[
        (&["--rpc", "http://x"], "--rpc"),
        (&["--rpc.capture-file", "/tmp/cap.json"], "--rpc.capture-file"),
        (&["--rpc.cache-dir", "/tmp/cache"], "--rpc.cache-dir"),
        (&["--rpc.clear-cache"], "--rpc.clear-cache"),
        (&["--rpc.no-cache-file"], "--rpc.no-cache-file"),
        (&["--rpc.cache-max-entries", "256"], "--rpc.cache-max-entries"),
    ];
    for (extra_flags, label) in cases {
        let mut argv = vec!["mega-evme", "--rpc.replay-file", "/tmp/replay.json"];
        argv.extend_from_slice(extra_flags);
        let err = RpcArgs::try_parse_from(argv)
            .expect_err(&format!("--rpc.replay-file should conflict with {label}"));
        assert!(
            err.to_string().contains("cannot be used with"),
            "{label}: error must explain the conflict, got: {err}",
        );
    }
}

/// `--rpc.capture-file` requires `--rpc`. Parsing without `--rpc` must fail.
#[test]
fn test_capture_file_requires_rpc() {
    let err = RpcArgs::try_parse_from(["mega-evme", "--rpc.capture-file", "/tmp/cap.json"])
        .expect_err("--rpc.capture-file without --rpc must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("required") || msg.contains("following"),
        "error must mention the requirement, got: {msg}",
    );
}

// ─── Caller-side validation tests ────────────────────────────────────────────

/// `build_provider` without `--rpc` errors.
#[tokio::test]
async fn test_build_provider_requires_rpc() {
    let args = RpcArgs::parse_from(["mega-evme"]);
    let err = args.build_provider().await.expect_err("should fail without --rpc");
    let msg = format!("{err}");
    assert!(msg.contains("No RPC URL"), "got: {msg}");
}

// ─── User-facing warning visibility ──────────────────────────────────────────

/// The low `--rpc.cu-per-sec` warning must reach stderr at default verbosity:
/// the tracing filter is `off` without `-v` flags, so a `warn!`-only event
/// would be discarded exactly where the user needs to learn why the run is
/// about to self-throttle.
#[tokio::test(flavor = "multi_thread")]
async fn test_low_cu_per_sec_warning_reaches_stderr_at_default_verbosity() {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(4326, 1).await;

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args([
            "replay",
            "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000",
            "--rpc",
            &server.uri(),
            "--rpc.no-cache-file",
            "--rpc.cu-per-sec",
            "50",
            "--rpc.max-retries",
            "1",
            "--rpc.backoff-ms",
            "1",
        ])
        .output()
        .expect("failed to run mega-evme");
    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(
        stderr.contains("warning:") && stderr.contains("--rpc.cu-per-sec is set to 50"),
        "the low-CU warning must be visible without -v flags, got stderr:\n{stderr}"
    );
}
