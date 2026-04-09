//! RPC provider factory and session lifetime owners for mega-evme.
//!
//! # Type hierarchy
//!
//! - [`RpcArgs`] — clap-parsed CLI arguments. Pure CLI-layer data.
//! - [`RpcFinalizer`] — owns the clean-exit cache persistence responsibility, including the
//!   "nothing to persist" case as a private internal state.
//! - [`RpcSession`] — owns the provider and the finalizer for a run that constructs the provider
//!   directly (today: `replay`).
//! - `InitialStateSession` (defined in `state.rs`) — owns the [`EvmeState`] and the finalizer for a
//!   run that constructs initial execution state (today: `run`, `tx`).
//!
//! [`EvmeState`]: super::EvmeState
//!
//! # Boundary contracts
//!
//! - **Workload scope**: this factory targets historical / forked / debugging workloads. The
//!   commands it serves (`replay`, `run --fork`, `tx --fork`) only issue read RPCs.
//! - **Cache file reuse**: the alloy `CacheLayer` keys requests by `keccak256(block_id + method +
//!   params)` and does NOT include endpoint or chain identity. Cache files must not be reused
//!   across different RPC endpoints or different chains. The tool does not detect violations.
//! - **Retry idempotency**: the configured retry policy may retry rate-limit responses and selected
//!   transport failures regardless of method semantics. Reusing this factory for mutation RPCs
//!   (e.g. `eth_sendRawTransaction`) would risk duplicate side effects and is not supported.

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
    DynProvider, ProviderBuilder,
};
use alloy_rpc_client::ClientBuilder;
use clap::Parser;
use tracing::{debug, warn};

use super::{EvmeError, Result};

/// OP-stack provider type used throughout mega-evme.
pub type OpProvider = DynProvider<op_alloy_network::Optimism>;

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

    /// Path to a file used to persist the RPC cache between runs (best-effort).
    ///
    /// Cache files are bound by user contract to one specific (endpoint, chain) pair.
    /// Reusing the same file across different endpoints or chains will silently return
    /// responses for the wrong chain. The tool does not detect violations.
    #[arg(long = "rpc.cache-file")]
    pub cache_file: Option<PathBuf>,

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
    /// Build an [`RpcSession`] from this CLI configuration.
    ///
    /// The session owns an [`OpProvider`] (configured with the optional retry
    /// and cache layers) and an [`RpcFinalizer`]. The finalizer is a real one
    /// when `--rpc.cache-size > 0` and `--rpc.cache-file` is set; otherwise it
    /// is a no-op. Callers do not need to distinguish the two cases.
    pub fn build_session(&self) -> Result<RpcSession> {
        let url = self.rpc_url.parse().map_err(|e| {
            EvmeError::RpcError(format!("Invalid RPC URL '{}': {}", self.rpc_url, e))
        })?;

        // 1. RpcClient with optional transport-level retry.
        //
        // Default `RateLimitRetryPolicy` covers HTTP 429 / 503 and JSON-RPC
        // rate-limit error responses. We extend it with `TransportErrorKind::Custom`
        // so connection refused / DNS / TLS failures are also retried. Any error
        // judged retryable still bails out after `max_retries` attempts with a
        // wrapped "Max retries exceeded" error.
        let client = if self.max_retries > 0 {
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
        };

        // 2. Provider with optional provider-level cache, plus a matching finalizer.
        let (provider, finalizer) = if self.cache_size > 0 {
            let cache_layer = CacheLayer::new(self.cache_size);
            let cache = cache_layer.cache();
            let finalizer = if let Some(path) = &self.cache_file {
                if let Err(err) = cache.load_cache(path.clone()) {
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "Failed to load RPC cache; starting empty",
                    );
                }
                RpcFinalizer::new(cache, path.clone())
            } else {
                // Cache layer is active in-memory but no file → nothing to persist.
                RpcFinalizer::noop()
            };
            let provider = ProviderBuilder::new()
                .disable_recommended_fillers()
                .layer(cache_layer)
                .network::<op_alloy_network::Optimism>()
                .connect_client(client);
            (DynProvider::new(provider), finalizer)
        } else {
            let provider = ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<op_alloy_network::Optimism>()
                .connect_client(client);
            (DynProvider::new(provider), RpcFinalizer::noop())
        };

        debug!(
            rpc_url = %self.rpc_url,
            cache_size = self.cache_size,
            max_retries = self.max_retries,
            backoff_ms = self.backoff_ms,
            "Built RPC session",
        );

        Ok(RpcSession { provider, finalizer })
    }
}

/// Clean-exit session finalization handle.
///
/// A `RpcFinalizer` may internally have nothing to finalize — non-fork run,
/// `--rpc.cache-size 0`, or no `--rpc.cache-file`. In those cases `finalize()`
/// is a no-op. Callers do not and must not distinguish the two cases; the whole
/// point of this type is a single uniform finalization entry point.
///
/// # Why not `Drop`
///
/// Persistence is **clean-exit-only**: callers invoke `finalize()` explicitly
/// on the success path. `Drop` also runs on panic and error unwind, so a
/// `Drop`-based implementation would silently persist partial-run state. That
/// is a correctness violation, not a style choice — do not "simplify" this
/// type into a `Drop` impl.
pub struct RpcFinalizer {
    /// `Some` when there is a cache to persist on clean exit; `None` is the
    /// no-op state.
    inner: Option<RpcFinalizerInner>,
}

struct RpcFinalizerInner {
    cache: SharedCache,
    cache_file: PathBuf,
}

impl RpcFinalizer {
    /// Construct a finalizer backed by a real cache and target file.
    pub(crate) fn new(cache: SharedCache, cache_file: PathBuf) -> Self {
        Self { inner: Some(RpcFinalizerInner { cache, cache_file }) }
    }

    /// Construct a no-op finalizer.
    pub(crate) fn noop() -> Self {
        Self { inner: None }
    }

    // ─── Test-only accessors ─────────────────────────────────────────────
    //
    // The three accessors below are gated on `cfg(any(test, feature =
    // "test-utils"))` because they leak internal state that the owner type
    // is otherwise designed to hide. Production code must not branch on any
    // of them — call `finalize()` instead, which is a no-op when there is
    // nothing to persist. Tests need them to assert wiring and to seed
    // cache entries without going through a real (or mock) RPC round-trip.

    /// True if this finalizer is the no-op variant (nothing to persist).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn is_noop(&self) -> bool {
        self.inner.is_none()
    }

    /// Returns the underlying [`SharedCache`], or `None` for a no-op finalizer.
    ///
    /// `SharedCache` is `Arc`-backed with interior mutability, so any caller
    /// holding the returned `&SharedCache` can keep mutating the cache after
    /// `finalize()` — silently subverting the consume-on-finalize contract.
    /// That is exactly why this API is unavailable in production builds.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache(&self) -> Option<&SharedCache> {
        self.inner.as_ref().map(|inner| &inner.cache)
    }

    /// Returns the configured cache file path, or `None` for a no-op finalizer.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache_file(&self) -> Option<&Path> {
        self.inner.as_ref().map(|inner| inner.cache_file.as_path())
    }

    /// Clean-exit session finalization. **Consumes the finalizer**, which
    /// type-level-enforces "finalize once, then stop" — see `Why not Drop`
    /// on this type for the correctness reason behind the consuming signature.
    ///
    /// Today this atomically persists the RPC cache to its configured path.
    /// Safe to call on any `RpcFinalizer`, including a no-op one. Persistence
    /// is strictly best-effort: I/O failures are warn-logged and swallowed so
    /// a save failure cannot turn a successful run into a non-zero exit.
    /// Callers invoke this **without** `?` on the clean-exit path.
    pub fn finalize(self) {
        let Some(inner) = self.inner else { return };
        match save_cache_atomic(&inner.cache, &inner.cache_file) {
            Ok(()) => debug!(path = %inner.cache_file.display(), "Persisted RPC cache"),
            Err(err) => warn!(
                path = %inner.cache_file.display(),
                error = %err,
                "Failed to save RPC cache (continuing)",
            ),
        }
    }
}

// Manual `Debug` because `SharedCache` does not implement `Debug` and the only
// debugging detail worth showing is the cache file path anyway.
impl fmt::Debug for RpcFinalizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Some(inner) => f
                .debug_struct("RpcFinalizer")
                .field("cache_file", &inner.cache_file)
                .finish_non_exhaustive(),
            None => f.debug_struct("RpcFinalizer").field("inner", &Option::<()>::None).finish(),
        }
    }
}

/// Owns the provider and cache finalization for a run that constructs its
/// provider directly (today: `replay`).
///
/// Persistence is clean-exit-only for the same reason as [`RpcFinalizer`]:
/// call `finalize()` explicitly on the success path.
#[derive(Debug)]
pub struct RpcSession {
    provider: OpProvider,
    finalizer: RpcFinalizer,
}

impl RpcSession {
    /// Borrow the underlying provider for short-lived use.
    ///
    /// [`OpProvider`] is `Arc`-backed, so call sites that need an owned copy
    /// (e.g. to hand to `EvmeState::new_forked`) can `.clone()` the borrow
    /// cheaply.
    pub fn provider(&self) -> &OpProvider {
        &self.provider
    }

    /// Split the session into `(provider, finalizer)`.
    ///
    /// Use this when the provider must be moved into a long-lived owner such
    /// as `EvmeState::new_forked` while the caller still needs to finalize the
    /// cache later via the detached finalizer. This is the bridge between
    /// [`RpcSession`] and [`InitialStateSession`](super::InitialStateSession).
    pub fn into_parts(self) -> (OpProvider, RpcFinalizer) {
        (self.provider, self.finalizer)
    }

    /// Test-only: true if this session is the no-op variant. See
    /// [`RpcFinalizer::is_noop`] for why production code must not branch on
    /// this.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn is_noop(&self) -> bool {
        self.finalizer.is_noop()
    }

    /// Clean-exit session finalization. **Consumes the session.** No-op if
    /// this session has nothing to finalize. See [`RpcFinalizer::finalize`].
    pub fn finalize(self) {
        self.finalizer.finalize();
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
    //! Inline tests for the **private** [`temp_path_for`] helper. Public-API
    //! tests for `RpcSession` / `RpcFinalizer` / `RpcArgs` live in
    //! `bin/mega-evme/tests/provider.rs`.

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
}
