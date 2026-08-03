//! Pure merge helpers for provider-cache and capture-envelope JSON shapes.
//!
//! Used by the `cache merge` subcommand and by lock-protected merge-on-persist
//! in [`crate::common::provider`]'s cache store.

use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::common::{EvmeError, Result};

/// Current on-disk envelope schema version (must match capture/replay).
pub(crate) const ENVELOPE_VERSION: u32 = 1;

/// One `{key, value}` entry shared by provider-cache files and envelope `cache` arrays.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CacheKv {
    /// Request fingerprint (typically `keccak256` of method + params).
    pub key: B256,
    /// Serialized JSON-RPC response body.
    pub value: String,
}

/// Detected on-disk shape of a cache file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheShape {
    /// JSON array of `{key, value}` (provider `--rpc.cache-dir` files).
    Provider,
    /// `{version, chain_id, cache, external_env?}` capture envelope.
    Envelope,
}

/// Minimal envelope view used for merge (independent of the store type).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct EnvelopeDoc {
    /// Schema version (must match [`ENVELOPE_VERSION`] for this build).
    pub version: u32,
    /// Chain ID recorded at capture time.
    pub chain_id: u64,
    /// Transport-level cache entries.
    pub cache: Vec<CacheKv>,
    /// Optional external-env snapshot (SALT buckets, …).
    #[serde(default)]
    pub external_env: Option<ExternalEnvDoc>,
}

/// External-env snapshot fields needed for envelope merge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ExternalEnvDoc {
    /// SALT bucket capacity pairs `(bucket_id, capacity)`.
    #[serde(default)]
    pub bucket_capacities: Vec<(u32, u64)>,
}

/// Path of the advisory lock sidecar for `target` (`<target>.lock`).
pub(crate) fn lock_sidecar_path(target: &Path) -> PathBuf {
    let mut os = target.as_os_str().to_owned();
    os.push(".lock");
    PathBuf::from(os)
}

/// Parse `rpc-cache-{chain_id}.json` from a path's file name.
///
/// Returns `None` when the name does not match the per-chain provider-cache
/// convention (so callers can warn that chain identity cannot be validated).
pub(crate) fn parse_rpc_cache_filename_chain_id(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("rpc-cache-")?.strip_suffix(".json")?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // Reject leading zeros (except the single digit `0`) so `rpc-cache-01.json`
    // is not treated as chain 1 under a different spelling.
    if rest.len() > 1 && rest.starts_with('0') {
        return None;
    }
    rest.parse().ok()
}

/// Union `base` with `overlay` by key; overlay wins on collision.
///
/// Output is sorted by key for deterministic files.
pub(crate) fn merge_kv_entries(base: Vec<CacheKv>, overlay: Vec<CacheKv>) -> Vec<CacheKv> {
    let mut map: BTreeMap<B256, String> = BTreeMap::new();
    for e in base {
        map.insert(e.key, e.value);
    }
    for e in overlay {
        map.insert(e.key, e.value);
    }
    map.into_iter().map(|(key, value)| CacheKv { key, value }).collect()
}

/// Detect whether `value` is a provider-cache array or a capture envelope.
pub(crate) fn detect_shape(value: &serde_json::Value, path: &Path) -> Result<CacheShape> {
    if value.is_array() {
        // Validate array elements look like {key, value} when non-empty.
        if let Some(arr) = value.as_array() {
            for (i, el) in arr.iter().enumerate() {
                if !el.is_object() || el.get("key").is_none() || el.get("value").is_none() {
                    return Err(EvmeError::InvalidInput(format!(
                        "Provider-cache entry {i} in '{}' is not a {{key, value}} object",
                        path.display()
                    )));
                }
            }
        }
        return Ok(CacheShape::Provider);
    }
    if let Some(obj) = value.as_object() {
        if obj.contains_key("version") && obj.contains_key("chain_id") && obj.contains_key("cache")
        {
            return Ok(CacheShape::Envelope);
        }
    }
    Err(EvmeError::InvalidInput(format!(
        "Unrecognized cache file shape in '{}': expected a JSON array of {{key, value}} \
         or a capture envelope {{version, chain_id, cache, ...}}",
        path.display()
    )))
}

/// Read and parse a provider-cache file (JSON array). Missing file → empty vec.
///
/// Corrupt / unreadable content returns `Err` so callers can degrade or hard-fail.
pub(crate) fn read_provider_cache(path: &Path) -> Result<Vec<CacheKv>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path).map_err(|e| {
        EvmeError::InvalidInput(format!("Failed to read cache file {}: {e}", path.display()))
    })?;
    let value: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
        EvmeError::InvalidInput(format!("Failed to parse cache file {}: {e}", path.display()))
    })?;
    match detect_shape(&value, path)? {
        CacheShape::Provider => serde_json::from_value(value).map_err(|e| {
            EvmeError::InvalidInput(format!(
                "Failed to decode provider-cache entries in {}: {e}",
                path.display()
            ))
        }),
        CacheShape::Envelope => Err(EvmeError::InvalidInput(format!(
            "Expected provider-cache array in '{}', found capture envelope",
            path.display()
        ))),
    }
}

/// Classification of an envelope re-read during concurrent persist merge.
///
/// Typed so hard identity failures (version / `chain_id` / wrong shape) are not
/// confused with corrupt JSON merely because a path or message contains those
/// substrings.
#[derive(Debug)]
pub(crate) enum EnvelopeReread {
    /// Successfully parsed and version-validated envelope.
    Ok(EnvelopeDoc),
    /// Corrupt, unreadable, or undecodable content — safe to warn and replace.
    Degradable(String),
    /// Schema / identity failure that must abort the capture persist.
    Hard(EvmeError),
}

/// Re-read an on-disk envelope for the lock-protected merge-on-persist path.
///
/// Distinguishes hard identity failures from degradable corrupt content without
/// substring-searching formatted messages.
pub(crate) fn reread_envelope_for_merge(path: &Path) -> EnvelopeReread {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return EnvelopeReread::Degradable(format!(
                "Failed to read envelope {}: {e}",
                path.display()
            ));
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            return EnvelopeReread::Degradable(format!(
                "Failed to parse envelope {}: {e}",
                path.display()
            ));
        }
    };
    let shape = match detect_shape(&value, path) {
        Ok(s) => s,
        Err(e) => {
            // Unrecognized shape is a hard identity/schema failure: the on-disk
            // file is not a capture envelope this build can merge into.
            return EnvelopeReread::Hard(EvmeError::FixtureError(e.to_string()));
        }
    };
    match shape {
        CacheShape::Envelope => {
            let doc: EnvelopeDoc = match serde_json::from_value(value) {
                Ok(d) => d,
                Err(e) => {
                    return EnvelopeReread::Degradable(format!(
                        "Failed to decode envelope {}: {e}",
                        path.display()
                    ));
                }
            };
            if doc.version != ENVELOPE_VERSION {
                return EnvelopeReread::Hard(EvmeError::FixtureError(format!(
                    "Unsupported cache file version {} in '{}'; expected {ENVELOPE_VERSION}",
                    doc.version,
                    path.display(),
                )));
            }
            EnvelopeReread::Ok(doc)
        }
        CacheShape::Provider => EnvelopeReread::Hard(EvmeError::FixtureError(format!(
            "Expected capture envelope in '{}', found provider-cache array",
            path.display()
        ))),
    }
}

/// Merge two provider-cache entry lists (overlay wins).
pub(crate) fn merge_provider_lists(base: Vec<CacheKv>, overlay: Vec<CacheKv>) -> Vec<CacheKv> {
    merge_kv_entries(base, overlay)
}

/// Merge `ours` over `on_disk` for envelope persist (ours wins on key collision).
///
/// Returns an error if `chain_id` or `version` disagree, or if both sides carry
/// non-null `external_env` snapshots that are not identical (same rule as
/// [`merge_envelopes_cli`]). One-sided or identical snapshots are accepted:
/// ours is kept when set, otherwise the on-disk snapshot is propagated.
pub(crate) fn merge_envelope_for_persist(
    on_disk: &EnvelopeDoc,
    ours: &EnvelopeDoc,
    path: &Path,
) -> Result<EnvelopeDoc> {
    if on_disk.version != ours.version {
        return Err(EvmeError::FixtureError(format!(
            "Envelope version mismatch when merging '{}': on-disk {}, ours {}",
            path.display(),
            on_disk.version,
            ours.version,
        )));
    }
    if on_disk.chain_id != ours.chain_id {
        return Err(EvmeError::FixtureError(format!(
            "Envelope chain_id mismatch when merging '{}': on-disk {}, ours {}",
            path.display(),
            on_disk.chain_id,
            ours.chain_id,
        )));
    }
    let external_env = match (&ours.external_env, &on_disk.external_env) {
        (Some(a), Some(b)) if a != b => {
            return Err(EvmeError::FixtureError(format!(
                "Conflicting external_env snapshots when merging '{}': \
                 on-disk {on_disk_env:?}, ours {ours_env:?}",
                path.display(),
                on_disk_env = on_disk.external_env,
                ours_env = ours.external_env,
            )));
        }
        (Some(a), _) => Some(a.clone()),
        (None, disk) => disk.clone(),
    };
    Ok(EnvelopeDoc {
        version: ours.version,
        chain_id: ours.chain_id,
        cache: merge_kv_entries(on_disk.cache.clone(), ours.cache.clone()),
        external_env,
    })
}

/// Merge multiple envelope inputs for the `cache merge` subcommand.
///
/// All inputs must share `version` and `chain_id`. Later inputs win on cache
/// key collision. Non-null `external_env` values must be identical when more
/// than one is present.
pub(crate) fn merge_envelopes_cli(docs: &[(PathBuf, EnvelopeDoc)]) -> Result<EnvelopeDoc> {
    let Some((_, first)) = docs.first() else {
        return Err(EvmeError::InvalidInput("cache merge requires at least one input file".into()));
    };
    let version = first.version;
    let chain_id = first.chain_id;
    if version != ENVELOPE_VERSION {
        return Err(EvmeError::InvalidInput(format!(
            "Unsupported envelope version {version} in '{}'; expected {ENVELOPE_VERSION}",
            docs[0].0.display(),
        )));
    }

    let mut cache = Vec::new();
    let mut external_env: Option<ExternalEnvDoc> = None;

    for (path, doc) in docs {
        if doc.version != version {
            return Err(EvmeError::InvalidInput(format!(
                "Envelope version mismatch: '{}' has version {}, expected {version}",
                path.display(),
                doc.version,
            )));
        }
        if doc.chain_id != chain_id {
            return Err(EvmeError::InvalidInput(format!(
                "Envelope chain_id mismatch: '{}' has chain_id {}, expected {chain_id}",
                path.display(),
                doc.chain_id,
            )));
        }
        cache = merge_kv_entries(cache, doc.cache.clone());
        if let Some(ref ext) = doc.external_env {
            match &external_env {
                None => external_env = Some(ext.clone()),
                Some(prev) if prev != ext => {
                    return Err(EvmeError::InvalidInput(format!(
                        "Conflicting external_env snapshots while merging '{}'",
                        path.display(),
                    )));
                }
                Some(_) => {}
            }
        }
    }

    Ok(EnvelopeDoc { version, chain_id, cache, external_env })
}

/// Atomically write `entries` as a provider-cache JSON array to `path`.
pub(crate) fn write_provider_cache_atomic(path: &Path, entries: &[CacheKv]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| {
        EvmeError::InvalidInput(format!("Failed to create directory {}: {e}", dir.display()))
    })?;
    let serialized = serde_json::to_vec(entries)
        .map_err(|e| EvmeError::InvalidInput(format!("Failed to serialize provider cache: {e}")))?;
    write_bytes_atomic(path, &serialized)
        .map_err(|e| EvmeError::InvalidInput(format!("Failed to write {}: {e}", path.display())))
}

/// Atomically write an envelope document (pretty-printed, matching capture).
pub(crate) fn write_envelope_atomic(path: &Path, doc: &EnvelopeDoc) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(|e| {
        EvmeError::FixtureError(format!(
            "Failed to create cache file directory {}: {e}",
            dir.display()
        ))
    })?;
    let serialized = serde_json::to_string_pretty(doc).map_err(|e| {
        EvmeError::FixtureError(format!("Failed to serialize envelope for {}: {e}", path.display()))
    })?;
    write_bytes_atomic(path, serialized.as_bytes()).map_err(|e| {
        EvmeError::FixtureError(format!("Failed to persist envelope to {}: {e}", path.display()))
    })
}

/// Temp-file + rename write.
pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| {
        std::io::Error::other(format!("failed to create temp file in {}: {e}", dir.display()))
    })?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| {
        std::io::Error::other(format!(
            "failed to rename temp file into {}: {}",
            path.display(),
            e.error,
        ))
    })?;
    Ok(())
}

/// Load any supported cache file and return its shape + entry count.
pub(crate) fn load_cache_file(path: &Path) -> Result<(CacheShape, LoadedCache)> {
    let content = fs::read_to_string(path)
        .map_err(|e| EvmeError::InvalidInput(format!("Failed to read {}: {e}", path.display())))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| EvmeError::InvalidInput(format!("Failed to parse {}: {e}", path.display())))?;
    let shape = detect_shape(&value, path)?;
    match shape {
        CacheShape::Provider => {
            let entries: Vec<CacheKv> = serde_json::from_value(value).map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Failed to decode provider cache {}: {e}",
                    path.display()
                ))
            })?;
            Ok((shape, LoadedCache::Provider(entries)))
        }
        CacheShape::Envelope => {
            let doc: EnvelopeDoc = serde_json::from_value(value).map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Failed to decode envelope {}: {e}",
                    path.display()
                ))
            })?;
            if doc.version != ENVELOPE_VERSION {
                return Err(EvmeError::InvalidInput(format!(
                    "Unsupported cache file version {} in '{}'; expected {ENVELOPE_VERSION}",
                    doc.version,
                    path.display(),
                )));
            }
            Ok((shape, LoadedCache::Envelope(doc)))
        }
    }
}

/// Parsed cache file payload.
#[derive(Debug)]
pub(crate) enum LoadedCache {
    Provider(Vec<CacheKv>),
    Envelope(EnvelopeDoc),
}

impl LoadedCache {
    pub(crate) fn entry_count(&self) -> usize {
        match self {
            Self::Provider(e) => e.len(),
            Self::Envelope(d) => d.cache.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(byte: u8, val: &str) -> CacheKv {
        CacheKv { key: B256::repeat_byte(byte), value: val.to_string() }
    }

    #[test]
    fn test_merge_kv_union_and_ours_wins() {
        let base = vec![kv(1, "a"), kv(2, "b")];
        let overlay = vec![kv(2, "B"), kv(3, "c")];
        let merged = merge_kv_entries(base, overlay);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0], kv(1, "a"));
        assert_eq!(merged[1], kv(2, "B")); // overlay wins
        assert_eq!(merged[2], kv(3, "c"));
    }

    #[test]
    fn test_detect_shape_provider_and_envelope() {
        let arr = serde_json::json!([{"key": B256::ZERO, "value": "x"}]);
        assert_eq!(detect_shape(&arr, Path::new("p.json")).unwrap(), CacheShape::Provider);
        let env = serde_json::json!({
            "version": 1,
            "chain_id": 1,
            "cache": []
        });
        assert_eq!(detect_shape(&env, Path::new("e.json")).unwrap(), CacheShape::Envelope);
    }

    #[test]
    fn test_merge_envelope_for_persist_union_and_ext() {
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "disk")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10)] }),
        };
        let ours = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "ours"), kv(2, "new")],
            external_env: None,
        };
        let merged = merge_envelope_for_persist(&on_disk, &ours, Path::new("x.json")).unwrap();
        assert_eq!(merged.cache, vec![kv(1, "ours"), kv(2, "new")]);
        assert_eq!(merged.external_env, Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10)] }));
    }

    #[test]
    fn test_merge_envelope_for_persist_chain_id_mismatch() {
        let on_disk = EnvelopeDoc { version: 1, chain_id: 1, cache: vec![], external_env: None };
        let ours = EnvelopeDoc { version: 1, chain_id: 2, cache: vec![], external_env: None };
        let err = merge_envelope_for_persist(&on_disk, &ours, Path::new("x.json")).unwrap_err();
        assert!(err.to_string().contains("chain_id"));
    }

    /// Conflicting non-null `external_env` snapshots hard-error and name both values.
    #[test]
    fn test_merge_envelope_for_persist_rejects_conflicting_external_env() {
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "disk")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10)] }),
        };
        let ours = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(2, "ours")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 99)] }),
        };
        let err =
            merge_envelope_for_persist(&on_disk, &ours, Path::new("capture.json")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("external_env"), "msg={msg}");
        assert!(msg.contains("on-disk"), "msg={msg}");
        assert!(msg.contains("ours"), "msg={msg}");
        // Both snapshots named in the message (Debug form of bucket capacities).
        assert!(msg.contains("10") && msg.contains("99"), "msg={msg}");
    }

    /// Identical non-null `external_env` snapshots merge successfully.
    #[test]
    fn test_merge_envelope_for_persist_identical_external_env() {
        let ext = ExternalEnvDoc { bucket_capacities: vec![(1, 10), (2, 20)] };
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "disk")],
            external_env: Some(ext.clone()),
        };
        let ours = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(2, "ours")],
            external_env: Some(ext.clone()),
        };
        let merged = merge_envelope_for_persist(&on_disk, &ours, Path::new("x.json")).unwrap();
        assert_eq!(merged.cache, vec![kv(1, "disk"), kv(2, "ours")]);
        assert_eq!(merged.external_env, Some(ext));
    }

    /// One-sided `external_env` propagates the non-null snapshot (either side).
    #[test]
    fn test_merge_envelope_for_persist_one_sided_external_env() {
        let ext = ExternalEnvDoc { bucket_capacities: vec![(3, 30)] };
        // Ours None, disk Some → disk propagates (covered by existing union test
        // for the reverse orientation; re-assert disk-propagates here).
        let on_disk =
            EnvelopeDoc { version: 1, chain_id: 1, cache: vec![], external_env: Some(ext.clone()) };
        let ours =
            EnvelopeDoc { version: 1, chain_id: 1, cache: vec![kv(1, "a")], external_env: None };
        let merged = merge_envelope_for_persist(&on_disk, &ours, Path::new("x.json")).unwrap();
        assert_eq!(merged.external_env, Some(ext.clone()));

        // Ours Some, disk None → ours kept.
        let on_disk = EnvelopeDoc { version: 1, chain_id: 1, cache: vec![], external_env: None };
        let ours = EnvelopeDoc {
            version: 1,
            chain_id: 1,
            cache: vec![kv(1, "a")],
            external_env: Some(ext.clone()),
        };
        let merged = merge_envelope_for_persist(&on_disk, &ours, Path::new("x.json")).unwrap();
        assert_eq!(merged.external_env, Some(ext));
    }

    #[test]
    fn test_merge_envelopes_cli_conflict_external_env() {
        let a = EnvelopeDoc {
            version: 1,
            chain_id: 1,
            cache: vec![kv(1, "a")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 1)] }),
        };
        let b = EnvelopeDoc {
            version: 1,
            chain_id: 1,
            cache: vec![kv(2, "b")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 2)] }),
        };
        let docs = vec![(PathBuf::from("a.json"), a), (PathBuf::from("b.json"), b)];
        let err = merge_envelopes_cli(&docs).unwrap_err();
        assert!(err.to_string().contains("external_env"));
    }

    #[test]
    fn test_lock_sidecar_path_suffix() {
        let p = Path::new("/tmp/rpc-cache-1.json");
        assert_eq!(lock_sidecar_path(p), PathBuf::from("/tmp/rpc-cache-1.json.lock"));
    }

    /// Filename-derived chain id for the standard provider-cache naming scheme.
    #[test]
    fn test_parse_rpc_cache_filename_chain_id() {
        assert_eq!(parse_rpc_cache_filename_chain_id(Path::new("rpc-cache-1.json")), Some(1));
        assert_eq!(
            parse_rpc_cache_filename_chain_id(Path::new("/tmp/rpc-cache-4326.json")),
            Some(4326)
        );
        assert_eq!(
            parse_rpc_cache_filename_chain_id(Path::new("worker/rpc-cache-11155420.json")),
            Some(11_155_420)
        );
        // Non-matching names cannot be validated from the filename alone.
        assert_eq!(parse_rpc_cache_filename_chain_id(Path::new("out.json")), None);
        assert_eq!(parse_rpc_cache_filename_chain_id(Path::new("rpc-cache.json")), None);
        assert_eq!(parse_rpc_cache_filename_chain_id(Path::new("rpc-cache-abc.json")), None);
        assert_eq!(parse_rpc_cache_filename_chain_id(Path::new("cache-4326.json")), None);
    }
}
