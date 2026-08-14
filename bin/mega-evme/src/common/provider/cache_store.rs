//! Clean-exit cache persistence and the on-disk envelope format.
//!
//! Every file this module writes is the same v1 envelope (`{version, chain_id,
//! cache, external_env}`), and the two writers differ only in policy:
//!
//! - **Online cache** (`--rpc.cache-dir`): a per-chain file, capped at the configured entry
//!   ceiling, written best-effort and stamped with `kind: "cache"` so it can be told apart from a
//!   fixture that happens to sit at the same path.
//! - **Fixture capture** (`--rpc.capture-file`): the whole recorded conversation, written as a hard
//!   requirement of the run, carrying the optional `external_env` snapshot.
//!
//! The envelope is v1. Forward-incompatible changes bump `ENVELOPE_VERSION`;
//! additive fields use `#[serde(default)]` instead. `kind` is such a field, and
//! only the online writer emits it — a capture file's bytes are what they always
//! were.
//!
//! # Concurrent cache-dir sharing
//!
//! Persist takes an exclusive advisory lock on a sidecar `<target>.lock`, re-reads
//! the target file, merges in-memory entries over on-disk ones (ours win on key
//! collision), then writes via temp-file + atomic rename. Multiple processes may
//! therefore share one `--rpc.cache-dir` without losing each other's entries.
//! The lock sidecar is left in place after the process exits (the flock is released
//! when the lock file handle is closed).
//!
//! Persist fails closed on the lock: if the lock cannot be acquired, nothing is
//! written. Writing unlocked would reintroduce exactly the lost-update race the
//! lock exists to prevent, and it would do so silently — a sibling process's
//! entries would vanish under our rename.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

#[cfg(any(test, feature = "test-utils"))]
use alloy_primitives::B256;
use alloy_provider::layers::SharedCache;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::transport::TransportCache;
use crate::{
    cache::{
        acquire_exclusive_lock, detect_shape, lock_sidecar_path, merge_envelope_for_persist,
        merge_provider_entries_capped, reread_envelope_for_merge, reread_provider_cache_for_merge,
        warn_user, write_bytes_atomic, write_envelope_atomic, CacheKv, CacheShape, EnvelopeDoc,
        EnvelopeReread, ExternalEnvDoc, ProviderReread, ENVELOPE_VERSION,
    },
    common::{EvmeError, Result},
};

/// Clean-exit cache persistence handle.
///
/// An `RpcCacheStore` may internally have nothing to persist — a non-fork run,
/// or `--rpc.no-cache-file`. In either case `persist()` is a no-op. Callers do
/// not and must not branch on whether
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
/// `OnlineCache` is the per-chain file backing `--rpc.cache-dir`;
/// `FixtureCapture` is the fixture backing `--rpc.capture-file`.
enum RpcCacheStoreInner {
    /// Provider-level LRU cache persisted to a per-chain file.
    ///
    /// No builder produces this any more — `--rpc.cache-dir` writes an envelope
    /// through [`RpcCacheStoreInner::OnlineCache`]. Kept until the alloy
    /// provider-cache support surface is removed as a whole.
    #[cfg_attr(not(test), allow(dead_code))]
    ProviderCache { cache: SharedCache, path: PathBuf },
    /// Transport-level cache persisted to the per-chain `--rpc.cache-dir` file.
    OnlineCache { cache: TransportCache, path: PathBuf, chain_id: u64 },
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
    /// Retained alongside the alloy provider-cache support surface it belongs
    /// to; the online builder in `mod.rs` calls [`Self::new_online_cache`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn new(cache: SharedCache, cache_path: PathBuf) -> Self {
        Self { inner: Some(RpcCacheStoreInner::ProviderCache { cache, path: cache_path }) }
    }

    /// Construct a store backed by the per-chain online cache file.
    ///
    /// `pub(super)` because only the builders in `mod.rs` construct these;
    /// external callers go through `build_provider` / `build_capture_provider`.
    /// `chain_id` is the value the live probe resolved, and persist refuses to
    /// write over a file claiming a different one.
    pub(super) fn new_online_cache(cache: TransportCache, path: PathBuf, chain_id: u64) -> Self {
        Self { inner: Some(RpcCacheStoreInner::OnlineCache { cache, path, chain_id }) }
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
    /// Silent no-op for the online-cache and no-op variants — callers use the
    /// same uniform interface regardless of which variant the builder produced,
    /// matching the `persist()` contract. The online cache is best-effort and
    /// swallows its write failures, so carrying the snapshot's
    /// concurrent-conflict protocol on that path would turn a designed hard
    /// error into a silent one.
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

    /// Seed a response into the in-memory transport cache, as a served RPC call
    /// would. Returns `false` for stores that hold no transport cache.
    ///
    /// The entry is persistable: seeding stands in for a response the cache
    /// policy admitted, which is the only kind a test has reason to plant.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn put_cache_entry(&self, key: B256, value: String) -> bool {
        match &self.inner {
            Some(
                RpcCacheStoreInner::OnlineCache { cache, .. } |
                RpcCacheStoreInner::FixtureCapture { cache, .. },
            ) => {
                cache.put(key, value, true);
                true
            }
            _ => false,
        }
    }

    /// Read a response back out of the in-memory transport cache.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache_entry(&self, key: &B256) -> Option<String> {
        match &self.inner {
            Some(
                RpcCacheStoreInner::OnlineCache { cache, .. } |
                RpcCacheStoreInner::FixtureCapture { cache, .. },
            ) => cache.get(key),
            _ => None,
        }
    }

    /// Returns the resolved cache file path, or `None` for a no-op store.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn cache_path(&self) -> Option<&Path> {
        match &self.inner {
            Some(
                RpcCacheStoreInner::ProviderCache { path, .. } |
                RpcCacheStoreInner::OnlineCache { path, .. } |
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
    /// - **`ProviderCache` / `OnlineCache`**: best-effort — failures are warn-logged and swallowed.
    /// - **`FixtureCapture`**: hard error — the fixture is the primary output of capture mode.
    /// - **No-op**: returns `Ok(())`.
    pub fn persist(self) -> Result<()> {
        let Some(inner) = self.inner else { return Ok(()) };
        match inner {
            RpcCacheStoreInner::ProviderCache { cache, path } => {
                match save_cache_atomic(&cache, &path) {
                    Ok(true) => info!(path = %path.display(), "Persisted RPC cache"),
                    // Intentional skip (e.g. foreign on-disk shape) already warned inside.
                    Ok(false) => {}
                    Err(err) => warn!(
                        path = %path.display(),
                        error = %err,
                        "Failed to save RPC cache (continuing)",
                    ),
                }
                Ok(())
            }
            RpcCacheStoreInner::OnlineCache { cache, path, chain_id } => {
                match save_online_cache_atomic(&cache, &path, chain_id) {
                    Ok(true) => info!(path = %path.display(), "Persisted RPC cache"),
                    // Intentional skip (e.g. a foreign file at the path) already
                    // warned inside.
                    Ok(false) => {}
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
                RpcCacheStoreInner::OnlineCache { path, .. } |
                RpcCacheStoreInner::FixtureCapture { path, .. },
            ) => f.debug_struct("RpcCacheStore").field("path", path).finish_non_exhaustive(),
            None => f.debug_struct("RpcCacheStore").field("inner", &Option::<()>::None).finish(),
        }
    }
}

/// Atomically persist `cache` to `target` via lock + re-read-merge + temp rename.
///
/// Returns `Ok(true)` when the file was written, `Ok(false)` when the write was
/// intentionally skipped (a recognizable foreign on-disk shape), and `Err` on
/// lock/IO failure. All error paths include `target` in the returned
/// [`std::io::Error`] so the warn-log in [`RpcCacheStore::persist`] identifies
/// which file failed.
///
/// Lock acquisition failure aborts the persist: the provider cache is a
/// best-effort artifact, so skipping it costs a re-fetch, while an unlocked
/// write can silently delete a sibling process's entries.
///
/// On-disk re-read is typed (same classification as `cache merge`):
/// - missing / provider array → merge and write;
/// - corrupt / unreadable → degrade to ours-only (with a `warn!`);
/// - recognizable foreign shape (capture envelope, …) → skip the write with a visible warning so a
///   mispointed cache-dir cannot destroy the foreign file.
fn save_cache_atomic(cache: &SharedCache, target: &Path) -> std::io::Result<bool> {
    let _guard = acquire_exclusive_lock(target).map_err(|e| {
        std::io::Error::other(format!(
            "failed to acquire the cache lock {} for {}: {e}; \
             cache entries were not saved (an unlocked write could drop a \
             concurrent process's entries)",
            lock_sidecar_path(target).display(),
            target.display(),
        ))
    })?;

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

    let disk_entries = match reread_provider_cache_for_merge(target) {
        ProviderReread::Ok(entries) => entries,
        ProviderReread::Degradable(msg) => {
            warn!(
                path = %target.display(),
                error = %msg,
                "Failed to re-read on-disk RPC cache during merge; persisting our entries only",
            );
            Vec::new()
        }
        ProviderReread::Hard(err) => {
            // Best-effort provider persist must not destroy a foreign file that
            // a shared-dir misconfiguration pointed it at (e.g. a capture
            // envelope). Skip the write so the foreign content survives.
            // stderr via `warn_user`: default CLI tracing is off, so a
            // `warn!`-only line would never reach the operator who needs to
            // fix the shared-dir misconfiguration.
            warn_user(format_args!(
                "Skipping RPC cache persist to '{}': {err}. On-disk file is not a \
                 provider cache; leaving it intact",
                target.display(),
            ));
            return Ok(false);
        }
    };

    // The union must respect the configured cap: a sibling's file plus ours can
    // otherwise exceed what either run was allowed to keep.
    let merged =
        merge_provider_entries_capped(disk_entries, our_entries, cache.max_items() as usize);
    let serialized = serde_json::to_vec(&merged).map_err(|e| {
        std::io::Error::other(format!(
            "failed to serialize merged cache for {}: {e}",
            target.display()
        ))
    })?;
    write_bytes_atomic(target, &serialized)?;
    Ok(true)
}

/// Value of the envelope's `kind` field that marks the online `--rpc.cache-dir`
/// cache.
///
/// The field is additive and optional: only this writer emits it, so a capture
/// fixture is byte-for-byte what it always was, and an envelope without the
/// field is by construction not one of ours.
const ONLINE_CACHE_KIND: &str = "cache";

/// On-disk form of the online `--rpc.cache-dir` cache.
///
/// The v1 envelope plus `kind`. `external_env` is always absent here: the
/// snapshot's concurrent-conflict protocol is a hard error by design, and this
/// path swallows its write failures, so carrying it would convert that protocol
/// into a silent one. The field is still written (as `null`) so the file is the
/// same shape every other reader of the envelope expects.
#[derive(Debug, Serialize)]
struct OnlineCacheDoc {
    version: u32,
    kind: &'static str,
    chain_id: u64,
    cache: Vec<CacheKv>,
    external_env: Option<ExternalEnvDoc>,
}

/// Identity fields of an on-disk envelope, read before its entries are decoded.
///
/// Split from the entries on purpose: whether a file is ours is decided from
/// three small fields, so a file that is ours but has an undecodable entry list
/// can be replaced while a file that is not ours is left alone whatever its
/// entries look like.
#[derive(Debug, Deserialize)]
struct OnlineCacheHeader {
    version: u32,
    #[serde(default)]
    kind: Option<String>,
    chain_id: u64,
}

/// What the file at the online cache path turned out to be.
#[derive(Debug)]
enum OnlineCacheFile {
    /// Nothing on disk.
    Absent,
    /// This chain's online cache: its entries may be adopted and merged into.
    Ours(Vec<CacheKv>),
    /// A readable file this writer did not produce (a capture fixture, an
    /// unrecognized shape, or a file that cannot even be read). Never loaded and
    /// never overwritten — a mispointed `--rpc.cache-dir` must not destroy it.
    Foreign(String),
    /// Ours in name but not usable: a retired on-disk format, or content this
    /// build cannot decode. The cache is a regenerable artifact, so it is
    /// replaced rather than reported.
    Stale(String),
    /// Our cache, for a different chain than the endpoint reports.
    ChainMismatch(u64),
}

/// Classify the file at the per-chain online cache path.
///
/// Reads the file once and decides from its content alone; `chain_id` is the
/// value the live probe resolved for this run.
fn classify_online_cache_file(path: &Path, chain_id: u64) -> OnlineCacheFile {
    if !path.exists() {
        return OnlineCacheFile::Absent;
    }
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            return OnlineCacheFile::Foreign(format!("cannot be read ({e})"));
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(e) => {
            return OnlineCacheFile::Stale(format!("is not valid JSON ({e})"));
        }
    };
    match detect_shape(&value, path) {
        // The bare `[{key, value}]` array a retired build wrote. It cannot be
        // converted: it stores hashed request keys, this build hashes requests
        // differently, and the method and params behind each entry are not
        // recoverable from the file.
        Ok(CacheShape::Provider) => OnlineCacheFile::Stale(
            "was written by an older mega-evme (a bare JSON array) and cannot be converted"
                .to_string(),
        ),
        Ok(CacheShape::Envelope) => {
            let Ok(header) = serde_json::from_value::<OnlineCacheHeader>(value.clone()) else {
                return OnlineCacheFile::Foreign(
                    "is an envelope whose identity fields this build cannot read".to_string(),
                );
            };
            if header.kind.as_deref() != Some(ONLINE_CACHE_KIND) {
                return OnlineCacheFile::Foreign(
                    "is a cache envelope written by something other than the RPC cache \
                     (a --rpc.capture-file fixture, most likely)"
                        .to_string(),
                );
            }
            if header.version != ENVELOPE_VERSION {
                return OnlineCacheFile::Stale(format!(
                    "has cache file version {}, and this build writes {ENVELOPE_VERSION}",
                    header.version,
                ));
            }
            if header.chain_id != chain_id {
                return OnlineCacheFile::ChainMismatch(header.chain_id);
            }
            match serde_json::from_value::<Vec<CacheKv>>(value["cache"].clone()) {
                Ok(entries) => OnlineCacheFile::Ours(entries),
                Err(e) => OnlineCacheFile::Stale(format!("has undecodable cache entries ({e})")),
            }
        }
        // Structured JSON in a shape neither writer produces: somebody else's
        // file, whatever it is.
        Err(e) => OnlineCacheFile::Foreign(e.to_string()),
    }
}

/// Seed `cache` from the online cache file at `path`, healing what cannot be used.
///
/// A cache file is a regenerable artifact, so an unusable one is replaced rather
/// than turned into a failed run: the operator is told once, the file is
/// removed, and the run continues with an empty cache. A file this writer did
/// not produce is left untouched instead — it is not ours to delete. A file that
/// *is* ours but claims another chain is the one hard failure: the per-chain
/// file name already separates chains, so disagreement inside means the file was
/// swapped or the directory is shared across chains, and silently continuing
/// would hide it (the persist that follows swallows its own errors).
pub(super) fn load_online_cache(cache: &TransportCache, path: &Path, chain_id: u64) -> Result<()> {
    match classify_online_cache_file(path, chain_id) {
        OnlineCacheFile::Absent => Ok(()),
        OnlineCacheFile::Ours(entries) => {
            let count = entries.len();
            for entry in entries {
                cache.put(entry.key, entry.value, true);
            }
            debug!(path = %path.display(), entries = count, "Loaded RPC cache");
            Ok(())
        }
        // stderr via `warn_user`: default CLI tracing is off, so a `warn!`-only
        // line would never reach the operator whose directory is mispointed.
        OnlineCacheFile::Foreign(reason) => {
            warn_user(format_args!(
                "Not using the RPC cache at '{}': it {reason}. Leaving the file \
                 untouched and running with an empty cache",
                path.display(),
            ));
            Ok(())
        }
        OnlineCacheFile::Stale(reason) => {
            match fs::remove_file(path) {
                Ok(()) => warn_user(format_args!(
                    "Replaced the RPC cache at '{}': it {reason}. Starting with an \
                     empty cache",
                    path.display(),
                )),
                Err(e) => warn_user(format_args!(
                    "Ignoring the RPC cache at '{}': it {reason}, and removing it failed \
                     ({e}). Starting with an empty cache",
                    path.display(),
                )),
            }
            Ok(())
        }
        OnlineCacheFile::ChainMismatch(found) => Err(EvmeError::InvalidInput(format!(
            "RPC cache '{}' holds chain {found}, but the endpoint reports chain {chain_id}. \
             Delete the file or point --rpc.cache-dir at a directory for this chain.",
            path.display(),
        ))),
    }
}

/// Atomically persist the online transport cache to `target` via lock +
/// re-read-merge + temp rename.
///
/// Returns `Ok(true)` when the file was written, `Ok(false)` when the write was
/// intentionally skipped (a file this writer did not produce, or one claiming
/// another chain), and `Err` on lock/IO failure. All error paths include
/// `target` so the warn-log in [`RpcCacheStore::persist`] identifies which file
/// failed.
///
/// Lock acquisition failure aborts the persist: the cache is a best-effort
/// artifact, so skipping it costs a re-fetch, while an unlocked write can
/// silently delete a sibling process's entries.
fn save_online_cache_atomic(
    cache: &TransportCache,
    target: &Path,
    chain_id: u64,
) -> std::io::Result<bool> {
    let _guard = acquire_exclusive_lock(target).map_err(|e| {
        std::io::Error::other(format!(
            "failed to acquire the cache lock {} for {}: {e}; \
             cache entries were not saved (an unlocked write could drop a \
             concurrent process's entries)",
            lock_sidecar_path(target).display(),
            target.display(),
        ))
    })?;

    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| {
        std::io::Error::other(format!("failed to create directory {}: {e}", dir.display()))
    })?;

    let our_entries: Vec<CacheKv> = serde_json::from_value(cache.to_value()).map_err(|e| {
        std::io::Error::other(format!(
            "failed to encode our cache entries for {}: {e}",
            target.display()
        ))
    })?;

    // Classified under the lock, not from what load saw: a sibling process may
    // have written since, and the decision that matters is about the bytes this
    // write would replace.
    let disk_entries = match classify_online_cache_file(target, chain_id) {
        OnlineCacheFile::Absent => Vec::new(),
        OnlineCacheFile::Ours(entries) => entries,
        OnlineCacheFile::Stale(reason) => {
            warn!(
                path = %target.display(),
                reason = %reason,
                "Replacing the on-disk RPC cache; persisting our entries only",
            );
            Vec::new()
        }
        OnlineCacheFile::Foreign(reason) => {
            warn_user(format_args!(
                "Skipping RPC cache persist to '{}': it {reason}; leaving it intact",
                target.display(),
            ));
            return Ok(false);
        }
        OnlineCacheFile::ChainMismatch(found) => {
            warn_user(format_args!(
                "Skipping RPC cache persist to '{}': it holds chain {found}, not {chain_id}; \
                 leaving it intact",
                target.display(),
            ));
            return Ok(false);
        }
    };

    // The union must respect the configured cap: a sibling's file plus ours can
    // otherwise exceed what either run was allowed to keep.
    let cap = cache.max_entries().unwrap_or(usize::MAX);
    let merged = merge_provider_entries_capped(disk_entries, our_entries, cap);
    let doc = OnlineCacheDoc {
        version: ENVELOPE_VERSION,
        kind: ONLINE_CACHE_KIND,
        chain_id,
        cache: merged,
        external_env: None,
    };
    let serialized = serde_json::to_string_pretty(&doc).map_err(|e| {
        std::io::Error::other(format!(
            "failed to serialize merged cache for {}: {e}",
            target.display()
        ))
    })?;
    write_bytes_atomic(target, serialized.as_bytes())?;
    Ok(true)
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
    /// the lock is a hard error — the envelope is the primary output of capture
    /// mode, so an unlocked write that silently drops a concurrent writer's
    /// entries is worse than a failed run. Write failures remain hard errors.
    pub(super) fn save(
        &self,
        path: &Path,
        loaded_external_env: Option<&ExternalEnvSnapshot>,
    ) -> Result<()> {
        let _guard = acquire_exclusive_lock(path).map_err(|e| {
            EvmeError::FixtureError(format!(
                "Failed to acquire the cache lock {} for envelope '{}': {e}. \
                 Refusing to write it unlocked: a concurrent writer's entries would be lost.",
                lock_sidecar_path(path).display(),
                path.display(),
            ))
        })?;

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
    use alloy_primitives::keccak256;
    use alloy_provider::layers::CacheLayer;

    use super::*;

    // ── Online cache: helpers ───────────────────────────────────────────────

    /// A transport cache bounded to `max_entries` and holding `entries`.
    fn online_cache(max_entries: u32, entries: &[(B256, &str)]) -> TransportCache {
        let cache = TransportCache::with_max_entries(max_entries);
        for (key, value) in entries {
            cache.put(*key, (*value).to_string(), true);
        }
        cache
    }

    /// The entries the online cache file at `path` holds.
    fn on_disk_entries(path: &Path) -> Vec<CacheKv> {
        let raw = fs::read_to_string(path).expect("read online cache");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("online cache is JSON");
        serde_json::from_value(value["cache"].clone()).expect("decode online cache entries")
    }

    /// A capture fixture, which the online writer must never produce or replace.
    fn capture_envelope_bytes() -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "chain_id": 7,
            "cache": [{ "key": B256::repeat_byte(0xee), "value": "captured" }],
            "external_env": null,
        }))
        .expect("serialize fixture")
    }

    // ── Online cache: persist ───────────────────────────────────────────────

    /// The written file is the envelope, marked as the online cache and
    /// carrying no snapshot: the marker is what lets a later run tell its own
    /// cache from a fixture, and the snapshot's conflict protocol has no place
    /// on a path that swallows its write failures.
    #[test]
    fn test_online_cache_persist_writes_a_marked_envelope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-7.json");

        let cache = online_cache(16, &[(B256::repeat_byte(0xaa), "ours")]);
        assert!(save_online_cache_atomic(&cache, &path, 7).expect("persist"));

        let raw = fs::read_to_string(&path).expect("read");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("JSON");
        assert_eq!(doc["version"], serde_json::json!(ENVELOPE_VERSION));
        assert_eq!(doc["kind"], serde_json::json!("cache"));
        assert_eq!(doc["chain_id"], serde_json::json!(7));
        assert!(doc["external_env"].is_null(), "doc={doc}");
        assert_eq!(on_disk_entries(&path).len(), 1);
    }

    /// Persisting into a shared cache directory keeps the file within the
    /// configured cap.
    ///
    /// Runs that share a directory touch disjoint RPC keys, so merging a
    /// sibling's file in wholesale would grow it past what either run was
    /// allowed to keep, and every later start would parse all of it.
    #[test]
    fn test_online_cache_persist_respects_the_configured_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");

        // A sibling with a bigger budget fills the file first.
        let sibling_entries: Vec<(B256, String)> =
            (0..20u8).map(|i| (B256::repeat_byte(i), format!(r#"{{"result":"{i}"}}"#))).collect();
        let sibling = TransportCache::with_max_entries(64);
        for (key, value) in &sibling_entries {
            sibling.put(*key, value.clone(), true);
        }
        assert!(save_online_cache_atomic(&sibling, &path, 1).expect("persist sibling"));

        // Ours is capped at 4 and holds keys the sibling never saw.
        let mine: Vec<B256> = (100..104u8).map(B256::repeat_byte).collect();
        let ours = TransportCache::with_max_entries(4);
        for key in &mine {
            ours.put(*key, r#"{"result":"mine"}"#.to_string(), true);
        }
        assert!(save_online_cache_atomic(&ours, &path, 1).expect("persist ours"));

        let entries = on_disk_entries(&path);
        assert!(
            entries.len() <= 4,
            "the merged file must respect this run's cap, got {} entries",
            entries.len(),
        );
        for key in &mine {
            assert!(entries.iter().any(|e| e.key == *key), "this run's entries survive: {key}");
        }
    }

    /// Interleaving: A holds only key A in memory; B persists key B; A then
    /// persists — the on-disk file must contain the union (B's entries survive).
    #[test]
    fn test_online_cache_persist_merges_interleaved_disk_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");
        let (key_a, key_b) = (B256::repeat_byte(0xaa), B256::repeat_byte(0xbb));

        let cache_b = online_cache(64, &[(key_b, r#"{"result":"b"}"#)]);
        assert!(save_online_cache_atomic(&cache_b, &path, 1).expect("persist b"));

        // Process A never loaded B's write; only has key_a in memory.
        let cache_a = online_cache(64, &[(key_a, r#"{"result":"a"}"#)]);
        assert!(save_online_cache_atomic(&cache_a, &path, 1).expect("persist a"));

        let entries = on_disk_entries(&path);
        assert_eq!(entries.len(), 2, "the union survives: {entries:?}");
        assert!(entries.iter().any(|e| e.key == key_a && e.value == r#"{"result":"a"}"#));
        assert!(entries.iter().any(|e| e.key == key_b && e.value == r#"{"result":"b"}"#));
    }

    /// On collision, the process that persists last wins for that key.
    #[test]
    fn test_online_cache_persist_ours_wins_on_collision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");
        let key = B256::repeat_byte(0x01);

        assert!(save_online_cache_atomic(&online_cache(64, &[(key, "from-b")]), &path, 1)
            .expect("persist b"));
        assert!(save_online_cache_atomic(&online_cache(64, &[(key, "from-a")]), &path, 1)
            .expect("persist a"));

        let entries = on_disk_entries(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].value, "from-a");
    }

    /// Online persist fails closed on the lock: nothing is written, and the
    /// file a sibling process left behind is intact.
    ///
    /// The store swallows the failure (the cache is best-effort), so the
    /// observable contract through it is the untouched file, not a return value.
    #[test]
    fn test_online_cache_persist_skips_when_the_lock_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");

        let sibling = online_cache(16, &[(B256::repeat_byte(0xbb), "from-sibling")]);
        assert!(save_online_cache_atomic(&sibling, &path, 1).expect("persist sibling"));
        let before = fs::read_to_string(&path).expect("read sibling file");

        // A directory in the sidecar's place makes the lock un-acquirable.
        fs::remove_file(lock_sidecar_path(&path)).expect("remove sidecar");
        fs::create_dir(lock_sidecar_path(&path)).expect("occupy sidecar path");

        let ours = online_cache(16, &[(B256::repeat_byte(0xaa), "ours")]);
        let err =
            save_online_cache_atomic(&ours, &path, 1).expect_err("lock failure must not write");
        let msg = err.to_string();
        assert!(msg.contains("rpc-cache-1.json.lock"), "msg={msg}");
        assert!(msg.contains("were not saved"), "msg={msg}");

        RpcCacheStore::new_online_cache(ours, path.clone(), 1)
            .persist()
            .expect("the online persist stays best-effort");
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "no unlocked write happened");
    }

    /// Persisting onto a capture fixture must leave it intact: a shared-dir
    /// misconfiguration must not truncate a fixture to the online cap. The
    /// fixture carries no `kind`, which is exactly what identifies it as one
    /// this writer did not produce.
    #[test]
    fn test_online_cache_persist_skips_a_capture_fixture_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-7.json");
        let before = capture_envelope_bytes();
        fs::write(&path, &before).expect("seed fixture");

        let ours = online_cache(16, &[(B256::repeat_byte(0xaa), "ours")]);
        let wrote = save_online_cache_atomic(&ours, &path, 7).expect("skip is Ok(false), not Err");
        assert!(!wrote, "a file we did not write must not be overwritten");
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "fixture left intact");

        // Same skip, same intact file, no hard error through the store.
        RpcCacheStore::new_online_cache(ours, path.clone(), 7)
            .persist()
            .expect("the online persist stays best-effort on a foreign skip");
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    /// An unrecognized structured JSON shape is foreign too: skip, do not replace.
    #[test]
    fn test_online_cache_persist_skips_an_unrecognized_foreign_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");
        let before = r#"{"not":"a-cache","nor":"an-envelope"}"#;
        fs::write(&path, before).expect("seed foreign");

        let ours = online_cache(16, &[(B256::repeat_byte(0xaa), "ours")]);
        assert!(!save_online_cache_atomic(&ours, &path, 1).expect("skip is Ok"));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    /// A file claiming another chain is not written over either: the per-chain
    /// name says whose file this is, and disagreement inside it means something
    /// swapped the file after the run started.
    #[test]
    fn test_online_cache_persist_skips_a_cross_chain_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-7.json");

        let theirs = online_cache(16, &[(B256::repeat_byte(0xcc), "theirs")]);
        assert!(save_online_cache_atomic(&theirs, &path, 9).expect("persist chain 9"));
        let before = fs::read_to_string(&path).expect("read");

        let ours = online_cache(16, &[(B256::repeat_byte(0xaa), "ours")]);
        assert!(!save_online_cache_atomic(&ours, &path, 7).expect("skip is Ok"));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    /// Corrupt content at the target does not abort the persist: it is our own
    /// regenerable file, so ours replace it.
    #[test]
    fn test_online_cache_persist_replaces_corrupt_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");
        fs::write(&path, "not-json{{{").expect("corrupt");

        let key = B256::repeat_byte(0xcc);
        let ours = online_cache(16, &[(key, "ok")]);
        assert!(save_online_cache_atomic(&ours, &path, 1).expect("corrupt degrades to write"));

        let entries = on_disk_entries(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, key);
    }

    // ── Online cache: load ──────────────────────────────────────────────────

    /// A file written by a build with a different envelope version is ours by
    /// marker but unusable: it is replaced, not reported, because the cache is
    /// regenerable and a hard error would fail every run until someone deletes
    /// the file by hand.
    #[test]
    fn test_online_cache_load_replaces_an_unsupported_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-7.json");
        fs::write(&path, r#"{"version":99,"kind":"cache","chain_id":7,"cache":[]}"#).expect("seed");

        let cache = TransportCache::with_max_entries(16);
        load_online_cache(&cache, &path, 7).expect("an unsupported version must not fail the run");
        assert_eq!(cache.len(), 0);
        assert!(!path.exists(), "the unusable file is removed");
    }

    /// An unrecognized shape is left alone: it is not ours to delete, and its
    /// entries are not ours to adopt.
    #[test]
    fn test_online_cache_load_leaves_an_unrecognized_shape_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-7.json");
        let before = r#"{"not":"a-cache","nor":"an-envelope"}"#;
        fs::write(&path, before).expect("seed foreign");

        let cache = TransportCache::with_max_entries(16);
        load_online_cache(&cache, &path, 7).expect("a foreign file must not fail the run");
        assert_eq!(cache.len(), 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    /// The load/persist pair round-trips a marked cache without the entry count
    /// drifting: what one run wrote is what the next one starts from.
    #[test]
    fn test_online_cache_load_adopts_our_own_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-7.json");
        let key = B256::repeat_byte(0xaa);

        let written = online_cache(16, &[(key, "ours")]);
        assert!(save_online_cache_atomic(&written, &path, 7).expect("persist"));

        let reloaded = TransportCache::with_max_entries(16);
        load_online_cache(&reloaded, &path, 7).expect("load");
        assert_eq!(reloaded.get(&key).as_deref(), Some("ours"));
        assert_eq!(reloaded.len(), 1);
    }

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
    /// Persisting into a shared cache directory keeps the file within the
    /// configured cap.
    ///
    /// Runs that share a directory touch disjoint RPC keys, so merging a
    /// sibling's file in wholesale would grow it past what either run was
    /// allowed to keep, and every later start would parse all of it.
    #[test]
    fn test_provider_cache_persist_respects_the_configured_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");

        // A sibling with a bigger budget fills the file first.
        let sibling = CacheLayer::new(64).cache();
        for i in 0..20u8 {
            sibling.put(B256::repeat_byte(i), format!(r#"{{"result":"{i}"}}"#)).expect("put");
        }
        RpcCacheStore::new(sibling, path.clone()).persist().expect("persist sibling");

        // Ours is capped at 4 and holds keys the sibling never saw.
        let ours = CacheLayer::new(4).cache();
        let mine: Vec<B256> = (100..104u8).map(B256::repeat_byte).collect();
        for key in &mine {
            ours.put(*key, r#"{"result":"mine"}"#.to_string()).expect("put");
        }
        RpcCacheStore::new(ours, path.clone()).persist().expect("persist ours");

        let entries = crate::cache::read_provider_cache(&path).expect("read merged cache");
        assert!(
            entries.len() <= 4,
            "the merged file must respect this run's cap, got {} entries",
            entries.len()
        );
        for key in &mine {
            assert!(entries.iter().any(|e| e.key == *key), "this run's entries survive: {key}");
        }
    }

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

    /// Provider persist fails closed when the lock cannot be acquired: nothing
    /// is written, and the file a sibling process left behind is intact.
    ///
    /// The store swallows the failure (the provider cache is best-effort), so
    /// the observable contract is the untouched file, not the return value.
    #[test]
    fn test_provider_cache_persist_skips_when_the_lock_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");

        // A sibling's file is already on disk.
        let sibling = CacheLayer::new(16).cache();
        sibling.put(B256::repeat_byte(0xbb), "from-sibling".into()).expect("put");
        RpcCacheStore::new(sibling, path.clone()).persist().expect("persist sibling");
        let before = fs::read_to_string(&path).expect("read sibling file");

        // A directory in the sidecar's place makes the lock un-acquirable.
        fs::remove_file(lock_sidecar_path(&path)).expect("remove sidecar");
        fs::create_dir(lock_sidecar_path(&path)).expect("occupy sidecar path");

        let ours = CacheLayer::new(16).cache();
        ours.put(B256::repeat_byte(0xaa), "ours".into()).expect("put");
        RpcCacheStore::new(ours, path.clone())
            .persist()
            .expect("provider persist stays best-effort");

        assert_eq!(fs::read_to_string(&path).unwrap(), before, "no unlocked write happened");
    }

    /// The skipped persist reports why, naming the lock and stating that the
    /// entries were not saved.
    #[test]
    fn test_save_cache_atomic_reports_the_lock_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");
        fs::create_dir(lock_sidecar_path(&path)).expect("occupy sidecar path");

        let cache = CacheLayer::new(16).cache();
        cache.put(B256::repeat_byte(0xaa), "ours".into()).expect("put");
        let err = save_cache_atomic(&cache, &path).expect_err("lock failure must not write");
        let msg = err.to_string();
        assert!(msg.contains("rpc-cache-1.json.lock"), "msg={msg}");
        assert!(msg.contains("were not saved"), "msg={msg}");
        assert!(!path.exists(), "nothing was written");
    }

    /// Persisting a provider cache onto a capture envelope must leave the
    /// envelope intact: a shared-dir misconfiguration must not destroy a
    /// foreign file the provider shape cannot fold.
    #[test]
    fn test_provider_cache_persist_skips_foreign_envelope_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");

        let envelope = serde_json::json!({
            "version": 1,
            "chain_id": 7,
            "cache": [],
            "external_env": null,
        });
        let before = serde_json::to_string_pretty(&envelope).unwrap();
        fs::write(&path, &before).expect("seed envelope");

        let cache = CacheLayer::new(16).cache();
        cache.put(B256::repeat_byte(0xaa), "ours".into()).expect("put");
        let wrote = save_cache_atomic(&cache, &path).expect("skip is Ok(false), not Err");
        assert!(!wrote, "foreign shape must not be overwritten");
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "envelope left intact");

        // Store path is best-effort: same skip, same intact file, no hard error.
        let store_cache = CacheLayer::new(16).cache();
        store_cache.put(B256::repeat_byte(0xbb), "store".into()).expect("put");
        RpcCacheStore::new(store_cache, path.clone())
            .persist()
            .expect("provider persist stays best-effort on foreign skip");
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "store path also leaves envelope");
    }

    /// An unrecognized structured JSON shape is also foreign: skip, do not replace.
    #[test]
    fn test_provider_cache_persist_skips_unrecognized_foreign_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-1.json");
        let before = r#"{"not":"a-provider-cache","nor":"an-envelope"}"#;
        fs::write(&path, before).expect("seed foreign");

        let cache = CacheLayer::new(16).cache();
        cache.put(B256::repeat_byte(0xaa), "ours".into()).expect("put");
        let wrote = save_cache_atomic(&cache, &path).expect("skip is Ok");
        assert!(!wrote);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }

    /// Envelope persist hard-errors when the lock cannot be acquired: the
    /// capture is the primary output, so a silently unlocked write is worse
    /// than a failed run.
    #[test]
    fn test_envelope_persist_errors_when_the_lock_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");

        CacheFileEnvelope::new(&TransportCache::new(), 7, None).save(&path, None).expect("seed");
        let before = fs::read_to_string(&path).expect("read seeded envelope");

        fs::remove_file(lock_sidecar_path(&path)).expect("remove sidecar");
        fs::create_dir(lock_sidecar_path(&path)).expect("occupy sidecar path");

        let cache = TransportCache::new();
        cache
            .merge(&serde_json::json!([{
                "key": keccak256("a"),
                "value": r#"{"result":"a"}"#,
            }]))
            .expect("seed ours");
        let err = CacheFileEnvelope::new(&cache, 7, None)
            .save(&path, None)
            .expect_err("lock failure must abort the capture persist");
        let msg = err.to_string();
        assert!(msg.contains("capture.json.lock"), "msg={msg}");
        assert!(msg.contains("unlocked"), "msg={msg}");
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "no unlocked write happened");
    }

    /// The store surfaces the envelope lock failure to its caller.
    #[test]
    fn test_store_envelope_persist_errors_when_the_lock_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capture.json");
        fs::create_dir(lock_sidecar_path(&path)).expect("occupy sidecar path");

        let store = RpcCacheStore::new_envelope(TransportCache::new(), path.clone(), 7, None);
        let err = store.persist().expect_err("capture persist must fail closed");
        assert!(err.to_string().contains("lock"), "msg={err}");
        assert!(!path.exists(), "nothing was written");
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
        let wrote = save_cache_atomic(&cache, &path).expect("corrupt degrades to write");
        assert!(wrote, "corrupt target is replaced with ours");

        let loaded = CacheLayer::new(16).cache();
        loaded.load_cache(path).expect("load");
        assert_eq!(loaded.get(&key).as_deref(), Some("ok"));
    }
}
