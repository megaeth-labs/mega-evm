//! RPC provider factory and on-disk cache store for mega-evme.
//!
//! Three provider builders for different use cases:
//!
//! - [`RpcArgs::build_provider`] — standard RPC with optional `--rpc.cache-dir` persistence.
//! - [`RpcArgs::build_replay_provider`] — offline replay from a `--rpc.replay-file` envelope.
//! - [`RpcArgs::build_capture_provider`] — RPC with transport-level capture to
//!   `--rpc.capture-file`.
//!
//! The `--rpc.cache-dir` path uses alloy's provider-level `CacheLayer` (caches ~8 methods).
//! The `--rpc.capture-file` / `--rpc.replay-file` paths use a transport-level
//! [`CachingTransport`] / [`ReplayTransport`] that captures single JSON-RPC request/response
//! pairs.

mod cache_store;
mod transport;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use alloy_provider::{
    layers::CacheLayer,
    transport::{
        layers::{RateLimitRetryPolicy, RetryBackoffLayer},
        RpcError, TransportError, TransportErrorKind,
    },
    DynProvider, Provider, ProviderBuilder,
};
use alloy_rpc_client::{ClientBuilder, RpcClient};
use clap::Parser;
use tracing::{debug, info, warn};

pub use self::cache_store::{ExternalEnvSnapshot, RpcCacheStore};
use self::{
    cache_store::CacheFileEnvelope,
    transport::{CachingTransport, ReplayTransport, TransportCache},
};
use super::{EvmeError, Result};
use crate::cache::{acquire_exclusive_lock, lock_sidecar_path};

/// OP-stack provider type used throughout mega-evme.
pub type OpProvider = DynProvider<op_alloy_network::Optimism>;

/// Return value of the `RpcArgs::build_*_provider` methods.
#[derive(Debug)]
pub struct BuildProviderOutput {
    /// Configured OP-stack provider. Already wrapped with the retry layer and the
    /// in-memory cache layer.
    pub provider: OpProvider,
    /// Clean-exit cache persistence handle. Call [`RpcCacheStore::persist`] on the
    /// success path; no-op when on-disk persistence is disabled (`--rpc.no-cache-file`).
    pub cache_store: RpcCacheStore,
    /// Chain id resolved during provider construction. Always populated —
    /// comes from `eth_chainId` (standard/capture) or the envelope (replay).
    pub chain_id: u64,
    /// External environment snapshot from the envelope (replay and capture refresh).
    pub external_env: Option<ExternalEnvSnapshot>,
}

/// Configuration for building an RPC provider.
#[derive(Parser, Debug, Clone)]
#[command(next_help_heading = "RPC Options")]
pub struct RpcArgs {
    /// RPC URL. Required for networked operation (replay, run --fork, tx --fork).
    #[arg(
        long = "rpc",
        visible_aliases = ["rpc-url"],
        alias = "fork.rpc",
    )]
    pub rpc_url: Option<String>,

    /// Capture JSON-RPC responses to a single file for later offline replay.
    /// Requires `--rpc`.
    /// If the file already exists, its entries are loaded and merged;
    /// missing entries are fetched via the RPC endpoint and persisted on clean exit.
    /// Cannot be used with --rpc.replay-file, --rpc.cache-dir, --rpc.clear-cache,
    /// --rpc.no-cache-file, or --rpc.cache-max-entries.
    #[arg(
        long = "rpc.capture-file",
        value_parser = parse_non_empty_path,
        requires = "rpc_url",
        conflicts_with_all = ["replay_file", "cache_dir", "clear_cache", "no_cache_file", "cache_max_entries"],
    )]
    pub capture_file: Option<PathBuf>,

    /// Replay from a previously captured JSON-RPC fixture file (offline).
    /// Cannot be used with `--rpc`.
    /// Any RPC miss is a hard error; the file is never written.
    /// Cannot be used with --rpc.capture-file, --rpc.cache-dir, --rpc.clear-cache,
    /// --rpc.no-cache-file, or --rpc.cache-max-entries.
    #[arg(
        long = "rpc.replay-file",
        value_parser = parse_non_empty_path,
        conflicts_with_all = ["rpc_url", "capture_file", "cache_dir", "clear_cache", "no_cache_file", "cache_max_entries"],
    )]
    pub replay_file: Option<PathBuf>,

    /// Maximum number of items in the in-memory RPC LRU cache (and therefore what
    /// gets persisted to the cache file). `0` = effectively unlimited (caps at
    /// 1,048,576 entries; the cache index is preallocated proportional to the
    /// cap). Default is `0`.
    #[arg(id = "cache_max_entries", long = "rpc.cache-max-entries", default_value_t = 0)]
    pub cache_max_entries: u32,

    /// Directory for per-chain RPC cache files.
    ///
    /// Each chain's cache is stored as `{cache_dir}/rpc-cache-{chain_id}.json`. Different chains
    /// cannot share a file, so cross-chain contamination is impossible by construction.
    ///
    /// Defaults to the platform cache directory (`$XDG_CACHE_HOME/mega-evme/rpc` on
    /// Linux, `~/Library/Caches/mega-evme/rpc` on macOS). Pass `--rpc.no-cache-file`
    /// to disable on-disk persistence entirely. Batch replay (`--tx-file` / `--block`)
    /// uses the on-disk cache only when this flag names a directory explicitly.
    #[arg(long = "rpc.cache-dir", value_parser = parse_non_empty_path)]
    pub cache_dir: Option<PathBuf>,

    /// Disable on-disk cache persistence. The in-memory LRU cache still applies.
    /// This is already the default for batch replay (`--tx-file` / `--block`)
    /// unless `--rpc.cache-dir` is passed.
    #[arg(long = "rpc.no-cache-file")]
    pub no_cache_file: bool,

    /// Delete the current chain's cache file before loading it. Recovery path for a
    /// polluted or corrupt cache file. If the unlink itself fails (e.g. insufficient
    /// permissions), `mega-evme` aborts rather than silently reloading the stale file.
    #[arg(long = "rpc.clear-cache")]
    pub clear_cache: bool,

    /// Maximum number of times the transport layer will retry a failing RPC request.
    /// Retries trigger on HTTP 429 / 503, JSON-RPC rate-limit error responses, and
    /// transport failures surfaced as `TransportErrorKind::Custom` (connection refused,
    /// DNS failure, TLS handshake, request timeout, etc.). Set to 0 to disable retries
    /// entirely.
    #[arg(long = "rpc.max-retries", default_value_t = 5)]
    pub max_retries: u32,

    /// Fixed sleep duration, in milliseconds, inserted between retry attempts unless the
    /// server supplies its own backoff hint. This layer does not perform exponential backoff.
    #[arg(long = "rpc.backoff-ms", default_value_t = 1_000)]
    pub backoff_ms: u64,

    /// Compute-unit budget (CU/s) for the retry layer's rate-limit accounting.
    /// This is NOT requests per second: each RPC method costs multiple compute units.
    /// A single-digit value will heavily self-throttle. Default (660) matches typical
    /// public-endpoint budgets.
    #[arg(long = "rpc.cu-per-sec", visible_alias = "rpc.rate-limit", default_value_t = 660)]
    pub compute_units_per_sec: u64,

    /// Total per-HTTP-request timeout in seconds (connect + response).
    /// `0` disables the timeout (previous behavior: a hung endpoint can block forever).
    /// A non-zero timeout surfaces a hung endpoint as a retryable transport error.
    #[arg(long = "rpc.request-timeout", default_value_t = 30)]
    pub request_timeout: u64,
}

impl RpcArgs {
    /// Build an RPC provider using `--rpc <URL>` with the standard `--rpc.cache-dir` path.
    ///
    /// Used by `replay` (online), `run --fork`, and `tx --fork`. Requires `rpc_url` to
    /// be `Some` — callers validate this before calling. For offline replay and capture
    /// mode, use [`Self::build_replay_provider`] and [`Self::build_capture_provider`].
    pub async fn build_provider(&self) -> Result<BuildProviderOutput> {
        let rpc_url_str = self.rpc_url.as_deref().ok_or_else(|| {
            EvmeError::InvalidInput("No RPC URL provided. Pass '--rpc <URL>'.".to_string())
        })?;

        let url: reqwest::Url = rpc_url_str.parse().map_err(|e| {
            EvmeError::InvalidInput(format!("Invalid RPC URL '{}': {}", rpc_url_str, e))
        })?;

        // Once per provider build (not per client: resolve_chain_id also builds a client).
        self.maybe_warn_low_cu_per_sec();

        // 1. Resolve chain id (always needed by downstream consumers).
        let chain_id = self.resolve_chain_id(url.clone()).await?;

        // 2. Resolve on-disk cache path (None when disk persistence is disabled).
        let cache_path = if self.no_cache_file {
            None
        } else {
            Some(resolve_cache_path(self.cache_dir.as_deref(), chain_id)?)
        };

        // 3. Build the cache layer and (optionally) the disk store.
        // Cache layer is always installed; 0 max entries maps to
        // EFFECTIVELY_UNLIMITED_CACHE_ENTRIES.
        let max_items = cache_max_entries_capacity(self.cache_max_entries);
        let cache_layer = CacheLayer::new(max_items);
        let cache = cache_layer.cache();
        let cache_store = match cache_path {
            Some(path) => {
                if self.clear_cache {
                    // Same sidecar lock as persist / `cache merge`: without it a
                    // writer mid re-read-merge-rename can land after the unlink
                    // (undoing the clear), or the clear can delete a file the
                    // locked writer just re-read. Fail closed if the lock cannot
                    // be acquired — the user asked for a deletion that is not
                    // safe to do unlocked.
                    let _clear_lock = acquire_exclusive_lock(&path).map_err(|e| {
                        EvmeError::RpcError(format!(
                            "Failed to acquire the cache lock {} for clear-cache of {}: {e}. \
                             Refusing to clear without it: a concurrent writer could race \
                             the unlink and silently recreate or rely on the file.",
                            lock_sidecar_path(&path).display(),
                            path.display(),
                        ))
                    })?;
                    if let Err(e) = fs::remove_file(&path) {
                        if e.kind() != std::io::ErrorKind::NotFound {
                            return Err(EvmeError::RpcError(format!(
                                "Failed to clear RPC cache at {}: {e}",
                                path.display(),
                            )));
                        }
                    } else {
                        info!(path = %path.display(), "Cleared existing RPC cache");
                    }
                    // Lock released on drop before load / provider build continue.
                }
                if let Some(parent) = path.parent() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        warn!(
                            path = %parent.display(),
                            error = %e,
                            "Failed to create cache directory; persist may fail",
                        );
                    }
                }
                if path.exists() {
                    // Oversized-load warning is intentionally omitted: alloy's
                    // `SharedCache` has no public entry-count API (`len` / loaded-count),
                    // so a file-with-N-entries-over-cap warning cannot be emitted
                    // without re-implementing load or file-size heuristics (rejected).
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

        // 4. Build the cached provider.
        let client = self.build_retry_client(url);
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .layer(cache_layer)
            .network::<op_alloy_network::Optimism>()
            .connect_client(client);

        info!(
            rpc_url = %rpc_url_str,
            cache_max_entries = self.cache_max_entries,
            cache_capacity = max_items,
            max_retries = self.max_retries,
            backoff_ms = self.backoff_ms,
            "Built RPC provider",
        );

        Ok(BuildProviderOutput {
            provider: DynProvider::new(provider),
            cache_store,
            chain_id,
            external_env: None,
        })
    }

    /// Build the provider in replay mode (`--rpc.replay-file` without `--rpc`).
    ///
    /// Loads the envelope's transport-level cache and builds the provider over
    /// [`ReplayTransport`], which serves cached responses directly and returns
    /// a descriptive error on cache miss. No `CacheLayer` is used — all caching
    /// is at the transport level so every RPC method is covered.
    pub async fn build_replay_provider(&self) -> Result<BuildProviderOutput> {
        let path = self.replay_file.as_ref().expect("replay mode requires --rpc.replay-file");

        let envelope = CacheFileEnvelope::load(path)?;
        let chain_id = envelope.chain_id;
        debug!(
            path = %path.display(),
            chain_id,
            has_external_env = envelope.external_env.is_some(),
            "Loaded replay envelope",
        );

        // Load the transport-level cache from the envelope.
        let transport_cache = TransportCache::from_value(&envelope.cache)?;
        debug!(entries = transport_cache.len(), "Seeded transport cache from envelope");

        // Build provider over ReplayTransport — serves from cache, BackendGone on miss.
        let replay_client = ClientBuilder::default()
            .transport(ReplayTransport::new(path.clone(), transport_cache), true);
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<op_alloy_network::Optimism>()
            .connect_client(replay_client);

        info!(
            path = %path.display(),
            chain_id,
            "Built RPC provider (replay from cache file)",
        );

        Ok(BuildProviderOutput {
            provider: DynProvider::new(provider),
            cache_store: RpcCacheStore::noop(),
            chain_id,
            external_env: envelope.external_env,
        })
    }

    /// Build the provider in capture mode (`--rpc.capture-file` + `--rpc`).
    ///
    /// If the capture file already exists, its entries are loaded into the transport-
    /// level cache. Missing entries are fetched via the HTTP transport (with retry),
    /// cached in-memory, and persisted as an envelope on clean exit. No `CacheLayer`
    /// is used — all caching is at the transport level so every RPC method is covered.
    pub async fn build_capture_provider(&self) -> Result<BuildProviderOutput> {
        let path = self.capture_file.as_ref().expect("capture mode requires --rpc.capture-file");
        let rpc_url_str = self.rpc_url.as_ref().expect("capture mode requires --rpc");

        let url: reqwest::Url = rpc_url_str.parse().map_err(|e| {
            EvmeError::InvalidInput(format!("Invalid RPC URL '{}': {}", rpc_url_str, e))
        })?;

        // Once per provider build (capture builds a single client; keep the same entry point).
        self.maybe_warn_low_cu_per_sec();

        // Load existing envelope if the file exists.
        let existing_envelope = if path.exists() {
            let env = CacheFileEnvelope::load(path)?;
            debug!(
                path = %path.display(),
                chain_id = env.chain_id,
                has_external_env = env.external_env.is_some(),
                "Found existing capture envelope; entries will merge after chain-id validation",
            );
            Some(env)
        } else {
            debug!(path = %path.display(), "No existing capture envelope; starting fresh");
            None
        };

        // Start with an empty transport cache so the eth_chainId call below
        // always hits the real endpoint — if we seeded from the existing
        // envelope first, a stale eth_chainId entry would short-circuit the
        // cross-chain validation.
        let transport_cache = TransportCache::new();
        let http = self.build_http_transport(url.clone());
        let caching = CachingTransport::new(http, transport_cache.clone());
        let client = self.build_client(caching, &url);
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<op_alloy_network::Optimism>()
            .connect_client(client);

        // Resolve chain_id through the caching provider (hits network, response captured).
        let chain_id = provider.get_chain_id().await.map_err(|e| {
            EvmeError::RpcError(format!("Failed to fetch chain ID from '{}': {e}", rpc_url_str))
        })?;

        // Validate chain_id match, then merge existing entries into the cache.
        if let Some(ref env) = existing_envelope {
            if env.chain_id != chain_id {
                return Err(EvmeError::RpcError(format!(
                    "Chain ID mismatch: cache file {} contains chain {} but endpoint returned {}",
                    path.display(),
                    env.chain_id,
                    chain_id,
                )));
            }
            transport_cache.merge(&env.cache)?;
            debug!(
                entries = transport_cache.len(),
                "Merged existing envelope entries into capture cache",
            );
        }

        info!(
            rpc_url = %rpc_url_str,
            path = %path.display(),
            chain_id,
            "Built RPC provider (capture to cache file)",
        );

        // Same snapshot whose entries were merged above — carry it into the
        // store as the OCC load-time baseline. Do not re-read the file: a
        // concurrent writer between this load and store construction would
        // make the later write treat C as baseline and silently overwrite it.
        let prev_external_env = existing_envelope.and_then(|e| e.external_env);

        Ok(BuildProviderOutput {
            provider: DynProvider::new(provider),
            cache_store: RpcCacheStore::new_envelope(
                transport_cache,
                path.clone(),
                chain_id,
                prev_external_env.clone(),
            ),
            chain_id,
            external_env: prev_external_env,
        })
    }

    /// Build the HTTP transport, applying [`Self::request_timeout`] when non-zero.
    ///
    /// All networked `Http` construction (standard provider, capture provider, and
    /// the throwaway chain-id probe) must go through this helper so the timeout is
    /// applied uniformly. Offline replay (`--rpc.replay-file`) never constructs
    /// an HTTP transport.
    ///
    /// When `request_timeout == 0`, uses the default reqwest client (no total
    /// request timeout). When non-zero, builds a client with both `.timeout` and
    /// `.connect_timeout` set to the same duration so a hung endpoint surfaces as
    /// a `TransportErrorKind::Custom` error (retryable under the policy below)
    /// instead of hanging the process forever.
    fn build_http_transport(
        &self,
        url: reqwest::Url,
    ) -> alloy_transport_http::Http<reqwest::Client> {
        if self.request_timeout == 0 {
            return alloy_transport_http::Http::new(url);
        }
        let timeout = Duration::from_secs(self.request_timeout);
        // `Client::builder` with only timeout settings cannot fail under normal
        // conditions (failure requires a broken TLS backend).
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .expect("reqwest Client with timeout settings must build");
        alloy_transport_http::Http::with_client(client, url)
    }

    /// Build an `RpcClient` over HTTP, wired with the configured retry layer.
    fn build_retry_client(&self, url: reqwest::Url) -> RpcClient {
        self.build_client(self.build_http_transport(url.clone()), &url)
    }

    /// Build an `RpcClient` over an arbitrary transport, wired with the configured
    /// retry layer (or bare when `max_retries == 0`). `url` is used only to detect
    /// whether the endpoint is local.
    ///
    /// Retry coverage: `RateLimitRetryPolicy` handles HTTP 429/503 and JSON-RPC
    /// rate-limit bodies. The `TransportErrorKind::Custom` predicate additionally
    /// retries transport failures, including reqwest request timeouts (mapped by
    /// `alloy-transport-http` via `TransportErrorKind::custom`).
    fn build_client<T: alloy_transport::IntoBoxTransport>(
        &self,
        transport: T,
        url: &reqwest::Url,
    ) -> RpcClient {
        let is_local =
            url.host_str().is_some_and(|h| h == "localhost" || h == "127.0.0.1" || h == "::1");
        if self.max_retries > 0 {
            // Reqwest timeouts, connection refused, DNS failure, TLS handshake,
            // etc. all arrive as `TransportErrorKind::Custom` and are retryable.
            let policy = RateLimitRetryPolicy::default().or(|err: &TransportError| {
                matches!(err, RpcError::Transport(TransportErrorKind::Custom(_)))
            });
            let retry = RetryBackoffLayer::new_with_policy(
                self.max_retries,
                self.backoff_ms,
                self.compute_units_per_sec,
                policy,
            );
            ClientBuilder::default().layer(retry).transport(transport, is_local)
        } else {
            ClientBuilder::default().transport(transport, is_local)
        }
    }

    /// Emit the low CU/s warning at most once per networked provider build.
    ///
    /// Not placed in [`Self::build_client`]: the standard path builds a throwaway
    /// client for chain-id resolution and then the real client, so a warning there
    /// would fire twice.
    fn maybe_warn_low_cu_per_sec(&self) {
        if let Some(msg) = cu_per_sec_warning(self.max_retries, self.compute_units_per_sec) {
            warn!("{msg}");
        }
    }

    /// Resolve the chain ID by issuing `eth_chainId` against a throwaway
    /// cache-less provider using the configured retry policy.
    async fn resolve_chain_id(&self, url: reqwest::Url) -> Result<u64> {
        let url_str = url.as_str().to_string();
        let bare = build_bare_op_provider(self.build_retry_client(url));
        bare.get_chain_id().await.map_err(|e| {
            EvmeError::RpcError(format!("Failed to fetch chain ID from '{}': {}", url_str, e))
        })
    }
}

/// Threshold below which a configured CU/s budget is considered dangerously low.
/// Values under this with retries enabled produce a one-shot warning at provider build.
const CU_PER_SEC_WARN_THRESHOLD: u64 = 100;

/// Return a warning message when the retry layer is enabled and the CU/s budget is
/// below [`CU_PER_SEC_WARN_THRESHOLD`]. Used so the trigger rule is unit-testable
/// without capturing log output.
fn cu_per_sec_warning(max_retries: u32, compute_units_per_sec: u64) -> Option<String> {
    (max_retries > 0 && compute_units_per_sec < CU_PER_SEC_WARN_THRESHOLD).then(|| {
        format!(
            "--rpc.cu-per-sec is set to {compute_units_per_sec}, which is a compute-unit \
             budget (CU/s) for the retry layer's rate-limit accounting, NOT requests per \
             second; such a low budget will heavily self-throttle RPC traffic"
        )
    })
}

/// `clap` value parser that rejects empty and whitespace-only path arguments
/// at parse time.
///
/// Without this check, `--rpc.cache-dir ""` would pass clap, land in
/// [`resolve_cache_path`] as `PathBuf::from("")`, and silently write the
/// per-chain file to the process's current working directory — a surprising
/// footgun, since the same command run from different cwds would produce
/// different cache files with no visible indication.
fn parse_non_empty_path(s: &str) -> std::result::Result<PathBuf, String> {
    if s.trim().is_empty() {
        Err("path must not be empty".to_string())
    } else {
        Ok(PathBuf::from(s))
    }
}

/// Cap used when `--rpc.cache-max-entries 0` ("effectively unlimited") is requested.
///
/// Alloy's `SharedCache` preallocates its LRU hash table to full capacity
/// (`lru::LruCache::with_hasher` → `HashMap::with_capacity_and_hasher`), so a true
/// `u32::MAX` mapping would preallocate ~2^33 hash buckets and balloon RSS by multi-GB
/// on every default-config online run. 2^20 entries preallocates ~2^21 pointer-sized
/// buckets (tens of MB) while covering ~2,600 blocks' worth of RPC entries per process
/// (a full mainnet block is ~200 entries; the largest real merged corpus to date was
/// 15,294).
const EFFECTIVELY_UNLIMITED_CACHE_ENTRIES: u32 = 1_048_576;

/// Map `--rpc.cache-max-entries` to the capacity passed to [`CacheLayer::new`].
///
/// `0` means effectively unlimited and is approximated by
/// [`EFFECTIVELY_UNLIMITED_CACHE_ENTRIES`] (see that constant's rationale).
/// Nonzero values pass through unchanged.
fn cache_max_entries_capacity(cache_max_entries: u32) -> u32 {
    if cache_max_entries == 0 {
        EFFECTIVELY_UNLIMITED_CACHE_ENTRIES
    } else {
        cache_max_entries
    }
}

/// Build a cache-less [`OpProvider`] from an already-configured `RpcClient`.
///
/// Used by the throwaway chain-id fetch. The standard path builds its provider
/// inline because the cache layer has to be inserted into the `ProviderBuilder`
/// chain before the client is attached.
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

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

    /// Warn when retries are on and CU/s is below the threshold.
    #[test]
    fn test_cu_per_sec_warning_fires_below_threshold_with_retries() {
        let msg = cu_per_sec_warning(5, 99).expect("should warn at 99 with retries on");
        assert!(msg.contains("99"), "message should include the configured value: {msg}");
        assert!(msg.contains("NOT requests per second") || msg.contains("NOT requests"), "{msg}");
        assert!(msg.contains("self-throttle"), "{msg}");
    }

    /// Silent at the threshold boundary (100) and at the production default (660).
    #[test]
    fn test_cu_per_sec_warning_silent_at_or_above_threshold() {
        assert!(cu_per_sec_warning(5, 100).is_none());
        assert!(cu_per_sec_warning(5, 660).is_none());
    }

    /// Silent when the retry layer is disabled, even with a low CU/s budget.
    #[test]
    fn test_cu_per_sec_warning_silent_when_retries_disabled() {
        assert!(cu_per_sec_warning(0, 1).is_none());
        assert!(cu_per_sec_warning(0, 99).is_none());
    }

    /// `0` maps to the effectively-unlimited cap; nonzero values pass through.
    #[test]
    fn test_cache_max_entries_capacity_mapping() {
        assert_eq!(cache_max_entries_capacity(0), EFFECTIVELY_UNLIMITED_CACHE_ENTRIES);
        assert_eq!(cache_max_entries_capacity(0), 1_048_576);
        assert_eq!(cache_max_entries_capacity(1), 1);
        assert_eq!(cache_max_entries_capacity(256), 256);
        assert_eq!(cache_max_entries_capacity(10_000), 10_000);
        assert_eq!(cache_max_entries_capacity(u32::MAX), u32::MAX);
    }

    /// Construction-reality check: actually allocate the cache at the mapped "unlimited"
    /// capacity, insert, and read back. Preallocating `u32::MAX` would hang/OOM here
    /// (hashbrown ctrl-byte memset over ~2^33 buckets); this test must complete quickly.
    #[test]
    fn test_cache_layer_constructs_at_effectively_unlimited_capacity() {
        let layer = CacheLayer::new(cache_max_entries_capacity(0));
        assert_eq!(layer.max_items(), EFFECTIVELY_UNLIMITED_CACHE_ENTRIES);

        let cache = layer.cache();
        assert_eq!(cache.max_items(), EFFECTIVELY_UNLIMITED_CACHE_ENTRIES);

        let key = alloy_primitives::B256::repeat_byte(0xab);
        let value = r#"{"result":"0x1"}"#.to_string();
        cache.put(key, value.clone()).expect("put into SharedCache");
        assert_eq!(cache.get(&key).as_deref(), Some(value.as_str()));
    }
}
