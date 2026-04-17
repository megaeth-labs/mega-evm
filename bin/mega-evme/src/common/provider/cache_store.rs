//! Clean-exit cache persistence and the on-disk envelope format.
//!
//! Two persistence shapes share [`RpcCacheStore`]:
//!
//! - **Provider cache** (`--rpc.cache-dir`): per-chain alloy `SharedCache` dump.
//! - **Fixture capture** (`--rpc.capture-file`): transport-level JSON envelope (`{version,
//!   chain_id, cache, external_env}`) produced by [`CacheFileEnvelope`].
//!
//! The envelope is v1. Forward-incompatible changes bump `ENVELOPE_VERSION`;
//! additive fields use `#[serde(default)]` instead.

use std::{
    fmt, fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use alloy_provider::layers::SharedCache;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::transport::TransportCache;
use crate::common::{EvmeError, Result};

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
///
/// Variants are named for their role in the workflow, not their on-disk shape:
/// `ProviderCache` is the provider-level LRU backing `--rpc.cache-dir`;
/// `FixtureCapture` is the transport-level envelope backing `--rpc.capture-file`.
enum RpcCacheStoreInner {
    /// Provider-level LRU cache persisted to a per-chain file (`--rpc.cache-dir`).
    ProviderCache { cache: SharedCache, path: PathBuf },
    /// Transport-level fixture envelope captured for offline replay (`--rpc.capture-file`).
    FixtureCapture {
        cache: TransportCache,
        path: PathBuf,
        chain_id: u64,
        /// Optional external-env snapshot to write into the envelope at
        /// persist time. Populated via [`RpcCacheStore::set_external_env`]
        /// by the command layer once it has computed the effective value
        /// from CLI + prior envelope.
        external_env: Option<ExternalEnvSnapshot>,
    },
}

impl RpcCacheStore {
    /// Construct a store backed by a provider-level LRU cache file.
    ///
    /// `pub(super)` because only the builders in `mod.rs` construct these;
    /// external callers go through `build_provider` / `build_capture_provider`.
    pub(super) fn new(cache: SharedCache, cache_path: PathBuf) -> Self {
        Self { inner: Some(RpcCacheStoreInner::ProviderCache { cache, path: cache_path }) }
    }

    /// Construct a store backed by a transport-level fixture envelope file.
    ///
    /// `pub(super)` to keep the `TransportCache` parameter from leaking out of
    /// this module. The snapshot field starts empty; callers inject it later
    /// via [`Self::set_external_env`].
    pub(super) fn new_envelope(cache: TransportCache, path: PathBuf, chain_id: u64) -> Self {
        Self {
            inner: Some(RpcCacheStoreInner::FixtureCapture {
                cache,
                path,
                chain_id,
                external_env: None,
            }),
        }
    }

    /// Attach an external-env snapshot to a fixture-capture store.
    ///
    /// Silent no-op for [`RpcCacheStoreInner::ProviderCache`] and no-op
    /// variants — callers use the same uniform interface regardless of which
    /// variant the builder produced, matching the `persist()` contract.
    pub fn set_external_env(&mut self, ext: ExternalEnvSnapshot) {
        if let Some(RpcCacheStoreInner::FixtureCapture { external_env, .. }) = &mut self.inner {
            *external_env = Some(ext);
        }
    }

    /// Construct a no-op store.
    ///
    /// `pub(crate)` because `common/state.rs` calls it for the non-fork path.
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

    /// Returns the underlying [`SharedCache`] (provider-cache path only), or `None`.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache(&self) -> Option<&SharedCache> {
        match &self.inner {
            Some(RpcCacheStoreInner::ProviderCache { cache, .. }) => Some(cache),
            _ => None,
        }
    }

    /// Returns the resolved cache file path, or `None` for a no-op store.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache_path(&self) -> Option<&Path> {
        match &self.inner {
            Some(
                RpcCacheStoreInner::ProviderCache { path, .. } |
                RpcCacheStoreInner::FixtureCapture { path, .. },
            ) => Some(path.as_path()),
            None => None,
        }
    }

    /// Persist the cache to disk atomically. **Consumes the store** to enforce
    /// "persist once, then stop" — see `Why not Drop` on this type.
    ///
    /// For fixture-capture stores, any `external_env` snapshot previously
    /// attached via [`Self::set_external_env`] is written into the envelope.
    ///
    /// - **`ProviderCache`**: best-effort — failures are warn-logged and swallowed.
    /// - **`FixtureCapture`**: hard error — the fixture is the primary output of capture mode.
    /// - **No-op**: returns `Ok(())`.
    pub fn persist(self) -> Result<()> {
        let Some(inner) = self.inner else { return Ok(()) };
        match inner {
            RpcCacheStoreInner::ProviderCache { cache, path } => {
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
            RpcCacheStoreInner::FixtureCapture { cache, path, chain_id, external_env } => {
                let entry_count = cache.len();
                CacheFileEnvelope::new(&cache, chain_id, external_env.as_ref()).save(&path)?;
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
                RpcCacheStoreInner::ProviderCache { path, .. } |
                RpcCacheStoreInner::FixtureCapture { path, .. },
            ) => f.debug_struct("RpcCacheStore").field("path", path).finish_non_exhaustive(),
            None => f.debug_struct("RpcCacheStore").field("inner", &Option::<()>::None).finish(),
        }
    }
}

/// Atomically persist `cache` to `target` via a temp file + rename.
///
/// All error paths include `target` in the returned [`std::io::Error`] so the
/// warn-log in [`RpcCacheStore::persist`] identifies which file failed.
fn save_cache_atomic(cache: &SharedCache, target: &Path) -> std::io::Result<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
        std::io::Error::other(format!("failed to create temp file in {}: {e}", dir.display()))
    })?;
    let tmp_path = tmp.path().to_path_buf();

    // alloy's save_cache takes a PathBuf, not a Write.
    cache.save_cache(tmp_path).map_err(|e| {
        std::io::Error::other(format!("failed to save cache for {}: {e}", target.display()))
    })?;

    // Atomic rename. persist() consumes the NamedTempFile without deleting it.
    tmp.persist(target).map_err(|e| {
        std::io::Error::other(format!(
            "failed to rename temp file into {}: {}",
            target.display(),
            e.error,
        ))
    })?;
    Ok(())
}

/// Envelope version accepted by this build.
const ENVELOPE_VERSION: u32 = 1;

/// On-disk envelope format shared by `--rpc.capture-file` (write) and
/// `--rpc.replay-file` (read). Contains a transport-level cache dump,
/// chain ID, and optional external environment snapshot.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CacheFileEnvelope {
    /// Schema version (currently always 1, reserved for future format changes).
    version: u32,
    /// Chain ID at the time of capture.
    pub(super) chain_id: u64,
    /// Transport-level cache entries: `[{key, value}, ...]`.
    pub(super) cache: serde_json::Value,
    /// External environment inputs not derivable from RPC (e.g., SALT bucket capacities).
    #[serde(default)]
    pub(super) external_env: Option<ExternalEnvSnapshot>,
}

impl CacheFileEnvelope {
    /// Build an envelope for the given transport cache and metadata.
    pub(super) fn new(
        cache: &TransportCache,
        chain_id: u64,
        external_env: Option<&ExternalEnvSnapshot>,
    ) -> Self {
        Self {
            version: ENVELOPE_VERSION,
            chain_id,
            cache: cache.to_value(),
            external_env: external_env.cloned(),
        }
    }

    /// Read and validate an envelope from `path`.
    pub(super) fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).map_err(|e| {
            EvmeError::FixtureError(format!(
                "Failed to read RPC cache file {}: {e}",
                path.display()
            ))
        })?;
        let envelope: Self = serde_json::from_str(&content).map_err(|e| {
            EvmeError::FixtureError(format!(
                "Failed to parse RPC cache file {}: {e}",
                path.display()
            ))
        })?;
        if envelope.version != ENVELOPE_VERSION {
            return Err(EvmeError::FixtureError(format!(
                "Unsupported cache file version {} in '{}'; expected {ENVELOPE_VERSION}",
                envelope.version,
                path.display(),
            )));
        }
        Ok(envelope)
    }

    /// Atomically write this envelope to `path`.
    pub(super) fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir).map_err(|e| {
            EvmeError::FixtureError(format!(
                "Failed to create cache file directory {}: {e}",
                dir.display()
            ))
        })?;

        let serialized = serde_json::to_string_pretty(self).map_err(|e| {
            EvmeError::FixtureError(format!(
                "Failed to serialize envelope for {}: {e}",
                path.display()
            ))
        })?;

        let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
            EvmeError::FixtureError(format!("Failed to create temp file in {}: {e}", dir.display()))
        })?;
        tmp.write_all(serialized.as_bytes())
            .map_err(|e| EvmeError::FixtureError(format!("Failed to write envelope: {e}")))?;
        tmp.persist(path).map_err(|e| {
            EvmeError::FixtureError(format!(
                "Failed to persist envelope to {}: {e}",
                path.display()
            ))
        })?;

        Ok(())
    }
}

/// Snapshot of mega-evm external environment inputs not derivable from RPC.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalEnvSnapshot {
    /// SALT bucket capacity pairs `(bucket_id, capacity)`.
    #[serde(default)]
    pub bucket_capacities: Vec<(u32, u64)>,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;

    use super::*;

    /// Save a cache as an envelope, load it back, and verify the round-trip
    /// preserves version, `chain_id`, cache payload, and `external_env`.
    #[test]
    fn test_envelope_roundtrip_preserves_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test-cache.json");

        let cache = TransportCache::new();
        cache
            .merge(&serde_json::json!([
                {
                    "key": keccak256("eth_blockNumber"),
                    "value": r#"{"id":0,"jsonrpc":"2.0","result":"0x1"}"#,
                }
            ]))
            .expect("seed cache");

        let ext = ExternalEnvSnapshot { bucket_capacities: vec![(1, 100), (2, 200)] };
        CacheFileEnvelope::new(&cache, 4326, Some(&ext)).save(&path).expect("save envelope");

        let envelope = CacheFileEnvelope::load(&path).expect("load envelope");
        assert_eq!(envelope.version, 1);
        assert_eq!(envelope.chain_id, 4326);
        assert!(envelope.cache.is_array(), "cache should be a JSON array");

        // Verify the cache entry survived the round-trip.
        let loaded = TransportCache::from_value(&envelope.cache).expect("from_value");
        assert_eq!(loaded.len(), 1);

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
        let err = CacheFileEnvelope::load(&p).expect_err("version 2 should be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("Unsupported"), "error should mention Unsupported: {msg}");
    }

    /// `CacheFileEnvelope::load` must reject envelopes missing required fields.
    #[test]
    fn test_envelope_rejects_missing_fields() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Missing chain_id.
        let p1 = dir.path().join("no-chain.json");
        fs::write(&p1, r#"{"version":1,"cache":{}}"#).unwrap();
        let err = CacheFileEnvelope::load(&p1).expect_err("missing chain_id");
        let msg = format!("{err}");
        assert!(msg.contains("parse"), "error should mention parse: {msg}");

        // Missing cache.
        let p2 = dir.path().join("no-cache.json");
        fs::write(&p2, r#"{"version":1,"chain_id":1}"#).unwrap();
        let err = CacheFileEnvelope::load(&p2).expect_err("missing cache");
        let msg = format!("{err}");
        assert!(msg.contains("parse"), "error should mention parse: {msg}");

        // Missing version.
        let p3 = dir.path().join("no-version.json");
        fs::write(&p3, r#"{"chain_id":1,"cache":{}}"#).unwrap();
        let err = CacheFileEnvelope::load(&p3).expect_err("missing version");
        let msg = format!("{err}");
        assert!(msg.contains("parse"), "error should mention parse: {msg}");
    }
}
