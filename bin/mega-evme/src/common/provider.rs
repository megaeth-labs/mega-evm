//! RPC provider factory and on-disk cache store for mega-evme.
//!
//! # What this module owns
//!
//! - [`RpcArgs`] — clap-parsed CLI arguments. Pure data.
//! - [`RpcCacheStore`] — owns the clean-exit cache persistence responsibility. Has a private no-op
//!   state for the "nothing to persist" case (non-fork run, `--rpc.cache-size 0`, or
//!   `--rpc.no-cache-file`).
//! - [`RpcArgs::build_provider`] — async factory: resolves chain identity, loads the on-disk cache
//!   if any, and returns a [`BuildProviderOutput`] for the call site to persist on clean exit.
//!
//! # Chain isolation
//!
//! Each chain gets its own cache file: `{cache_dir}/rpc-cache-{chain_id}.json`. `chain_id` is
//! resolved via `--rpc.chain-id` if set, otherwise fetched from the endpoint with `eth_chainId`
//! once inside [`RpcArgs::build_provider`]. The per-chain filename makes cross-chain contamination
//! impossible by construction: a cache populated from mainnet physically cannot be loaded during
//! a testnet run, because they resolve to different files.
//!
//! # Workload scope
//!
//! This factory targets historical / forked / debugging workloads (`replay`, `run --fork`,
//! `tx --fork`) that only issue read RPCs. The configured retry policy may retry rate-limit
//! responses and selected transport failures regardless of method semantics. Reusing this
//! factory for mutation RPCs (`eth_sendRawTransaction`) would risk duplicate side effects
//! and is not supported.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use alloy_provider::{
    layers::{CacheLayer, SharedCache},
    transport::{
        layers::{RateLimitRetryPolicy, RetryBackoffLayer},
        RpcError, TransportError, TransportErrorKind,
    },
    DynProvider, Provider, ProviderBuilder,
};
use alloy_rpc_client::{ClientBuilder, RpcClient};
use clap::Parser;
use tracing::{info, warn};

use super::{EvmeError, Result};

/// OP-stack provider type used throughout mega-evme.
pub type OpProvider = DynProvider<op_alloy_network::Optimism>;

/// Return value of [`RpcArgs::build_provider`].
#[derive(Debug)]
pub struct BuildProviderOutput {
    /// Configured OP-stack provider. Already wrapped with the retry layer and (unless
    /// the cache is disabled) the in-memory cache layer.
    pub provider: OpProvider,
    /// Clean-exit cache persistence handle. Call [`RpcCacheStore::persist`] on the
    /// success path; no-op when the cache is disabled.
    pub cache_store: RpcCacheStore,
    /// Resolved chain id, when known.
    ///
    /// `Some` when either `--rpc.chain-id` was set by the user, or disk persistence is
    /// enabled (in which case `build_provider` resolved it via `eth_chainId` to name
    /// the cache file). `None` when neither condition holds — downstream code that
    /// needs the chain id (e.g. `replay`'s hardfork selection) must fetch it itself.
    ///
    /// Propagating this value has two purposes: (1) honour `--rpc.chain-id` as an
    /// authoritative override end-to-end, not just for cache-file naming, and (2)
    /// avoid a second `eth_chainId` round-trip when `build_provider` already resolved it.
    pub chain_id: Option<u64>,
}

/// Configuration for building an RPC provider.
#[derive(Parser, Debug, Clone)]
#[command(next_help_heading = "RPC Options")]
pub struct RpcArgs {
    /// RPC URL
    #[arg(
        long = "rpc",
        visible_aliases = ["rpc-url"],
        alias = "fork.rpc",
        env = "RPC_URL",
        default_value = "http://localhost:8545"
    )]
    pub rpc_url: String,

    /// Maximum number of items to keep in the in-memory RPC LRU cache.
    /// Set to 0 to disable the cache layer entirely.
    #[arg(long = "rpc.cache-size", default_value_t = 10_000)]
    pub cache_size: u32,

    /// Directory for per-chain RPC cache files.
    ///
    /// Each chain's cache is stored as `{cache_dir}/rpc-cache-{chain_id}.json`. Different chains
    /// cannot share a file, so cross-chain contamination is impossible by construction.
    ///
    /// Defaults to the platform cache directory (`$XDG_CACHE_HOME/mega-evme/rpc` on
    /// Linux, `~/Library/Caches/mega-evme/rpc` on macOS). Pass `--rpc.no-cache-file`
    /// to disable on-disk persistence entirely.
    #[arg(long = "rpc.cache-dir", value_parser = parse_non_empty_path)]
    pub cache_dir: Option<PathBuf>,

    /// Disable on-disk cache persistence. The in-memory LRU cache still applies — use
    /// `--rpc.cache-size 0` to disable that too.
    #[arg(long = "rpc.no-cache-file")]
    pub no_cache_file: bool,

    /// Chain ID override. When set, `mega-evme` skips the `eth_chainId` call at startup
    /// and uses this value both to locate the per-chain cache file and to feed downstream
    /// chain-dependent logic (e.g. `replay`'s hardfork and spec selection). Use for fully
    /// offline replay against an existing cache, or to keep tests hermetic.
    #[arg(id = "rpc_chain_id", long = "rpc.chain-id")]
    pub chain_id: Option<u64>,

    /// Delete the current chain's cache file before loading it. Recovery path for a
    /// polluted or corrupt cache file. If the unlink itself fails (e.g. insufficient
    /// permissions), `mega-evme` aborts rather than silently reloading the stale file.
    #[arg(long = "rpc.clear-cache")]
    pub clear_cache: bool,

    /// Maximum number of times the transport layer will retry a failing RPC request.
    /// Retries trigger on HTTP 429 / 503, JSON-RPC rate-limit error responses, and
    /// transport failures surfaced as `TransportErrorKind::Custom` (connection refused,
    /// DNS failure, TLS handshake, etc.). Set to 0 to disable retries entirely.
    #[arg(long = "rpc.max-retries", default_value_t = 5)]
    pub max_retries: u32,

    /// Fixed sleep duration, in milliseconds, inserted between retry attempts unless the
    /// server supplies its own backoff hint. This layer does not perform exponential backoff.
    #[arg(long = "rpc.backoff-ms", default_value_t = 1_000)]
    pub backoff_ms: u64,

    /// Compute units per second budget passed to the retry layer's rate-limit accounting.
    #[arg(long = "rpc.rate-limit", default_value_t = 660)]
    pub compute_units_per_sec: u64,
}

impl RpcArgs {
    /// Build the RPC provider, its clean-exit cache store, and a resolved chain-id hint,
    /// bundled into a [`BuildProviderOutput`] record.
    ///
    /// See [`BuildProviderOutput::chain_id`] for the hint's semantics — it is `Some` when
    /// the user passed `--rpc.chain-id` or when disk persistence forced us to resolve
    /// `eth_chainId` to compute the cache file path, and `None` otherwise.
    ///
    /// Fast path (`--rpc.cache-size 0`): no cache layer, no network call for the chain id,
    /// returns a no-op [`RpcCacheStore`] with the user's override (possibly `None`).
    ///
    /// Otherwise: installs an in-memory cache layer. If on-disk persistence is enabled
    /// (`--rpc.no-cache-file` not set), resolves `chain_id` — via `--rpc.chain-id` if
    /// set, otherwise via an `eth_chainId` call against the endpoint — and computes the
    /// cache file path as `{cache_dir}/rpc-cache-{chain_id}.json`.
    ///
    /// # Error surface
    ///
    /// Hard errors (returned as [`EvmeError::RpcError`]):
    /// - invalid `--rpc` URL;
    /// - `eth_chainId` resolution failure (when disk cache is enabled and no override);
    /// - default cache directory unavailable (only when `--rpc.cache-dir` is not set);
    /// - `--rpc.clear-cache` unlink failure — this path is intentionally strict because silently
    ///   falling back to `load_cache` would reload exactly the content the user asked to wipe.
    ///
    /// Warn-and-continue (best-effort paths):
    /// - `create_dir_all` on the cache directory (if it fails, the later `persist()` will also fail
    ///   and log its own warning);
    /// - `load_cache` on a corrupt or unreadable file — the in-memory cache starts empty.
    pub async fn build_provider(&self) -> Result<BuildProviderOutput> {
        let url: reqwest::Url = self.rpc_url.parse().map_err(|e| {
            EvmeError::RpcError(format!("Invalid RPC URL '{}': {}", self.rpc_url, e))
        })?;

        // 1. Fast path: cache fully disabled.
        if self.cache_size == 0 {
            let provider = build_bare_op_provider(self.build_retry_client(url));
            info!(
                rpc_url = %self.rpc_url,
                max_retries = self.max_retries,
                backoff_ms = self.backoff_ms,
                "Built RPC provider (cache disabled)",
            );
            // Propagate the user's override (if any) without touching the network.
            return Ok(BuildProviderOutput {
                provider,
                cache_store: RpcCacheStore::noop(),
                chain_id: self.chain_id,
            });
        }

        // 2. Cache enabled. Resolve the chain id exactly once, with two branches:
        //    - disk persistence on: we *must* have a chain id for the file name, so fetch
        //      `eth_chainId` unless the user provided an override;
        //    - disk persistence off: we only care about the chain id if the user gave us an
        //      override, because there is no file name to compute.
        let chain_id_hint: Option<u64> = if self.no_cache_file {
            self.chain_id
        } else {
            Some(self.resolve_chain_id(url.clone()).await?)
        };

        // 3. Cache enabled. Resolve the on-disk cache path (or `None` if disk persistence is
        //    disabled — the in-memory layer still applies).
        let cache_path = if self.no_cache_file {
            None
        } else {
            // `chain_id_hint` is guaranteed `Some` in this branch because we just resolved it.
            let chain_id =
                chain_id_hint.expect("chain id resolved above when disk persistence is enabled");
            Some(resolve_cache_path(self.cache_dir.as_deref(), chain_id)?)
        };

        // 4. Build the cache layer and (optionally) the disk store.
        let cache_layer = CacheLayer::new(self.cache_size);
        let cache = cache_layer.cache();
        let cache_store = match cache_path {
            Some(path) => {
                if self.clear_cache {
                    match std::fs::remove_file(&path) {
                        Ok(()) => info!(path = %path.display(), "Cleared existing RPC cache"),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            // Hard error rather than warn-and-continue:
                            // `--rpc.clear-cache` is the user's explicit
                            // recovery path, and if the unlink silently
                            // failed the `load_cache` call below would
                            // reload exactly the content the user asked
                            // to wipe.
                            return Err(EvmeError::RpcError(format!(
                                "Failed to clear RPC cache at {}: {}",
                                path.display(),
                                e
                            )));
                        }
                    }
                }
                if let Some(parent) = path.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        warn!(
                            path = %parent.display(),
                            error = %e,
                            "Failed to create cache directory; persist may fail",
                        );
                    }
                }
                if path.exists() {
                    if let Err(err) = cache.load_cache(path.clone()) {
                        warn!(
                            path = %path.display(),
                            error = %err,
                            "Failed to load RPC cache; starting empty",
                        );
                    }
                }
                RpcCacheStore::new(cache, path)
            }
            None => RpcCacheStore::noop(),
        };

        // 5. Build the cached provider. The same retry policy wraps both the throwaway chain-id
        //    client (if used) and this one.
        let client = self.build_retry_client(url);
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .layer(cache_layer)
            .network::<op_alloy_network::Optimism>()
            .connect_client(client);

        info!(
            rpc_url = %self.rpc_url,
            cache_size = self.cache_size,
            max_retries = self.max_retries,
            backoff_ms = self.backoff_ms,
            "Built RPC provider",
        );

        Ok(BuildProviderOutput {
            provider: DynProvider::new(provider),
            cache_store,
            chain_id: chain_id_hint,
        })
    }

    /// Build an `RpcClient` wired with the configured retry layer (or a bare client
    /// when `max_retries == 0`). Used both for the cached provider and, when
    /// `--rpc.chain-id` is not set, for the throwaway provider that fetches
    /// `eth_chainId` inside [`Self::resolve_chain_id`].
    fn build_retry_client(&self, url: reqwest::Url) -> RpcClient {
        if self.max_retries > 0 {
            let policy = RateLimitRetryPolicy::default().or(|err: &TransportError| {
                matches!(err, RpcError::Transport(TransportErrorKind::Custom(_)))
            });
            let retry = RetryBackoffLayer::new_with_policy(
                self.max_retries,
                self.backoff_ms,
                self.compute_units_per_sec,
                policy,
            );
            ClientBuilder::default().layer(retry).http(url)
        } else {
            ClientBuilder::default().http(url)
        }
    }

    /// Resolve the chain ID. Prefers the `--rpc.chain-id` override; otherwise
    /// issues `eth_chainId` against a throwaway cache-less provider using the
    /// configured retry policy. Hard error on transport / RPC failure.
    async fn resolve_chain_id(&self, url: reqwest::Url) -> Result<u64> {
        if let Some(id) = self.chain_id {
            return Ok(id);
        }
        let bare = build_bare_op_provider(self.build_retry_client(url));
        bare.get_chain_id().await.map_err(|e| {
            EvmeError::RpcError(format!("Failed to fetch chain ID from '{}': {}", self.rpc_url, e))
        })
    }
}

/// `clap` value parser for `--rpc.cache-dir` that rejects empty and
/// whitespace-only arguments at parse time.
///
/// Without this check, `--rpc.cache-dir ""` would pass clap, land in
/// [`resolve_cache_path`] as `PathBuf::from("")`, and silently write the
/// per-chain file to the process's current working directory — a surprising
/// footgun, since the same command run from different cwds would produce
/// different cache files with no visible indication.
fn parse_non_empty_path(s: &str) -> std::result::Result<PathBuf, String> {
    if s.trim().is_empty() {
        Err("cache-dir must not be empty; omit the flag to use the default".to_string())
    } else {
        Ok(PathBuf::from(s))
    }
}

/// Build a cache-less [`OpProvider`] from an already-configured `RpcClient`.
///
/// Used by the cache-disabled fast path and by the throwaway chain-id fetch.
/// The cache-enabled path builds its provider inline because the cache layer
/// has to be inserted into the `ProviderBuilder` chain before the client is
/// attached.
fn build_bare_op_provider(client: RpcClient) -> OpProvider {
    DynProvider::new(
        ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<op_alloy_network::Optimism>()
            .connect_client(client),
    )
}

/// Resolve the absolute cache file path for `chain_id`.
///
/// If the user passed `--rpc.cache-dir` we use it verbatim. Otherwise we fall back to
/// the platform cache directory (via `dirs::cache_dir()`): `$XDG_CACHE_HOME/mega-evme/rpc`
/// on Linux, `~/Library/Caches/mega-evme/rpc` on macOS, `%LOCALAPPDATA%\mega-evme\rpc` on
/// Windows. If the platform has no cache directory we error out and ask the user to
/// either pass `--rpc.cache-dir` or `--rpc.no-cache-file`.
fn resolve_cache_path(user_cache_dir: Option<&Path>, chain_id: u64) -> Result<PathBuf> {
    let dir = match user_cache_dir {
        Some(dir) => dir.to_path_buf(),
        None => dirs::cache_dir()
            .ok_or_else(|| {
                EvmeError::RpcError(
                    "Could not determine default cache directory; pass --rpc.cache-dir \
                     or --rpc.no-cache-file"
                        .to_string(),
                )
            })?
            .join("mega-evme")
            .join("rpc"),
    };
    Ok(dir.join(format!("rpc-cache-{chain_id}.json")))
}

/// Clean-exit cache persistence handle.
///
/// An `RpcCacheStore` may internally have nothing to persist — non-fork run,
/// `--rpc.cache-size 0`, or `--rpc.no-cache-file`. In any of those cases
/// `persist()` is a no-op. Callers do not and must not branch on whether
/// a given store is real or no-op; the whole point of this type is a single
/// uniform persistence entry point.
///
/// # Why not `Drop`
///
/// Persistence is **clean-exit-only**: callers invoke `persist()` explicitly on the
/// success path. `Drop` also runs on panic and error unwind, so a `Drop`-based
/// implementation would silently persist partial-run state. That is a correctness
/// violation, not a style choice — do not "simplify" this type into a `Drop` impl.
pub struct RpcCacheStore {
    /// `Some` when there is a cache to persist on clean exit; `None` is the no-op state.
    inner: Option<RpcCacheStoreInner>,
}

struct RpcCacheStoreInner {
    cache: SharedCache,
    cache_path: PathBuf,
}

impl RpcCacheStore {
    /// Construct a store backed by a real cache and target file.
    pub(crate) fn new(cache: SharedCache, cache_path: PathBuf) -> Self {
        Self { inner: Some(RpcCacheStoreInner { cache, cache_path }) }
    }

    /// Construct a no-op store.
    pub(crate) fn noop() -> Self {
        Self { inner: None }
    }

    // ─── Test-only accessors ─────────────────────────────────────────────
    //
    // The three accessors below are gated on `cfg(any(test, feature =
    // "test-utils"))` because they leak internal state that the owner type
    // is otherwise designed to hide. Production code must not branch on any
    // of them — call `persist()` instead, which is a no-op when there is
    // nothing to persist. Tests need them to assert wiring and to seed
    // cache entries without going through a real (or mock) RPC round-trip.

    /// True if this store is the no-op variant (nothing to persist).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn is_noop(&self) -> bool {
        self.inner.is_none()
    }

    /// Returns the underlying [`SharedCache`], or `None` for a no-op store.
    ///
    /// `SharedCache` is `Arc`-backed with interior mutability, so any caller
    /// holding the returned `&SharedCache` can keep mutating the cache after
    /// `persist()` — silently subverting the consume-on-persist contract.
    /// That is exactly why this API is unavailable in production builds.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache(&self) -> Option<&SharedCache> {
        self.inner.as_ref().map(|inner| &inner.cache)
    }

    /// Returns the resolved cache file path, or `None` for a no-op store.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache_path(&self) -> Option<&Path> {
        self.inner.as_ref().map(|inner| inner.cache_path.as_path())
    }

    /// Persist the RPC cache to its resolved path atomically. **Consumes the store**,
    /// which type-level-enforces "persist once, then stop" — see `Why not Drop` on this
    /// type for the correctness reason behind the consuming signature.
    ///
    /// Safe to call on any `RpcCacheStore`, including a no-op one. Persistence is strictly
    /// best-effort: I/O failures are warn-logged and swallowed so a save failure cannot
    /// turn a successful run into a non-zero exit. Callers invoke this **without** `?` on
    /// the clean-exit path.
    pub fn persist(self) {
        let Some(inner) = self.inner else { return };
        match save_cache_atomic(&inner.cache, &inner.cache_path) {
            Ok(()) => info!(path = %inner.cache_path.display(), "Persisted RPC cache"),
            Err(err) => warn!(
                path = %inner.cache_path.display(),
                error = %err,
                "Failed to save RPC cache (continuing)",
            ),
        }
    }
}

// Manual `Debug` because `SharedCache` does not implement `Debug` and the only
// debugging detail worth showing is the cache file path anyway.
impl fmt::Debug for RpcCacheStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Some(inner) => f
                .debug_struct("RpcCacheStore")
                .field("cache_path", &inner.cache_path)
                .finish_non_exhaustive(),
            None => f.debug_struct("RpcCacheStore").field("inner", &Option::<()>::None).finish(),
        }
    }
}

/// Compute the temporary file path used by [`save_cache_atomic`] for `target`.
///
/// The result is in the same directory as `target` (so that the subsequent
/// `rename` is atomic on POSIX), is never byte-equal to `target`, and carries
/// a per-call uniqueness suffix (`pid`.`unix_nanos`.`atomic_counter`) so two
/// calls — even back-to-back on coarse clocks, even in concurrent processes —
/// never collide.
///
/// Returns an error if `target` has no file name component (e.g. a bare root).
fn temp_path_for(target: &Path) -> std::io::Result<PathBuf> {
    use std::{
        ffi::OsString,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    let file_name = target.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache file path has no file name")
    })?;

    let pid = std::process::id();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);

    // Strictly-increasing counter for per-call uniqueness when back-to-back
    // calls land on the same nanosecond on coarse clocks.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut temp_name = OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(".mega-evme-tmp.{pid}.{nanos}.{counter}"));

    Ok(target.with_file_name(temp_name))
}

/// Atomically persist `cache` to `target`.
///
/// Success: `target` holds a complete snapshot in the upstream serialization
/// format. Failure: `target` is unmodified and the temp file is removed.
/// Temp-cleanup errors are silently ignored because the caller is already on
/// the best-effort path and the original error is the load-bearing one.
fn save_cache_atomic(cache: &SharedCache, target: &Path) -> std::io::Result<()> {
    let tmp = temp_path_for(target)?;

    // 1. Serialize into the temp file.
    if let Err(e) = cache.save_cache(tmp.clone()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::other(format!("{e}")));
    }

    // 2. Atomic rename onto the target. Same parent dir = same filesystem, which is what makes
    //    `rename` atomic on POSIX.
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Inline tests for the **private** [`temp_path_for`] helper and
    //! [`resolve_cache_path`]. Public-API tests for `RpcArgs::build_provider`
    //! and `RpcCacheStore` live in `bin/mega-evme/tests/provider.rs`.

    use std::path::PathBuf;

    use super::*;

    /// A target whose own filename happens to look like a mega-evme temp file
    /// must still get a strictly different temp path.
    #[test]
    fn test_temp_path_self_collision_freedom() {
        let target = PathBuf::from("/tmp/.foo.cache.mega-evme-tmp.1.2.3");
        let tmp = temp_path_for(&target).expect("temp_path_for");
        assert_ne!(tmp, target, "temp path must not equal target path");
    }

    /// Two distinct targets that share only their stem must produce distinct
    /// temp paths.
    #[test]
    fn test_temp_path_stem_collision_freedom() {
        let target_a = PathBuf::from("/tmp/foo.cache");
        let target_b = PathBuf::from("/tmp/foo.json");
        let tmp_a = temp_path_for(&target_a).expect("temp_path_for");
        let tmp_b = temp_path_for(&target_b).expect("temp_path_for");
        assert_ne!(tmp_a, tmp_b, "distinct targets must get distinct temp paths");
        let name_a = tmp_a.file_name().unwrap().to_string_lossy().into_owned();
        let name_b = tmp_b.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name_a.contains("foo.cache"), "temp name should embed full target file name");
        assert!(name_b.contains("foo.json"), "temp name should embed full target file name");
    }

    /// Two consecutive calls with the same target must produce distinct temp
    /// paths (per-call uniqueness via the atomic counter).
    #[test]
    fn test_temp_path_per_call_uniqueness() {
        let target = PathBuf::from("/tmp/foo.cache");
        let tmp1 = temp_path_for(&target).expect("temp_path_for #1");
        let tmp2 = temp_path_for(&target).expect("temp_path_for #2");
        assert_ne!(tmp1, tmp2, "consecutive calls must produce distinct temp paths");
    }

    /// The temp file must live in the target's parent dir — required for
    /// `rename` to be atomic on POSIX.
    #[test]
    fn test_temp_path_same_directory() {
        let target = PathBuf::from("/some/nested/dir/cache.json");
        let tmp = temp_path_for(&target).expect("temp_path_for");
        assert_eq!(
            tmp.parent(),
            target.parent(),
            "temp file must share the target's parent directory"
        );
    }

    /// A target with no file name component (e.g. a bare root) must error.
    #[test]
    fn test_temp_path_no_file_name_errors() {
        let target = PathBuf::from("/");
        let err = temp_path_for(&target).expect_err("root path has no file name");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// Explicit `--rpc.cache-dir` is used verbatim; the file is `<chain_id>.json` inside it.
    #[test]
    fn test_resolve_cache_path_with_explicit_dir() {
        let dir = PathBuf::from("/some/user/dir");
        // 4326 = MegaETH mainnet.
        let path = resolve_cache_path(Some(&dir), 4326).expect("resolve");
        assert_eq!(path, PathBuf::from("/some/user/dir/rpc-cache-4326.json"));
    }

    /// No explicit dir falls back to `dirs::cache_dir()` on platforms that have one.
    /// The path ends in `mega-evme/rpc/rpc-cache-<chain_id>.json`.
    #[test]
    fn test_resolve_cache_path_default_is_under_platform_cache() {
        let Some(expected_root) = dirs::cache_dir() else {
            // Skip on exotic platforms where no cache directory is available.
            return;
        };
        let path = resolve_cache_path(None, 11_155_420).expect("resolve");
        let expected = expected_root.join("mega-evme").join("rpc").join("rpc-cache-11155420.json");
        assert_eq!(path, expected);
    }
}
