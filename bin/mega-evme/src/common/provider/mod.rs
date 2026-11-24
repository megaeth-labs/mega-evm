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

    /// Capture JSON-RPC responses to a single file for later offline replay.
    /// Requires `--rpc`.
    /// If the file already exists, its entries are loaded and merged;
    /// missing entries are fetched via the RPC endpoint and persisted on clean exit.
    /// Cannot be used with --rpc.replay-file, --rpc.cache-dir, --rpc.clear-cache,
    /// --rpc.no-cache-file, or --rpc.cache-size.
    #[arg(
        long = "rpc.capture-file",
        value_parser = parse_non_empty_path,
        requires = "rpc_url",
        conflicts_with_all = ["replay_file", "cache_dir", "clear_cache", "no_cache_file", "cache_size"],
    )]
    pub capture_file: Option<PathBuf>,

    /// Replay from a previously captured JSON-RPC fixture file (offline).
    /// Cannot be used with `--rpc`.
    /// Any RPC miss is a hard error; the file is never written.
    /// Cannot be used with --rpc.capture-file, --rpc.cache-dir, --rpc.clear-cache,
    /// --rpc.no-cache-file, or --rpc.cache-size.
    #[arg(
        long = "rpc.replay-file",
        value_parser = parse_non_empty_path,
        conflicts_with_all = ["rpc_url", "capture_file", "cache_dir", "clear_cache", "no_cache_file", "cache_size"],
    )]
    pub replay_file: Option<PathBuf>,

    /// Maximum number of items to keep in the in-memory RPC LRU cache.
    /// Set to 0 to disable the cache layer entirely.
    #[arg(id = "cache_size", long = "rpc.cache-size", default_value_t = 10_000)]
    pub cache_size: u32,

    /// Directory for per-endpoint RPC cache files.
    ///
    /// Each endpoint of a chain is stored as `{cache_dir}/rpc-cache-{chain_id}-{endpoint}.json`,
    /// where `{endpoint}` is a short hash of the URL with any userinfo removed. The name does
    /// not contain the URL, and two endpoints of one chain do not share a file.
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

        let url = parse_rpc_url(rpc_url_str)?;

        // 1. Resolve chain id (always needed by downstream consumers).
        let chain_id = self.resolve_chain_id(url.clone()).await?;

        // 2. Fast path: cache fully disabled. A clear still deletes the on-disk file: leaving
        // it would reload the wiped entries the next time the cache is turned back on.
        if self.cache_size == 0 {
            if self.clear_cache && !self.no_cache_file {
                let path = resolve_cache_path(self.cache_dir.as_deref(), chain_id, rpc_url_str)?;
                clear_cache_file(&path)?;
            }
            let provider = build_bare_op_provider(self.build_retry_client(url));
            info!(
                rpc_url = %redact_rpc_url(rpc_url_str),
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
            Some(resolve_cache_path(self.cache_dir.as_deref(), chain_id, rpc_url_str)?)
        };

        // 4. Build the cache layer and (optionally) the disk store.
        let cache_layer = CacheLayer::new(self.cache_size);
        let cache = cache_layer.cache();
        let cache_store = match cache_path {
            Some(path) => {
                if self.clear_cache {
                    clear_cache_file(&path)?;
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
            rpc_url = %redact_rpc_url(rpc_url_str),
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

    /// A provider for reading what decides a command's engine (the chain id, and a block's
    /// timestamp), over the same source the command reads, without a cache that could be
    /// persisted: the replay file when one is given, otherwise the RPC endpoint directly.
    pub async fn build_lookup_provider(&self) -> Result<(OpProvider, u64)> {
        if self.replay_file.is_some() {
            let BuildProviderOutput { provider, chain_id, .. } =
                self.build_replay_provider().await?;
            return Ok((provider, chain_id));
        }
        let rpc_url_str = self.rpc_url.as_deref().ok_or_else(|| {
            EvmeError::InvalidInput(
                "'mega-evme replay' requires '--rpc <URL>', '--rpc.capture-file <PATH>', or \
                 '--rpc.replay-file <PATH>'"
                    .to_string(),
            )
        })?;
        let url = parse_rpc_url(rpc_url_str)?;
        let chain_id = self.resolve_chain_id(url.clone()).await?;
        Ok((build_bare_op_provider(self.build_retry_client(url)), chain_id))
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

        let url = parse_rpc_url(rpc_url_str)?;

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
        let http = alloy_transport_http::Http::new(url.clone());
        let caching = CachingTransport::new(http, transport_cache.clone());
        let client = self.build_client(caching, &url);
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .network::<op_alloy_network::Optimism>()
            .connect_client(client);

        // Resolve chain_id through the caching provider (hits network, response captured).
        let chain_id = provider.get_chain_id().await.map_err(|e| {
            EvmeError::RpcError(format!(
                "Failed to fetch chain ID from '{}': {e}",
                redact_rpc_url(rpc_url_str)
            ))
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
            rpc_url = %redact_rpc_url(rpc_url_str),
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
    fn build_retry_client(&self, url: alloy_transport_http::reqwest::Url) -> RpcClient {
        self.build_client(alloy_transport_http::Http::new(url.clone()), &url)
    }

    /// Build an `RpcClient` over an arbitrary transport, wired with the configured
    /// retry layer (or bare when `max_retries == 0`). `url` is used only to detect
    /// whether the endpoint is local.
    fn build_client<T: alloy_transport::IntoBoxTransport>(
        &self,
        transport: T,
        url: &alloy_transport_http::reqwest::Url,
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
    async fn resolve_chain_id(&self, url: alloy_transport_http::reqwest::Url) -> Result<u64> {
        let url_str = redact_rpc_url(url.as_str());
        let bare = build_bare_op_provider(self.build_retry_client(url));
        bare.get_chain_id().await.map_err(|e| {
            EvmeError::RpcError(format!("Failed to fetch chain ID from '{}': {e}", url_str))
        })
    }
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

/// The URL with any userinfo removed, for a log line, an error, or a cache file name.
///
/// Userinfo is the part of the authority before its last `@`, as the URL standard reads it, so a
/// password holding an `@` goes whole. It is removed from the text itself, so it holds for a URL
/// written without its scheme (`user:password@host`) and for one that does not parse. Credentials
/// anywhere else in the URL, such as an API key in the path or the query, are not recognised and
/// stay.
pub(crate) fn redact_rpc_url(raw: &str) -> String {
    let (scheme, rest) = raw.find("://").map_or(("", raw), |at| raw.split_at(at + 3));
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let stripped = match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}{}", &rest[at + 1..]),
        None => return normalized(raw),
    };
    normalized(&stripped)
}

/// `raw` as the URL parser writes it, or unchanged when it does not parse.
fn normalized(raw: &str) -> String {
    raw.parse::<alloy_transport_http::reqwest::Url>()
        .map_or_else(|_| raw.to_string(), |u| u.to_string())
}

/// Parses `--rpc`. The transport speaks HTTP, so any other scheme is refused here, before the URL
/// reaches a library whose error text would repeat it whole.
fn parse_rpc_url(raw: &str) -> Result<alloy_transport_http::reqwest::Url> {
    let url: alloy_transport_http::reqwest::Url = raw.parse().map_err(|e| {
        EvmeError::RpcError(format!("Invalid RPC URL '{}': {e}", redact_rpc_url(raw)))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(EvmeError::InvalidInput(format!(
            "Invalid RPC URL '{}': it has to start with http:// or https://",
            redact_rpc_url(raw)
        )));
    }
    Ok(url)
}

/// `rpc-cache-{chain_id}-{endpoint}.json`. `{endpoint}` is eight hex characters of the
/// keccak of the URL with userinfo removed.
pub fn rpc_cache_file_name(chain_id: u64, rpc_url: &str) -> String {
    let digest = alloy_primitives::keccak256(redact_rpc_url(rpc_url).as_bytes());
    format!("rpc-cache-{chain_id}-{}.json", alloy_primitives::hex::encode(&digest[..4]))
}

/// Deletes `path` when `--rpc.clear-cache` asked for it. A missing file is already clear.
fn clear_cache_file(path: &Path) -> Result<()> {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(EvmeError::RpcError(format!(
                "Failed to clear RPC cache at {}: {e}",
                path.display(),
            )));
        }
    } else {
        info!(path = %path.display(), "Cleared existing RPC cache");
    }
    Ok(())
}

/// Resolve the absolute cache file path for `chain_id`.
///
/// If the user passed `--rpc.cache-dir` we use it verbatim. Otherwise we fall back to
/// the platform cache directory (via `dirs::cache_dir()`): `$XDG_CACHE_HOME/mega-evme/rpc`
/// on Linux, `~/Library/Caches/mega-evme/rpc` on macOS, `%LOCALAPPDATA%\mega-evme\rpc` on
/// Windows. If the platform has no cache directory we error out and ask the user to
/// either pass `--rpc.cache-dir` or `--rpc.no-cache-file`.
fn resolve_cache_path(
    user_cache_dir: Option<&Path>,
    chain_id: u64,
    rpc_url: &str,
) -> Result<PathBuf> {
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
    Ok(dir.join(rpc_cache_file_name(chain_id, rpc_url)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Explicit `--rpc.cache-dir` is used verbatim. The file names the chain and a hash of the
    /// endpoint, and the hash does not change when only the userinfo does.
    #[test]
    fn test_resolve_cache_path_with_explicit_dir() {
        let dir = PathBuf::from("/some/user/dir");
        let url = "http://alice:secret@example.com/rpc";
        let path = resolve_cache_path(Some(&dir), 4326, url).expect("resolve");
        let name = rpc_cache_file_name(4326, url);
        assert_eq!(path, dir.join(&name));
        assert!(name.starts_with("rpc-cache-4326-"), "{name}");
        assert!(!name.contains("secret") && !name.contains("alice") && !name.contains("example"));
        assert_eq!(name, rpc_cache_file_name(4326, "http://example.com/rpc"));
        assert_ne!(name, rpc_cache_file_name(4326, "http://other.example/rpc"));
    }

    #[test]
    fn test_redact_rpc_url_strips_userinfo() {
        assert_eq!(
            redact_rpc_url("http://alice:secret@example.com/rpc"),
            redact_rpc_url("http://example.com/rpc")
        );
        assert!(!redact_rpc_url("http://alice:secret@example.com/rpc").contains("secret"));
        let broken = redact_rpc_url("http://alice:secret@not a url");
        assert!(!broken.contains("secret") && !broken.contains("alice"), "{broken}");
        assert_eq!(redact_rpc_url("not a url"), "not a url");
    }

    /// A URL written without its scheme parses as a scheme named after the user. Its userinfo
    /// goes all the same.
    #[test]
    fn test_redact_rpc_url_strips_userinfo_of_a_url_without_a_scheme() {
        assert_eq!(redact_rpc_url("alice:s3cret@rpc.example.com"), "rpc.example.com");
        assert_eq!(
            redact_rpc_url("alice:s3cret@rpc.example.com:8545/path"),
            "rpc.example.com:8545/path"
        );
        assert_eq!(redact_rpc_url("rpc.example.com"), "rpc.example.com");
        assert_eq!(redact_rpc_url("localhost:8545"), "localhost:8545");
    }

    /// The userinfo ends at the last `@` of the authority, so an `@` in the password does not
    /// leave its tail behind, whether the URL parses or not.
    #[test]
    fn test_redact_rpc_url_strips_a_password_holding_an_at_sign() {
        assert_eq!(
            redact_rpc_url("http://alice:p@ss@example.com/rpc"),
            redact_rpc_url("http://example.com/rpc")
        );
        let broken = redact_rpc_url("http://alice:p@ss@bad host/");
        assert_eq!(broken, "http://bad host/");
        assert_eq!(redact_rpc_url("alice:p@ss@example.com"), "example.com");
        // An `@` after the authority belongs to the path or the query.
        assert_eq!(redact_rpc_url("http://example.com/a@b"), "http://example.com/a@b");
        assert_eq!(redact_rpc_url("http://example.com/?mail=a@b"), "http://example.com/?mail=a@b");
    }

    /// Only HTTP is spoken, and the refusal of another scheme names the URL without its userinfo.
    #[test]
    fn test_parse_rpc_url_refuses_another_scheme_without_the_userinfo() {
        let error = parse_rpc_url("alice:s3cret@rpc.example.com").unwrap_err().to_string();
        assert!(error.contains("http://") && !error.contains("s3cret"), "{error}");
        let error = parse_rpc_url("ftp://alice:s3cret@example.com").unwrap_err().to_string();
        assert!(!error.contains("s3cret"), "{error}");
        assert!(parse_rpc_url("http://alice:s3cret@example.com").is_ok());
        assert!(parse_rpc_url("https://example.com").is_ok());
    }

    /// No explicit dir falls back to `dirs::cache_dir()` on platforms that have one.
    /// The path ends in `mega-evme/rpc/` and the endpoint's file name.
    #[test]
    fn test_resolve_cache_path_default_is_under_platform_cache() {
        let Some(expected_root) = dirs::cache_dir() else {
            // Skip on exotic platforms where no cache directory is available.
            return;
        };
        let url = "https://example.test/rpc";
        let path = resolve_cache_path(None, 11_155_420, url).expect("resolve");
        let expected =
            expected_root.join("mega-evme").join("rpc").join(rpc_cache_file_name(11_155_420, url));
        assert_eq!(path, expected);
    }
}
