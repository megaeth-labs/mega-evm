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
//!
//! # Concurrent cache-dir sharing
//!
//! Persist takes an exclusive advisory lock on a sidecar `<target>.lock`, re-reads
//! the target file, merges in-memory entries over on-disk ones (ours win on key
//! collision), then writes via temp-file + atomic rename. Multiple processes may
//! therefore share one `--rpc.cache-dir` without losing each other's entries.
//! The lock sidecar is left in place after the process exits (the flock is released
//! when the lock file handle is closed).

use std::{
    fmt, fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use alloy_provider::layers::SharedCache;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::transport::TransportCache;
use crate::{
    cache::{
        lock_sidecar_path, merge_envelope_for_persist, merge_kv_entries, read_provider_cache,
        reread_envelope_for_merge, write_bytes_atomic, write_envelope_atomic, CacheKv, EnvelopeDoc,
        EnvelopeReread, ExternalEnvDoc, ENVELOPE_VERSION,
    },
    common::{EvmeError, Result},
};

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
        /// Snapshot observed when the capture file was loaded (or `None` when
        /// the file was absent / carried no snapshot). Used for optimistic
        /// concurrency at persist: intentional A→B refreshes are accepted when
        /// the locked re-read is still A; only a true concurrent change conflicts.
        loaded_external_env: Option<ExternalEnvSnapshot>,
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
    /// this module. The write snapshot starts empty; callers inject it later
    /// via [`Self::set_external_env`].
    ///
    /// `loaded_external_env` is the load-time baseline already observed by the
    /// caller when it opened the capture file (if any). Persist compares the
    /// locked re-read against this value so intentional A→B refreshes are
    /// accepted when the on-disk snapshot is still A. The baseline must come
    /// from that first load — this constructor must not re-read the file.
    pub(super) fn new_envelope(
        cache: TransportCache,
        path: PathBuf,
        chain_id: u64,
        loaded_external_env: Option<ExternalEnvSnapshot>,
    ) -> Self {
        Self {
            inner: Some(RpcCacheStoreInner::FixtureCapture {
                cache,
                path,
                chain_id,
                external_env: None,
                loaded_external_env,
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
    /// Persist takes an exclusive advisory lock on `<path>.lock`, re-reads the
    /// on-disk file (a sibling process may have written since load), and merges
    /// our in-memory entries over the on-disk ones (ours win on key collision)
    /// before the atomic write.
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
            RpcCacheStoreInner::FixtureCapture {
                cache,
                path,
                chain_id,
                external_env,
                loaded_external_env,
            } => {
                let entry_count = cache.len();
                CacheFileEnvelope::new(&cache, chain_id, external_env.as_ref())
                    .save(&path, loaded_external_env.as_ref())?;
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

/// RAII exclusive lock on the sidecar file for `target`.
///
/// The lock is released when this guard is dropped (file handle closed).
/// The sidecar file itself is left on disk.
struct ExclusiveFileLock {
    _file: fs::File,
}

/// Acquire an exclusive advisory lock on `<target>.lock`, blocking until held.
///
/// The sidecar is created if missing and left in place after unlock.
fn acquire_exclusive_lock(target: &Path) -> std::io::Result<ExclusiveFileLock> {
    let lock_path = lock_sidecar_path(target);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    // truncate(false): the sidecar is only a flock target; keep any existing bytes.
    let file =
        OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&lock_path)?;
    file.lock_exclusive()?;
    Ok(ExclusiveFileLock { _file: file })
}

/// Atomically persist `cache` to `target` via lock + re-read-merge + temp rename.
///
/// All error paths include `target` in the returned [`std::io::Error`] so the
/// warn-log in [`RpcCacheStore::persist`] identifies which file failed.
///
/// Lock acquisition failure degrades to an unlocked write with a `warn!`.
/// A missing or corrupt on-disk file during re-read degrades to persisting
/// our entries only (with a `warn!` for corrupt).
fn save_cache_atomic(cache: &SharedCache, target: &Path) -> std::io::Result<()> {
    let _guard = match acquire_exclusive_lock(target) {
        Ok(g) => Some(g),
        Err(err) => {
            warn!(
                path = %target.display(),
                error = %err,
                "Failed to acquire RPC cache lock; persisting without lock",
            );
            None
        }
    };

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| {
        std::io::Error::other(format!("failed to create directory {}: {e}", dir.display()))
    })?;

    // SharedCache has no iteration API — dump our entries to a temp file and re-read.
    let our_tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
        std::io::Error::other(format!("failed to create temp file in {}: {e}", dir.display()))
    })?;
    let our_tmp_path = our_tmp.path().to_path_buf();
    cache.save_cache(our_tmp_path.clone()).map_err(|e| {
        std::io::Error::other(format!("failed to save cache for {}: {e}", target.display()))
    })?;

    let our_entries: Vec<CacheKv> = match fs::read_to_string(&our_tmp_path)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(entries) => entries,
        Err(err) => {
            return Err(std::io::Error::other(format!(
                "failed to re-read our cache dump for {}: {err}",
                target.display()
            )));
        }
    };
    // Drop the NamedTempFile so it is unlinked; we only needed the dump bytes.
    drop(our_tmp);

    let disk_entries = match read_provider_cache(target) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(
                path = %target.display(),
                error = %err,
                "Failed to re-read on-disk RPC cache during merge; persisting our entries only",
            );
            Vec::new()
        }
    };

    let merged = merge_kv_entries(disk_entries, our_entries);
    let serialized = serde_json::to_vec(&merged).map_err(|e| {
        std::io::Error::other(format!(
            "failed to serialize merged cache for {}: {e}",
            target.display()
        ))
    })?;
    write_bytes_atomic(target, &serialized)?;
    Ok(())
}

/// On-disk envelope format shared by `--rpc.capture-file` (write) and
/// `--rpc.replay-file` (read). Contains a transport-level cache dump,
/// chain ID, and optional external environment snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Atomically write this envelope to `path` under a lock, merging with any
    /// on-disk envelope already present (ours win on cache key collision).
    ///
    /// `loaded_external_env` is the snapshot observed when this process opened
    /// the capture file. Persist accepts an intentional A→B refresh when the
    /// locked re-read is still A (or absent); a concurrent change to a third
    /// snapshot C hard-errors and names loaded/ours/on-disk. See
    /// [`merge_envelope_for_persist`].
    ///
    /// On-disk re-read failures are typed: identity/schema mismatches hard-fail;
    /// corrupt JSON degrades to ours-only with a warning.
    ///
    /// Lock contention blocks until the lock is free. Failure to create/acquire
    /// the lock degrades to an unlocked write with a `warn!`. Write failures
    /// remain hard errors.
    pub(super) fn save(
        &self,
        path: &Path,
        loaded_external_env: Option<&ExternalEnvSnapshot>,
    ) -> Result<()> {
        let _guard = match acquire_exclusive_lock(path) {
            Ok(g) => Some(g),
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "Failed to acquire envelope lock; persisting without lock",
                );
                None
            }
        };

        let ours = self.to_merge_doc()?;
        let loaded_doc = loaded_external_env
            .map(|e| ExternalEnvDoc { bucket_capacities: e.bucket_capacities.clone() });
        let to_write = if path.exists() {
            // Typed hard vs degradable: no substring matching on formatted messages.
            match reread_envelope_for_merge(path) {
                EnvelopeReread::Ok(on_disk) => {
                    merge_envelope_for_persist(&on_disk, &ours, loaded_doc.as_ref(), path)?
                }
                EnvelopeReread::Hard(err) => return Err(err),
                EnvelopeReread::Degradable(msg) => {
                    warn!(
                        path = %path.display(),
                        error = %msg,
                        "Failed to re-read on-disk envelope during merge; persisting our entries only",
                    );
                    // Still write the canonical form when replacing corrupt content.
                    canonicalize_envelope_external_env(ours)
                }
            }
        } else {
            canonicalize_envelope_external_env(ours)
        };

        write_envelope_atomic(path, &to_write)
    }

    fn to_merge_doc(&self) -> Result<EnvelopeDoc> {
        let cache: Vec<CacheKv> = serde_json::from_value(self.cache.clone()).map_err(|e| {
            EvmeError::FixtureError(format!("Failed to decode envelope cache entries: {e}"))
        })?;
        Ok(EnvelopeDoc {
            version: self.version,
            chain_id: self.chain_id,
            cache,
            external_env: self
                .external_env
                .as_ref()
                .map(|e| ExternalEnvDoc { bucket_capacities: e.bucket_capacities.clone() }),
        })
    }
}

/// Canonicalize `external_env` on a merge doc about to be written alone (no
/// on-disk merge). Merge path already returns a canonical snapshot.
fn canonicalize_envelope_external_env(mut doc: EnvelopeDoc) -> EnvelopeDoc {
    if let Some(ext) = doc.external_env.take() {
        doc.external_env = Some(ext.canonicalized());
    }
    doc
}

/// Snapshot of mega-evm external environment inputs not derivable from RPC.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalEnvSnapshot {
    /// SALT bucket capacity pairs `(bucket_id, capacity)`.
    #[serde(default)]
    pub bucket_capacities: Vec<(u32, u64)>,
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{keccak256, B256};
    use alloy_provider::layers::CacheLayer;

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
        CacheFileEnvelope::new(&cache, 4326, Some(&ext)).save(&path, None).expect("save envelope");

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

    /// Interleaving: A holds only key A in memory; B persists key B; A then
    /// persists — on-disk file must contain the union (B's entries survive).
    #[test]
    fn test_provider_cache_persist_merges_interleaved_disk_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");

        let key_a = B256::repeat_byte(0xaa);
        let key_b = B256::repeat_byte(0xbb);
        let val_a = r#"{"result":"a"}"#.to_string();
        let val_b = r#"{"result":"b"}"#.to_string();

        // Process B persists first.
        let cache_b = CacheLayer::new(64).cache();
        cache_b.put(key_b, val_b.clone()).expect("put b");
        RpcCacheStore::new(cache_b, path.clone()).persist().expect("persist b");

        // Process A never loaded B's write; only has key_a in memory.
        let cache_a = CacheLayer::new(64).cache();
        cache_a.put(key_a, val_a.clone()).expect("put a");
        RpcCacheStore::new(cache_a, path.clone()).persist().expect("persist a");

        let loaded = CacheLayer::new(64).cache();
        loaded.load_cache(path).expect("load");
        assert_eq!(loaded.get(&key_a).as_deref(), Some(val_a.as_str()));
        assert_eq!(loaded.get(&key_b).as_deref(), Some(val_b.as_str()));
    }

    /// On collision, the process that persists last wins for that key.
    #[test]
    fn test_provider_cache_persist_ours_wins_on_collision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");
        let key = B256::repeat_byte(0x01);

        let cache_b = CacheLayer::new(64).cache();
        cache_b.put(key, "from-b".into()).expect("put");
        RpcCacheStore::new(cache_b, path.clone()).persist().expect("persist b");

        let cache_a = CacheLayer::new(64).cache();
        cache_a.put(key, "from-a".into()).expect("put");
        RpcCacheStore::new(cache_a, path.clone()).persist().expect("persist a");

        let loaded = CacheLayer::new(64).cache();
        loaded.load_cache(path).expect("load");
        assert_eq!(loaded.get(&key).as_deref(), Some("from-a"));
    }

    /// Lock sidecar `<target>.lock` is created on persist and left in place.
    #[test]
    fn test_provider_cache_persist_creates_lock_sidecar_left_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-9.json");
        let lock = lock_sidecar_path(&path);
        assert!(!lock.exists());

        let cache = CacheLayer::new(16).cache();
        cache.put(B256::repeat_byte(1), "v".into()).expect("put");
        RpcCacheStore::new(cache, path.clone()).persist().expect("persist");

        assert!(path.exists(), "cache file written");
        assert!(lock.exists(), "lock sidecar left in place");
        // Sidecar is an empty (or near-empty) lock file, not the cache payload.
        let lock_meta = fs::metadata(&lock).expect("lock meta");
        assert!(lock_meta.len() == 0 || lock_meta.is_file());
    }

    /// Envelope persist merges on-disk entries the same way, with `chain_id` check.
    #[test]
    fn test_envelope_persist_merges_interleaved_disk_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        let key_a = keccak256("a");
        let key_b = keccak256("b");

        let cache_b = TransportCache::new();
        cache_b
            .merge(&serde_json::json!([{
                "key": key_b,
                "value": r#"{"result":"b"}"#,
            }]))
            .expect("seed b");
        CacheFileEnvelope::new(&cache_b, 99, None).save(&path, None).expect("save b");

        let cache_a = TransportCache::new();
        cache_a
            .merge(&serde_json::json!([{
                "key": key_a,
                "value": r#"{"result":"a"}"#,
            }]))
            .expect("seed a");
        CacheFileEnvelope::new(&cache_a, 99, None).save(&path, None).expect("save a");

        let env = CacheFileEnvelope::load(&path).expect("load");
        let loaded = TransportCache::from_value(&env.cache).expect("from_value");
        assert_eq!(loaded.len(), 2);
        assert_eq!(env.chain_id, 99);
    }

    /// Envelope persist hard-errors on `chain_id` mismatch with on-disk file.
    #[test]
    fn test_envelope_persist_rejects_chain_id_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        let cache_b = TransportCache::new();
        CacheFileEnvelope::new(&cache_b, 1, None).save(&path, None).expect("save b");

        let cache_a = TransportCache::new();
        let err =
            CacheFileEnvelope::new(&cache_a, 2, None).save(&path, None).expect_err("mismatch");
        assert!(err.to_string().contains("chain_id"));
    }

    /// Corrupt on-disk envelope under a path whose name contains `chain_id` is
    /// degradable (warn + replace), not a hard error — classification is typed,
    /// not substring-based on the formatted message / path.
    #[test]
    fn test_envelope_persist_degrades_on_corrupt_disk_path_containing_chain_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Path deliberately contains the substrings the old classifier matched.
        let path = dir.path().join("chain_id_version_capture.json");
        fs::write(&path, "not-json{{{").expect("corrupt");

        let cache = TransportCache::new();
        cache
            .merge(&serde_json::json!([{
                "key": keccak256("eth_blockNumber"),
                "value": r#"{"id":0,"jsonrpc":"2.0","result":"0x1"}"#,
            }]))
            .expect("seed");
        CacheFileEnvelope::new(&cache, 7, None)
            .save(&path, None)
            .expect("corrupt disk with chain_id in path must degrade, not hard-fail");

        let env = CacheFileEnvelope::load(&path).expect("ours written");
        assert_eq!(env.chain_id, 7);
        assert_eq!(TransportCache::from_value(&env.cache).expect("from_value").len(), 1);
    }

    /// Genuine `chain_id` mismatch remains a hard error (typed path via merge).
    #[test]
    fn test_envelope_persist_hard_errors_on_genuine_chain_id_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Same path naming trap as the corrupt-file test: must not flip classification.
        let path = dir.path().join("chain_id_version_capture.json");

        let cache_b = TransportCache::new();
        CacheFileEnvelope::new(&cache_b, 1, None).save(&path, None).expect("save b");

        let cache_a = TransportCache::new();
        let err = CacheFileEnvelope::new(&cache_a, 2, None)
            .save(&path, None)
            .expect_err("chain_id mismatch must hard-fail");
        let msg = err.to_string();
        assert!(msg.contains("chain_id"), "msg={msg}");
    }

    /// Sequential capture refresh: loaded A, ours B, disk still A → B is written.
    #[test]
    fn test_envelope_persist_intentional_external_env_refresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        let loaded = ExternalEnvSnapshot { bucket_capacities: vec![(1, 10)] };
        let cache_a = TransportCache::new();
        CacheFileEnvelope::new(&cache_a, 7, Some(&loaded)).save(&path, None).expect("seed A");

        let ours = ExternalEnvSnapshot { bucket_capacities: vec![(1, 99)] };
        let cache_b = TransportCache::new();
        CacheFileEnvelope::new(&cache_b, 7, Some(&ours))
            .save(&path, Some(&loaded))
            .expect("intentional A→B refresh");

        let env = CacheFileEnvelope::load(&path).expect("load");
        let written = env.external_env.expect("external_env written");
        assert_eq!(written.bucket_capacities, vec![(1, 99)]);
    }

    /// Store constructed with baseline A; disk still A; ours B → B wins
    /// (intentional refresh through `set_external_env` + `persist`).
    #[test]
    fn test_store_persist_intentional_external_env_refresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        let a = ExternalEnvSnapshot { bucket_capacities: vec![(1, 10)] };
        CacheFileEnvelope::new(&TransportCache::new(), 7, Some(&a))
            .save(&path, None)
            .expect("seed A");

        // Baseline A is passed in (same object the caller loaded); no re-read.
        let mut store =
            RpcCacheStore::new_envelope(TransportCache::new(), path.clone(), 7, Some(a));
        store.set_external_env(ExternalEnvSnapshot { bucket_capacities: vec![(1, 99)] });
        store.persist().expect("store-level intentional A→B refresh must succeed");

        let env = CacheFileEnvelope::load(&path).expect("load");
        assert_eq!(
            env.external_env.expect("external_env written").bucket_capacities,
            vec![(1, 99)]
        );
    }

    /// Store constructed with baseline A; on-disk mutated to C before persist;
    /// ours B derived from A → hard conflict naming loaded/ours/on-disk.
    ///
    /// Regression for the double-read defect: the store must use the caller's
    /// loaded baseline, not re-read the file at construction (which would
    /// observe C and treat ours-from-A as an intentional refresh of C).
    #[test]
    fn test_store_persist_rejects_concurrent_external_env_conflict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        let a = ExternalEnvSnapshot { bucket_capacities: vec![(1, 10)] };
        CacheFileEnvelope::new(&TransportCache::new(), 7, Some(&a))
            .save(&path, None)
            .expect("seed A");

        // Baseline A from the first load — not re-read from disk at construction.
        let mut store =
            RpcCacheStore::new_envelope(TransportCache::new(), path.clone(), 7, Some(a.clone()));

        // Concurrent writer lands C (≠A, ≠B) after our load, before our persist.
        let c = ExternalEnvSnapshot { bucket_capacities: vec![(1, 42)] };
        CacheFileEnvelope::new(&TransportCache::new(), 7, Some(&c))
            .save(&path, Some(&a))
            .expect("concurrent C");

        store.set_external_env(ExternalEnvSnapshot { bucket_capacities: vec![(1, 99)] });
        let err = store.persist().expect_err("true concurrent conflict via store");
        let msg = err.to_string();
        assert!(msg.contains("external_env"), "msg={msg}");
        assert!(
            msg.contains("loaded") && msg.contains("ours") && msg.contains("on-disk"),
            "msg={msg}"
        );
        assert!(msg.contains("10") && msg.contains("99") && msg.contains("42"), "msg={msg}");
    }

    /// If the store re-read the file at construction, a concurrent C would be
    /// mistaken for the baseline and an A-derived B would silently overwrite C.
    /// Passing baseline A while disk is already C must still conflict.
    #[test]
    fn test_store_persist_uses_passed_baseline_not_disk_at_construction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        // Disk already holds C when the store is constructed (simulates a
        // writer that landed between the caller's first load and store build).
        let a = ExternalEnvSnapshot { bucket_capacities: vec![(1, 10)] };
        let c = ExternalEnvSnapshot { bucket_capacities: vec![(1, 42)] };
        CacheFileEnvelope::new(&TransportCache::new(), 7, Some(&c))
            .save(&path, None)
            .expect("disk is C");

        let mut store = RpcCacheStore::new_envelope(TransportCache::new(), path, 7, Some(a));
        store.set_external_env(ExternalEnvSnapshot { bucket_capacities: vec![(1, 99)] });
        let err = store
            .persist()
            .expect_err("passed baseline A must not be replaced by on-disk C at construction");
        let msg = err.to_string();
        assert!(msg.contains("external_env"), "msg={msg}");
        assert!(
            msg.contains("loaded") && msg.contains("ours") && msg.contains("on-disk"),
            "msg={msg}"
        );
        assert!(msg.contains("10") && msg.contains("99") && msg.contains("42"), "msg={msg}");
    }

    /// Concurrent conflict through save: loaded A, ours B, disk C → hard error.
    #[test]
    fn test_envelope_persist_rejects_concurrent_external_env_conflict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        let c = ExternalEnvSnapshot { bucket_capacities: vec![(1, 42)] };
        CacheFileEnvelope::new(&TransportCache::new(), 7, Some(&c))
            .save(&path, None)
            .expect("seed C");

        let loaded = ExternalEnvSnapshot { bucket_capacities: vec![(1, 10)] };
        let ours = ExternalEnvSnapshot { bucket_capacities: vec![(1, 99)] };
        let err = CacheFileEnvelope::new(&TransportCache::new(), 7, Some(&ours))
            .save(&path, Some(&loaded))
            .expect_err("true concurrent conflict");
        let msg = err.to_string();
        assert!(msg.contains("external_env"), "msg={msg}");
        assert!(
            msg.contains("loaded") && msg.contains("ours") && msg.contains("on-disk"),
            "msg={msg}"
        );
        assert!(msg.contains("10") && msg.contains("99") && msg.contains("42"), "msg={msg}");
    }

    /// Same effective capacities in different order do not conflict at save.
    #[test]
    fn test_envelope_persist_order_insensitive_external_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        let a = ExternalEnvSnapshot { bucket_capacities: vec![(1, 10), (2, 20)] };
        CacheFileEnvelope::new(&TransportCache::new(), 7, Some(&a))
            .save(&path, None)
            .expect("seed");

        let b = ExternalEnvSnapshot { bucket_capacities: vec![(2, 20), (1, 10)] };
        // Pretend we loaded something else so equality is the only thing that
        // would save us from a false concurrent-conflict report.
        let foreign = ExternalEnvSnapshot { bucket_capacities: vec![(9, 9)] };
        CacheFileEnvelope::new(&TransportCache::new(), 7, Some(&b))
            .save(&path, Some(&foreign))
            .expect("order-only difference must not conflict");

        let env = CacheFileEnvelope::load(&path).expect("load");
        assert_eq!(env.external_env.expect("present").bucket_capacities, vec![(1, 10), (2, 20)]);
    }

    /// Typed re-read: corrupt content is Degradable even when path mentions `chain_id`.
    #[test]
    fn test_reread_envelope_classifies_corrupt_vs_identity() {
        let dir = tempfile::tempdir().expect("tempdir");

        let corrupt = dir.path().join("chain_id_and_version.json");
        fs::write(&corrupt, "{not valid").expect("write");
        match reread_envelope_for_merge(&corrupt) {
            EnvelopeReread::Degradable(msg) => {
                assert!(msg.contains("parse") || msg.contains("Failed"), "{msg}");
            }
            other => panic!("corrupt must be Degradable, got {other:?}"),
        }

        let bad_version = dir.path().join("env.json");
        fs::write(&bad_version, r#"{"version":99,"chain_id":1,"cache":[]}"#).unwrap();
        match reread_envelope_for_merge(&bad_version) {
            EnvelopeReread::Hard(err) => {
                assert!(err.to_string().contains("Unsupported") || err.to_string().contains("99"));
            }
            other => panic!("unsupported version must be Hard, got {other:?}"),
        }

        let ok_path = dir.path().join("ok.json");
        fs::write(&ok_path, r#"{"version":1,"chain_id":5,"cache":[]}"#).unwrap();
        match reread_envelope_for_merge(&ok_path) {
            EnvelopeReread::Ok(doc) => assert_eq!(doc.chain_id, 5),
            other => panic!("valid envelope must be Ok, got {other:?}"),
        }
    }

    /// Corrupt on-disk provider cache during re-read does not abort; ours are written.
    #[test]
    fn test_provider_cache_persist_degrades_on_corrupt_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");
        fs::write(&path, "not-json{{{").expect("corrupt");

        let key = B256::repeat_byte(0xcc);
        let cache = CacheLayer::new(16).cache();
        cache.put(key, "ok".into()).expect("put");
        RpcCacheStore::new(cache, path.clone()).persist().expect("persist");

        let loaded = CacheLayer::new(16).cache();
        loaded.load_cache(path).expect("load");
        assert_eq!(loaded.get(&key).as_deref(), Some("ok"));
    }
}
