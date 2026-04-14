//! RPC provider factory and on-disk cache store for mega-evme.
//!
//! Three provider builders for different use cases:
//!
//! - [`RpcArgs::build_provider`] — standard RPC with optional `--rpc.cache-dir` persistence.
//! - [`RpcArgs::build_replay_provider`] — offline replay from a `--rpc.cache-file` envelope.
//! - [`RpcArgs::build_capture_provider`] — RPC with transport-level caching to `--rpc.cache-file`.
//!
//! The `--rpc.cache-dir` path uses alloy's provider-level `CacheLayer` (caches ~8 methods).
//! The `--rpc.cache-file` paths use a transport-level `CachingTransport` / `ReplayTransport`
//! that captures all JSON-RPC request/response pairs.

use std::{
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_primitives::{keccak256, B256};
use alloy_provider::{
    layers::{CacheLayer, SharedCache},
    transport::{
        layers::{RateLimitRetryPolicy, RetryBackoffLayer},
        RpcError, TransportError, TransportErrorKind,
    },
    DynProvider, Provider, ProviderBuilder,
};
use alloy_rpc_client::{ClientBuilder, RpcClient};
use alloy_transport::TransportFut;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{EvmeError, Result};

/// OP-stack provider type used throughout mega-evme.
pub type OpProvider = DynProvider<op_alloy_network::Optimism>;

/// Return value of the `RpcArgs::build_*_provider` methods.
#[derive(Debug)]
pub struct BuildProviderOutput {
    /// Configured OP-stack provider. Already wrapped with the retry layer and (unless
    /// the cache is disabled) the in-memory cache layer.
    pub provider: OpProvider,
    /// Clean-exit cache persistence handle. Call [`RpcCacheStore::persist`] on the
    /// success path; no-op when the cache is disabled.
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

    /// Single-file RPC cache and fixture.
    /// Combined with --rpc: load if present, fetch missing entries, persist on clean exit.
    /// Without --rpc: read-only replay; any RPC miss is a hard error; file is never written.
    /// Cannot be used with --rpc.cache-dir, --rpc.clear-cache,
    /// --rpc.no-cache-file, or --rpc.cache-size.
    #[arg(
        long = "rpc.cache-file",
        value_parser = parse_non_empty_path,
        conflicts_with_all = ["cache_dir", "clear_cache", "no_cache_file", "cache_size"],
    )]
    pub cache_file: Option<PathBuf>,

    /// Maximum number of items to keep in the in-memory RPC LRU cache.
    /// Set to 0 to disable the cache layer entirely.
    #[arg(id = "cache_size", long = "rpc.cache-size", default_value_t = 10_000)]
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
            EvmeError::RpcError(format!("Invalid RPC URL '{}': {}", rpc_url_str, e))
        })?;

        // 1. Resolve chain id (always needed by downstream consumers).
        let chain_id = self.resolve_chain_id(url.clone()).await?;

        // 2. Fast path: cache fully disabled.
        if self.cache_size == 0 {
            let provider = build_bare_op_provider(self.build_retry_client(url));
            info!(
                rpc_url = %rpc_url_str,
                max_retries = self.max_retries,
                backoff_ms = self.backoff_ms,
                "Built RPC provider (cache disabled)",
            );
            return Ok(BuildProviderOutput {
                provider,
                cache_store: RpcCacheStore::noop(),
                chain_id,
                external_env: None,
            });
        }

        // 3. Resolve on-disk cache path (None when disk persistence is disabled).
        let cache_path = if self.no_cache_file {
            None
        } else {
            Some(resolve_cache_path(self.cache_dir.as_deref(), chain_id)?)
        };

        // 4. Build the cache layer and (optionally) the disk store.
        let cache_layer = CacheLayer::new(self.cache_size);
        let cache = cache_layer.cache();
        let cache_store = match cache_path {
            Some(path) => {
                if self.clear_cache {
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

        // 5. Build the cached provider.
        let client = self.build_retry_client(url);
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .layer(cache_layer)
            .network::<op_alloy_network::Optimism>()
            .connect_client(client);

        info!(
            rpc_url = %rpc_url_str,
            cache_size = self.cache_size,
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

    /// Build the provider in replay mode (`--rpc.cache-file` without `--rpc`).
    ///
    /// Loads the envelope's transport-level cache and builds the provider over
    /// [`ReplayTransport`], which serves cached responses directly and returns
    /// a descriptive error on cache miss. No `CacheLayer` is used — all caching
    /// is at the transport level so every RPC method is covered.
    pub async fn build_replay_provider(&self) -> Result<BuildProviderOutput> {
        let path = self.cache_file.as_ref().expect("replay mode requires --rpc.cache-file");

        let envelope = load_envelope(path)?;
        let chain_id = envelope.chain_id;

        // Load the transport-level cache from the envelope.
        let transport_cache = TransportCache::from_value(&envelope.cache)?;

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

    /// Build the provider in capture mode (`--rpc.cache-file` + `--rpc`).
    ///
    /// If the cache file already exists, its entries are loaded into the transport-
    /// level cache. Missing entries are fetched via the HTTP transport (with retry),
    /// cached in-memory, and persisted as an envelope on clean exit. No `CacheLayer`
    /// is used — all caching is at the transport level so every RPC method is covered.
    pub async fn build_capture_provider(&self) -> Result<BuildProviderOutput> {
        let path = self.cache_file.as_ref().expect("capture mode requires --rpc.cache-file");
        let rpc_url_str = self.rpc_url.as_ref().expect("capture mode requires --rpc");

        let url: reqwest::Url = rpc_url_str.parse().map_err(|e| {
            EvmeError::RpcError(format!("Invalid RPC URL '{}': {}", rpc_url_str, e))
        })?;

        // Load existing envelope if the file exists.
        let existing_envelope = if path.exists() { Some(load_envelope(path)?) } else { None };

        // Start with an empty transport cache so the eth_chainId call below
        // always hits the real endpoint — if we seeded from the existing
        // envelope first, a stale eth_chainId entry would short-circuit the
        // cross-chain validation.
        let transport_cache = TransportCache::new();
        let http = alloy_transport_http::Http::new(url.clone());
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
        }

        info!(
            rpc_url = %rpc_url_str,
            path = %path.display(),
            chain_id,
            "Built RPC provider (capture to cache file)",
        );

        let prev_external_env = existing_envelope.and_then(|e| e.external_env);

        Ok(BuildProviderOutput {
            provider: DynProvider::new(provider),
            cache_store: RpcCacheStore::new_envelope(transport_cache, path.clone(), chain_id),
            chain_id,
            external_env: prev_external_env,
        })
    }

    /// Build an `RpcClient` over HTTP, wired with the configured retry layer.
    fn build_retry_client(&self, url: reqwest::Url) -> RpcClient {
        self.build_client(alloy_transport_http::Http::new(url.clone()), &url)
    }

    /// Build an `RpcClient` over an arbitrary transport, wired with the configured
    /// retry layer (or bare when `max_retries == 0`). `url` is used only to detect
    /// whether the endpoint is local.
    fn build_client<T: alloy_transport::IntoBoxTransport>(
        &self,
        transport: T,
        url: &reqwest::Url,
    ) -> RpcClient {
        let is_local =
            url.host_str().is_some_and(|h| h == "localhost" || h == "127.0.0.1" || h == "::1");
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
            ClientBuilder::default().layer(retry).transport(transport, is_local)
        } else {
            ClientBuilder::default().transport(transport, is_local)
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

/// Discriminated inner state of [`RpcCacheStore`].
enum RpcCacheStoreInner {
    /// Legacy per-chain raw alloy cache (`--rpc.cache-dir`).
    Raw { cache: SharedCache, path: PathBuf },
    /// Transport-level envelope cache (`--rpc.cache-file`).
    Envelope { cache: TransportCache, path: PathBuf, chain_id: u64 },
}

impl RpcCacheStore {
    /// Construct a store backed by a raw (per-chain) alloy cache file.
    pub(crate) fn new(cache: SharedCache, cache_path: PathBuf) -> Self {
        Self { inner: Some(RpcCacheStoreInner::Raw { cache, path: cache_path }) }
    }

    /// Construct a store backed by a transport-level envelope cache file.
    pub(crate) fn new_envelope(cache: TransportCache, path: PathBuf, chain_id: u64) -> Self {
        Self { inner: Some(RpcCacheStoreInner::Envelope { cache, path, chain_id }) }
    }

    /// Construct a no-op store.
    pub(crate) fn noop() -> Self {
        Self { inner: None }
    }

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

    /// Returns the underlying [`SharedCache`] (raw path only), or `None`.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache(&self) -> Option<&SharedCache> {
        match &self.inner {
            Some(RpcCacheStoreInner::Raw { cache, .. }) => Some(cache),
            _ => None,
        }
    }

    /// Returns the resolved cache file path, or `None` for a no-op store.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache_path(&self) -> Option<&Path> {
        match &self.inner {
            Some(
                RpcCacheStoreInner::Raw { path, .. } | RpcCacheStoreInner::Envelope { path, .. },
            ) => Some(path.as_path()),
            None => None,
        }
    }

    /// Persist the cache to disk atomically. **Consumes the store** to enforce
    /// "persist once, then stop" — see `Why not Drop` on this type.
    ///
    /// `external_env` is written into the envelope for `--rpc.cache-file` stores;
    /// ignored for raw (`--rpc.cache-dir`) and no-op stores.
    ///
    /// - **Raw**: best-effort — failures are warn-logged and swallowed.
    /// - **Envelope**: hard error — the fixture is the primary output of capture mode.
    /// - **No-op**: returns `Ok(())`.
    pub fn persist(self, external_env: Option<&ExternalEnvSnapshot>) -> Result<()> {
        let Some(inner) = self.inner else { return Ok(()) };
        match inner {
            RpcCacheStoreInner::Raw { cache, path } => {
                match save_cache_atomic(&cache, &path) {
                    Ok(()) => info!(path = %path.display(), "Persisted RPC cache"),
                    Err(err) => warn!(
                        path = %path.display(),
                        error = %err,
                        "Failed to save RPC cache (continuing)",
                    ),
                }
                Ok(())
            }
            RpcCacheStoreInner::Envelope { cache, path, chain_id } => {
                let entry_count = cache.len();
                save_envelope(&cache, chain_id, external_env, &path)?;
                info!(
                    path = %path.display(),
                    entries = entry_count,
                    "Persisted RPC cache envelope",
                );
                Ok(())
            }
        }
    }
}

// Manual `Debug` because `SharedCache` does not implement `Debug`.
impl fmt::Debug for RpcCacheStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Some(
                RpcCacheStoreInner::Raw { path, .. } | RpcCacheStoreInner::Envelope { path, .. },
            ) => f.debug_struct("RpcCacheStore").field("path", path).finish_non_exhaustive(),
            None => f.debug_struct("RpcCacheStore").field("inner", &Option::<()>::None).finish(),
        }
    }
}

/// Atomically persist `cache` to `target` via a temp file + rename.
fn save_cache_atomic(cache: &SharedCache, target: &Path) -> std::io::Result<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir)?;
    let tmp_path = tmp.path().to_path_buf();

    // alloy's save_cache takes a PathBuf, not a Write.
    cache.save_cache(tmp_path).map_err(|e| std::io::Error::other(format!("{e}")))?;

    // Atomic rename. persist() consumes the NamedTempFile without deleting it.
    tmp.persist(target).map_err(|e| e.error)?;
    Ok(())
}

/// Envelope version accepted by this build.
const ENVELOPE_VERSION: u32 = 1;

/// On-disk format for `--rpc.cache-file`. Contains a transport-level cache
/// dump, chain ID, and optional external environment snapshot.
#[derive(Debug, Serialize, Deserialize)]
struct CacheFileEnvelope {
    /// Schema version (currently always 1, reserved for future format changes).
    version: u32,
    /// Chain ID at the time of capture.
    chain_id: u64,
    /// Transport-level cache entries: `[{key, value}, ...]`.
    cache: serde_json::Value,
    /// External environment inputs not derivable from RPC (e.g., SALT bucket capacities).
    #[serde(default)]
    external_env: Option<ExternalEnvSnapshot>,
}

/// Snapshot of mega-evm external environment inputs not derivable from RPC.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalEnvSnapshot {
    /// SALT bucket capacity pairs `(bucket_id, capacity)`.
    #[serde(default)]
    pub bucket_capacities: Vec<(u32, u64)>,
}

/// Read a cache-file envelope from `path`.
fn load_envelope(path: &Path) -> Result<CacheFileEnvelope> {
    let content = fs::read_to_string(path).map_err(|e| {
        EvmeError::RpcError(format!("Failed to read RPC cache file {}: {e}", path.display()))
    })?;
    let envelope: CacheFileEnvelope = serde_json::from_str(&content).map_err(|e| {
        EvmeError::RpcError(format!("Failed to parse RPC cache file {}: {e}", path.display()))
    })?;
    if envelope.version != ENVELOPE_VERSION {
        return Err(EvmeError::RpcError(format!(
            "Unsupported cache file version {} in '{}'; expected {ENVELOPE_VERSION}",
            envelope.version,
            path.display(),
        )));
    }
    Ok(envelope)
}

/// Serialize a [`TransportCache`] into an envelope and atomically write it to `path`.
fn save_envelope(
    cache: &TransportCache,
    chain_id: u64,
    external_env: Option<&ExternalEnvSnapshot>,
    path: &Path,
) -> Result<()> {
    let envelope = CacheFileEnvelope {
        version: ENVELOPE_VERSION,
        chain_id,
        cache: cache.to_value(),
        external_env: external_env.cloned(),
    };

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| {
        EvmeError::RpcError(format!("Failed to create cache file directory {}: {e}", dir.display()))
    })?;

    let serialized = serde_json::to_string_pretty(&envelope).map_err(|e| {
        EvmeError::RpcError(format!("Failed to serialize envelope for {}: {e}", path.display()))
    })?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
        EvmeError::RpcError(format!("Failed to create temp file in {}: {e}", dir.display()))
    })?;
    std::io::Write::write_all(&mut tmp, serialized.as_bytes())
        .map_err(|e| EvmeError::RpcError(format!("Failed to write envelope: {e}")))?;
    tmp.persist(path).map_err(|e| {
        EvmeError::RpcError(format!("Failed to persist envelope to {}: {e}", path.display()))
    })?;

    Ok(())
}

/// A single entry in the transport-level cache.
#[derive(Debug, Serialize, Deserialize)]
struct TransportCacheEntry {
    key: B256,
    value: String,
}

/// Transport-level RPC cache. Captures **all** JSON-RPC request/response
/// pairs, keyed by `keccak256(method + params)`.
///
/// Unlike alloy's provider-level `CacheLayer` (which only caches a subset
/// of methods it explicitly overrides), this captures every RPC call that
/// passes through the transport — making it suitable for building complete
/// offline replay fixtures.
#[derive(Debug, Clone, Default)]
pub(crate) struct TransportCache {
    entries: Arc<RwLock<HashMap<B256, String>>>,
}

impl TransportCache {
    fn new() -> Self {
        Self::default()
    }

    fn get(&self, key: &B256) -> Option<String> {
        self.entries.read().expect("cache lock poisoned").get(key).cloned()
    }

    fn put(&self, key: B256, value: String) {
        self.entries.write().expect("cache lock poisoned").insert(key, value);
    }

    fn len(&self) -> usize {
        self.entries.read().expect("cache lock poisoned").len()
    }

    /// Merge entries from a serialized cache value. Existing entries are NOT
    /// overwritten — this preserves fresh responses (e.g. `eth_chainId`)
    /// that were fetched before the merge.
    fn merge(&self, value: &serde_json::Value) -> Result<()> {
        let entries: Vec<TransportCacheEntry> =
            serde_json::from_value(value.clone()).map_err(|e| {
                EvmeError::RpcError(format!("Failed to parse transport cache entries: {e}"))
            })?;
        let mut map = self.entries.write().expect("cache lock poisoned");
        for entry in entries {
            map.entry(entry.key).or_insert(entry.value);
        }
        Ok(())
    }

    /// Serve a cached response if the key is present. Deserializes the cached
    /// JSON, fixes up the response ID, and returns a ready future. Returns
    /// `None` on cache miss.
    fn try_serve(
        &self,
        key: &B256,
        request_id: alloy_json_rpc::Id,
    ) -> Option<TransportFut<'static>> {
        let cached = self.get(key)?;
        Some(Box::pin(async move {
            let mut resp: alloy_json_rpc::Response =
                serde_json::from_str(&cached).map_err(TransportErrorKind::custom)?;
            resp.id = request_id;
            Ok(ResponsePacket::Single(resp))
        }))
    }

    /// Serialize all entries to a JSON value for the envelope.
    /// Sorted by key for deterministic output (avoids noisy diffs on committed fixtures).
    fn to_value(&self) -> serde_json::Value {
        let mut entries: Vec<TransportCacheEntry> = self
            .entries
            .read()
            .expect("cache lock poisoned")
            .iter()
            .map(|(k, v)| TransportCacheEntry { key: *k, value: v.clone() })
            .collect();
        entries.sort_by_key(|e| e.key);
        serde_json::to_value(entries).expect("TransportCacheEntry is always serializable")
    }

    /// Deserialize from the envelope's `cache` field.
    fn from_value(value: &serde_json::Value) -> Result<Self> {
        let entries: Vec<TransportCacheEntry> =
            serde_json::from_value(value.clone()).map_err(|e| {
                EvmeError::RpcError(format!("Failed to parse transport cache entries: {e}"))
            })?;
        let cache = Self::new();
        {
            let mut map = cache.entries.write().expect("cache lock poisoned");
            for entry in entries {
                map.insert(entry.key, entry.value);
            }
        }
        Ok(cache)
    }
}

/// Compute a deterministic cache key from an RPC method and params.
/// The request ID is deliberately excluded so the same logical request
/// produces the same key across runs.
fn transport_cache_key(method: &str, params: Option<&serde_json::value::RawValue>) -> B256 {
    let params_str = params.map_or("null", |p| p.get());
    keccak256(format!("{method}\x00{params_str}"))
}

/// Transport wrapper that records all JSON-RPC responses into a
/// [`TransportCache`]. Used in capture mode: cache hits are served locally,
/// misses are forwarded to the inner transport and the response is cached.
#[derive(Debug, Clone)]
struct CachingTransport<T> {
    inner: T,
    cache: TransportCache,
}

impl<T> CachingTransport<T> {
    fn new(inner: T, cache: TransportCache) -> Self {
        Self { inner, cache }
    }
}

impl<T> tower::Service<RequestPacket> for CachingTransport<T>
where
    T: tower::Service<
            RequestPacket,
            Response = ResponsePacket,
            Error = TransportError,
            Future = TransportFut<'static>,
        > + Send
        + Sync
        + Clone
        + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        if let RequestPacket::Single(ref r) = req {
            let key = transport_cache_key(r.method(), r.params());

            if let Some(fut) = self.cache.try_serve(&key, r.id().clone()) {
                return fut;
            }

            // Cache miss: forward to inner, cache successful responses.
            let cache = self.cache.clone();
            let fut = self.inner.call(req);
            return Box::pin(async move {
                let response = fut.await?;
                if let ResponsePacket::Single(ref resp) = response {
                    if let Ok(serialized) = serde_json::to_string(resp) {
                        cache.put(key, serialized);
                    }
                }
                Ok(response)
            });
        }

        // Batch: forward without caching. alloy's default provider never batches,
        // so uncached batch responses won't cause offline replay failures in practice.
        self.inner.call(req)
    }
}

/// A transport that serves RPC responses from a [`TransportCache`] and
/// returns `BackendGone` on cache miss. Used for offline replay — no
/// network I/O, no retry layer.
#[derive(Debug, Clone)]
struct ReplayTransport {
    cache_file_path: PathBuf,
    cache: TransportCache,
}

impl ReplayTransport {
    fn new(cache_file_path: PathBuf, cache: TransportCache) -> Self {
        Self { cache_file_path, cache }
    }
}

impl tower::Service<RequestPacket> for ReplayTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        if let RequestPacket::Single(ref r) = req {
            let key = transport_cache_key(r.method(), r.params());

            if let Some(fut) = self.cache.try_serve(&key, r.id().clone()) {
                return fut;
            }

            // Cache miss: build a descriptive error so the user knows which RPC
            // response is missing and how to refresh the fixture.
            let path = self.cache_file_path.display().to_string();
            let method = r.method().to_string();
            let params = r.params().map_or_else(|| "null".to_string(), |p| p.get().to_string());
            let msg = format!(
                "cache miss in offline replay file '{path}': method={method}, params={params}\n\
                 hint: re-capture with `mega-evme replay <tx> --rpc <URL> --rpc.cache-file {path}`"
            );
            // Custom error so the message propagates. Safe: retry layer is never
            // installed on the replay path.
            return Box::pin(async move { Err(TransportErrorKind::custom_str(&msg)) });
        }

        // Batch miss: BackendGone (not Custom, avoids retry).
        Box::pin(async { Err(TransportErrorKind::backend_gone()) })
    }
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

    // ─── Envelope I/O tests ─────────────────────────────────────────────

    /// Save a cache as an envelope, load it back, and verify the round-trip
    /// preserves version, `chain_id`, cache payload, and `external_env`.
    #[test]
    fn test_envelope_roundtrip_preserves_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test-cache.json");

        let cache = TransportCache::new();
        cache.put(
            keccak256("eth_blockNumber"),
            r#"{"id":0,"jsonrpc":"2.0","result":"0x1"}"#.to_string(),
        );

        let ext = ExternalEnvSnapshot { bucket_capacities: vec![(1, 100), (2, 200)] };
        save_envelope(&cache, 4326, Some(&ext), &path).expect("save_envelope");

        let envelope = load_envelope(&path).expect("load_envelope");
        assert_eq!(envelope.version, 1);
        assert_eq!(envelope.chain_id, 4326);
        assert!(envelope.cache.is_array(), "cache should be a JSON array");

        // Verify the cache entry survived the round-trip.
        let loaded = TransportCache::from_value(&envelope.cache).expect("from_value");
        assert!(loaded.get(&keccak256("eth_blockNumber")).is_some());

        let env = envelope.external_env.expect("external_env should round-trip");
        assert_eq!(env.bucket_capacities, vec![(1, 100), (2, 200)]);
    }

    /// An envelope with an unrecognized version must be rejected so that a
    /// future format change doesn't silently produce wrong results.
    #[test]
    fn test_envelope_rejects_unknown_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("v2.json");
        fs::write(&p, r#"{"version":2,"chain_id":1,"cache":[]}"#).unwrap();
        let err = load_envelope(&p).expect_err("version 2 should be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("Unsupported"), "error should mention Unsupported: {msg}");
    }

    /// `load_envelope` must reject envelopes missing required fields.
    #[test]
    fn test_envelope_rejects_missing_fields() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Missing chain_id.
        let p1 = dir.path().join("no-chain.json");
        fs::write(&p1, r#"{"version":1,"cache":{}}"#).unwrap();
        let err = load_envelope(&p1).expect_err("missing chain_id");
        let msg = format!("{err}");
        assert!(msg.contains("parse"), "error should mention parse: {msg}");

        // Missing cache.
        let p2 = dir.path().join("no-cache.json");
        fs::write(&p2, r#"{"version":1,"chain_id":1}"#).unwrap();
        let err = load_envelope(&p2).expect_err("missing cache");
        let msg = format!("{err}");
        assert!(msg.contains("parse"), "error should mention parse: {msg}");

        // Missing version.
        let p3 = dir.path().join("no-version.json");
        fs::write(&p3, r#"{"chain_id":1,"cache":{}}"#).unwrap();
        let err = load_envelope(&p3).expect_err("missing version");
        let msg = format!("{err}");
        assert!(msg.contains("parse"), "error should mention parse: {msg}");
    }

    // ─── ReplayTransport tests ──────────────────────────────────────────

    /// The replay transport is always ready.
    /// Single-request cache misses return a descriptive Custom error (safe because
    /// the retry layer is never installed on the replay path).
    /// Batch misses return `BackendGone`.
    #[tokio::test]
    async fn test_replay_transport_cache_miss() {
        use tower::Service;

        let mut transport =
            ReplayTransport::new(PathBuf::from("/tmp/test.cache.json"), TransportCache::new());

        // poll_ready should be Ready(Ok(())).
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(transport.poll_ready(&mut cx).is_ready());

        // Single request: cache miss should include method and fixture path.
        let req =
            alloy_json_rpc::Request::new("eth_blockNumber", alloy_json_rpc::Id::Number(1), ())
                .serialize()
                .expect("serialize request");
        let result = transport.call(RequestPacket::Single(req)).await;
        assert!(result.is_err(), "cache miss must error");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("cache miss"), "error should say 'cache miss': {msg}");
        assert!(msg.contains("eth_blockNumber"), "error should include method: {msg}");
        assert!(msg.contains("test.cache.json"), "error should include fixture path: {msg}");
    }
}
