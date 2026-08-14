//! Pure merge helpers for the on-disk cache envelope.
//!
//! Used by the `cache merge` subcommand and by lock-protected merge-on-persist
//! in [`crate::common::provider`]'s cache store.
//!
//! Every file mega-evme reads or writes is the envelope. The bare `[{key,
//! value}]` array a retired build wrote is still *recognized* — see
//! [`CacheShape::Provider`] — but only so a diagnostic can name it; nothing here
//! reads, merges, or writes that shape.

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

/// One `{key, value}` entry of an envelope's `cache` array.
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
    /// The bare JSON array of `{key, value}` a retired build wrote for
    /// `--rpc.cache-dir`.
    ///
    /// Recognized, never read: the array stores hashed request keys, this build
    /// hashes requests differently, and the method and params behind each entry
    /// are not recoverable from the file. The variant exists so a diagnostic can
    /// tell the operator what the file is instead of calling it corrupt.
    Provider,
    /// `{version, chain_id, cache, external_env?}` envelope — the only shape
    /// this build reads or writes.
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

impl ExternalEnvDoc {
    /// Canonical form used for equality and on-disk writes.
    ///
    /// Deduplicates by bucket id with last-wins (matching runtime map-insert
    /// semantics when applying `--bucket-capacity`), then sorts by bucket id so
    /// two workers with the same effective capacities never conflict solely
    /// because of CLI order.
    pub(crate) fn canonicalized(&self) -> Self {
        Self { bucket_capacities: canonicalize_bucket_capacities(&self.bucket_capacities) }
    }
}

/// Deduplicate by bucket id (last-wins), then sort by bucket id.
pub(crate) fn canonicalize_bucket_capacities(caps: &[(u32, u64)]) -> Vec<(u32, u64)> {
    let mut map = BTreeMap::new();
    for &(id, capacity) in caps {
        map.insert(id, capacity);
    }
    map.into_iter().collect()
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

/// Merge `ours` over `on_disk` for a capped cache file, keeping at most `cap`
/// entries.
///
/// `--rpc.cache-max-entries` bounds what a run persists, so the union of a
/// sibling's file and ours must be bounded too: runs that share a cache
/// directory but touch disjoint RPC keys would otherwise grow the file without
/// limit, and every later start would parse all of it before the in-memory LRU
/// could evict anything.
///
/// This process's entries are kept first — they are already LRU-bounded by the
/// same cap, and they are the ones this run just proved it needs. On-disk
/// entries then fill whatever room is left.
pub(crate) fn merge_cache_entries_capped(
    on_disk: Vec<CacheKv>,
    ours: Vec<CacheKv>,
    cap: usize,
) -> Vec<CacheKv> {
    let mut map: BTreeMap<B256, String> = BTreeMap::new();
    for e in ours.into_iter().take(cap) {
        map.insert(e.key, e.value);
    }
    for e in on_disk {
        if map.len() >= cap {
            break;
        }
        map.entry(e.key).or_insert(e.value);
    }
    map.into_iter().map(|(key, value)| CacheKv { key, value }).collect()
}

/// Classify `value` as the envelope this build uses or the retired array format.
///
/// `Err` means neither: structured JSON somebody else wrote, which no caller may
/// adopt or overwrite.
pub(crate) fn detect_shape(value: &serde_json::Value, path: &Path) -> Result<CacheShape> {
    if value.is_array() {
        // An array of anything else is not the retired format either — it is
        // some other tool's file, and falls through to the unrecognized arm.
        if let Some(arr) = value.as_array() {
            for (i, el) in arr.iter().enumerate() {
                if !el.is_object() || el.get("key").is_none() || el.get("value").is_none() {
                    return Err(EvmeError::InvalidInput(format!(
                        "Unrecognized cache file shape in '{}': entry {i} of the JSON \
                         array is not a {{key, value}} object",
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
        "Unrecognized cache file shape in '{}': expected a cache envelope \
         {{version, chain_id, cache, ...}}",
        path.display()
    )))
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
        CacheShape::Provider => {
            EnvelopeReread::Hard(EvmeError::FixtureError(retired_array_format_message(path)))
        }
    }
}

/// The one diagnostic a reader owes an operator whose file is not an envelope.
///
/// Deliberately a single message for every non-envelope input rather than one
/// per shape. The overwhelmingly likely case is the `[{key, value}]` array a
/// retired build wrote, and that file cannot be converted: it stores only hashed
/// request keys, this build hashes requests differently, and the method and
/// params behind each entry are unrecoverable from the file. Naming a repair
/// that cannot exist would be worse than naming the two ways forward, and those
/// two ways are the same whatever the file turns out to be.
fn retired_array_format_message(path: &Path) -> String {
    format!(
        "'{}' is not a cache envelope ({{version, chain_id, cache, ...}}). It may be \
         an RPC cache from an older mega-evme (a bare JSON array), a format this \
         build no longer reads: such a file stores only hashed request keys, and \
         this build hashes requests differently, so the method and params behind \
         each entry cannot be recovered. Delete it, or re-record the responses \
         with --rpc.capture-file.",
        path.display(),
    )
}

/// Merge `ours` over `on_disk` for envelope persist (ours wins on key collision).
///
/// Returns an error if `chain_id` or `version` disagree.
///
/// `external_env` uses optimistic concurrency against `loaded_external_env` — the
/// snapshot observed when this process opened the capture file (or `None` when
/// the file was absent / had no snapshot). See
/// [`resolve_external_env_for_persist`] for the full decision table; only a true
/// concurrent conflict is a hard error, and the message then names all three
/// values (loaded, ours, on-disk).
pub(crate) fn merge_envelope_for_persist(
    on_disk: &EnvelopeDoc,
    ours: &EnvelopeDoc,
    loaded_external_env: Option<&ExternalEnvDoc>,
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
    let external_env = resolve_external_env_for_persist(
        &ours.external_env,
        &on_disk.external_env,
        loaded_external_env,
        path,
    )?;
    Ok(EnvelopeDoc {
        version: ours.version,
        chain_id: ours.chain_id,
        cache: merge_kv_entries(on_disk.cache.clone(), ours.cache.clone()),
        external_env,
    })
}

/// Resolve the envelope `external_env` under optimistic concurrency.
///
/// Three inputs decide the outcome: `loaded` (the snapshot this process observed
/// when it opened the file), `ours` (what this process would write), and
/// `on_disk` (what the locked re-read found). All three are canonicalized first,
/// so CLI ordering alone never decides anything.
///
/// | `ours` | `on_disk` | relation                     | result    | why                                                              |
/// | ------ | --------- | ---------------------------- | --------- | ---------------------------------------------------------------- |
/// | `None` | any       | —                            | `on_disk` | this run has no snapshot to contribute                           |
/// | `Some` | `None`    | —                            | `ours`    | nothing on disk to disagree with                                 |
/// | `Some` | `Some`    | equal                        | `ours`    | no decision to make                                              |
/// | `Some` | `Some`    | differ, `ours == loaded`     | `on_disk` | this run changed nothing: sibling's refresh wins                 |
/// | `Some` | `Some`    | differ, `on_disk == loaded`  | `ours`    | nobody wrote since load: our intentional refresh wins            |
/// | `Some` | `Some`    | differ, neither              | `Err`     | true conflict: two runs changed the same snapshot differently    |
///
/// The last two conditions are mutually exclusive, so their order does not
/// matter: if both held, `ours` and `on_disk` would each equal `loaded` and
/// therefore each other, contradicting "differ".
///
/// Row four carries as much weight as row six. A capture run given no
/// `--bucket-capacity` carries the previous snapshot forward verbatim, so
/// `ours == loaded` means "changed nothing", not "chose this value". Calling
/// that a conflict fails the persist — and because capture persistence is a
/// hard error, that discards every RPC response the run captured, over metadata
/// the run never had a stake in.
///
/// The predicate is value equality, not "was the flag passed": persist has no
/// record of the caller's argv, so re-asserting the values already in force is
/// indistinguishable from carrying them forward, and both yield.
fn resolve_external_env_for_persist(
    ours: &Option<ExternalEnvDoc>,
    on_disk: &Option<ExternalEnvDoc>,
    loaded: Option<&ExternalEnvDoc>,
    path: &Path,
) -> Result<Option<ExternalEnvDoc>> {
    let ours_c = ours.as_ref().map(ExternalEnvDoc::canonicalized);
    let disk_c = on_disk.as_ref().map(ExternalEnvDoc::canonicalized);
    let loaded_c = loaded.map(ExternalEnvDoc::canonicalized);

    match (&ours_c, &disk_c) {
        (Some(o), Some(d)) if o != d => {
            let ours_carried_forward = loaded_c.as_ref() == Some(o);
            let disk_unchanged_since_load = loaded_c.as_ref() == Some(d);
            if ours_carried_forward {
                Ok(disk_c)
            } else if disk_unchanged_since_load {
                Ok(ours_c)
            } else {
                Err(EvmeError::FixtureError(format!(
                    "Conflicting external_env snapshots when merging '{}': \
                     loaded {loaded_env:?}, ours {ours_env:?}, on-disk {on_disk_env:?}",
                    path.display(),
                    loaded_env = loaded_c,
                    ours_env = ours_c,
                    on_disk_env = disk_c,
                )))
            }
        }
        (Some(_), _) => Ok(ours_c),
        (None, disk) => Ok(disk.clone()),
    }
}

/// Merge multiple envelope inputs for the `cache merge` subcommand.
///
/// All inputs must share `version` and `chain_id`. Later inputs win on cache
/// key collision. Non-null `external_env` values must be identical (after
/// canonicalization) when more than one is present; the written snapshot is
/// always the canonical form.
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
            let canon = ext.canonicalized();
            match &external_env {
                None => external_env = Some(canon),
                Some(prev) if prev != &canon => {
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

/// Fold the envelope already on disk at the merge output into `merged_inputs`.
///
/// Called by `cache merge` under the output's exclusive lock, so `on_disk` is
/// whatever a concurrent writer left behind while this merge waited. It joins
/// the union as one more input under the same rules the CLI merge applies to
/// its inputs: identity must agree, entries union by key, and non-null
/// `external_env` snapshots must be identical after canonicalization.
///
/// The merge's own inputs win on key collision — the operator named those
/// files, and this matches the ours-win rule the persist path uses for the
/// same read-modify-write cycle. Errors name the output path, since that is
/// the file the caller did not list on the command line.
pub(crate) fn fold_output_envelope(
    output: &Path,
    on_disk: EnvelopeDoc,
    merged_inputs: EnvelopeDoc,
) -> Result<EnvelopeDoc> {
    if on_disk.version != merged_inputs.version {
        return Err(EvmeError::InvalidInput(format!(
            "Envelope version mismatch with existing output '{}': output has version {}, \
             inputs have version {}",
            output.display(),
            on_disk.version,
            merged_inputs.version,
        )));
    }
    if on_disk.chain_id != merged_inputs.chain_id {
        return Err(EvmeError::InvalidInput(format!(
            "Envelope chain_id mismatch with existing output '{}': output has chain_id {}, \
             inputs have chain_id {}",
            output.display(),
            on_disk.chain_id,
            merged_inputs.chain_id,
        )));
    }

    let external_env = match (&on_disk.external_env, &merged_inputs.external_env) {
        (Some(disk), Some(ours)) => {
            let (disk_c, ours_c) = (disk.canonicalized(), ours.canonicalized());
            if disk_c != ours_c {
                return Err(EvmeError::InvalidInput(format!(
                    "Conflicting external_env snapshots with existing output '{}': \
                     output {disk_c:?}, inputs {ours_c:?}",
                    output.display(),
                )));
            }
            Some(ours_c)
        }
        (Some(disk), None) => Some(disk.canonicalized()),
        (None, Some(ours)) => Some(ours.canonicalized()),
        (None, None) => None,
    };

    Ok(EnvelopeDoc {
        version: merged_inputs.version,
        chain_id: merged_inputs.chain_id,
        cache: merge_kv_entries(on_disk.cache, merged_inputs.cache),
        external_env,
    })
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
    // flush() only clears the userspace buffer. Without sync_all() a crash
    // between write and rename can publish a truncated file under the target
    // name — the rename is atomic, the contents are not.
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| {
        std::io::Error::other(format!(
            "failed to rename temp file into {}: {}",
            path.display(),
            e.error,
        ))
    })?;
    Ok(())
}

/// Load a `cache merge` input, which must be an envelope.
///
/// Anything else — the retired array format included — is rejected with the one
/// diagnostic that names what the file probably is and what to do about it. A
/// merge of the old arrays is not withheld here, it is impossible: the shapes
/// key their entries differently, so a union of the two would be a file no
/// build can serve ([`retired_array_format_message`]).
pub(crate) fn load_cache_file(path: &Path) -> Result<EnvelopeDoc> {
    let content = fs::read_to_string(path)
        .map_err(|e| EvmeError::InvalidInput(format!("Failed to read {}: {e}", path.display())))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| EvmeError::InvalidInput(format!("Failed to parse {}: {e}", path.display())))?;
    if !matches!(detect_shape(&value, path), Ok(CacheShape::Envelope)) {
        return Err(EvmeError::InvalidInput(retired_array_format_message(path)));
    }
    let doc: EnvelopeDoc = serde_json::from_value(value).map_err(|e| {
        EvmeError::InvalidInput(format!("Failed to decode envelope {}: {e}", path.display()))
    })?;
    if doc.version != ENVELOPE_VERSION {
        return Err(EvmeError::InvalidInput(format!(
            "Unsupported cache file version {} in '{}'; expected {ENVELOPE_VERSION}",
            doc.version,
            path.display(),
        )));
    }
    Ok(doc)
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

    /// The retired array stays recognizable, which is what lets a diagnostic
    /// name it instead of calling it corrupt. An array of anything else is not
    /// that format and does not get the retired-format story told about it.
    #[test]
    fn test_detect_shape_recognizes_the_retired_array_and_the_envelope() {
        let arr = serde_json::json!([{"key": B256::ZERO, "value": "x"}]);
        assert_eq!(detect_shape(&arr, Path::new("p.json")).unwrap(), CacheShape::Provider);
        let env = serde_json::json!({
            "version": 1,
            "chain_id": 1,
            "cache": []
        });
        assert_eq!(detect_shape(&env, Path::new("e.json")).unwrap(), CacheShape::Envelope);

        let other = serde_json::json!([{"not": "a cache entry"}]);
        assert!(detect_shape(&other, Path::new("o.json")).is_err());
    }

    /// `cache merge` reads envelopes and nothing else, and says so in a way the
    /// operator can act on.
    #[test]
    fn test_load_cache_file_rejects_the_retired_array_with_guidance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rpc-cache-4326.json");
        fs::write(&path, serde_json::to_string(&vec![kv(1, "legacy")]).unwrap()).expect("write");

        let err = load_cache_file(&path).expect_err("the retired array is not an input");
        let msg = err.to_string();
        assert!(msg.contains("not a cache envelope"), "msg={msg}");
        assert!(msg.contains("bare JSON array"), "the likely cause must be named: msg={msg}");
        assert!(msg.contains("Delete it"), "the way forward must be named: msg={msg}");
        assert!(msg.contains("--rpc.capture-file"), "msg={msg}");
    }

    /// An envelope input still loads, so the rejection above is about the shape
    /// and not about `load_cache_file` refusing everything.
    #[test]
    fn test_load_cache_file_accepts_an_envelope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fixture.json");
        let doc =
            EnvelopeDoc { version: 1, chain_id: 4326, cache: vec![kv(1, "a")], external_env: None };
        fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).expect("write");

        assert_eq!(load_cache_file(&path).expect("envelope loads"), doc);
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
        let merged =
            merge_envelope_for_persist(&on_disk, &ours, None, Path::new("x.json")).unwrap();
        assert_eq!(merged.cache, vec![kv(1, "ours"), kv(2, "new")]);
        assert_eq!(merged.external_env, Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10)] }));
    }

    #[test]
    fn test_merge_envelope_for_persist_chain_id_mismatch() {
        let on_disk = EnvelopeDoc { version: 1, chain_id: 1, cache: vec![], external_env: None };
        let ours = EnvelopeDoc { version: 1, chain_id: 2, cache: vec![], external_env: None };
        let err =
            merge_envelope_for_persist(&on_disk, &ours, None, Path::new("x.json")).unwrap_err();
        assert!(err.to_string().contains("chain_id"));
    }

    /// Intentional refresh: loaded A, ours B, disk still A → B wins.
    #[test]
    fn test_merge_envelope_for_persist_intentional_refresh_wins() {
        let loaded = ExternalEnvDoc { bucket_capacities: vec![(1, 10)] };
        let ours_ext = ExternalEnvDoc { bucket_capacities: vec![(1, 99)] };
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "disk")],
            external_env: Some(loaded.clone()),
        };
        let ours = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(2, "ours")],
            external_env: Some(ours_ext.clone()),
        };
        let merged =
            merge_envelope_for_persist(&on_disk, &ours, Some(&loaded), Path::new("capture.json"))
                .expect("intentional A→B refresh must succeed");
        assert_eq!(merged.external_env, Some(ours_ext.canonicalized()));
        assert_eq!(merged.cache, vec![kv(1, "disk"), kv(2, "ours")]);
    }

    /// The merged cache file never exceeds the configured cap, and this
    /// process's entries survive the truncation.
    ///
    /// Runs sharing a cache directory touch disjoint RPC keys, so an uncapped
    /// union grows the file without limit no matter how small each run's LRU is.
    #[test]
    fn test_merge_cache_entries_capped_bounds_the_union() {
        let on_disk: Vec<CacheKv> = (0..10).map(|i| kv(i, "disk")).collect();
        let ours: Vec<CacheKv> = (100..104).map(|i| kv(i, "ours")).collect();

        let merged = merge_cache_entries_capped(on_disk, ours.clone(), 6);
        assert_eq!(merged.len(), 6, "the union is capped, not the sum of both sides");
        for entry in &ours {
            assert!(
                merged.iter().any(|m| m.key == entry.key && m.value == entry.value),
                "this run's entries survive truncation: {:?}",
                entry.key
            );
        }
    }

    /// A cap larger than the union keeps everything, and ours win on collision.
    #[test]
    fn test_merge_cache_entries_capped_keeps_all_below_the_cap() {
        let on_disk = vec![kv(1, "disk"), kv(2, "disk")];
        let ours = vec![kv(2, "ours"), kv(3, "ours")];

        let merged = merge_cache_entries_capped(on_disk, ours, 16);
        assert_eq!(merged, vec![kv(1, "disk"), kv(2, "ours"), kv(3, "ours")]);
    }

    /// No opinion: loaded A, ours A (carried forward), disk now B → B wins and
    /// our cache entries still merge. A run given no `--bucket-capacity` reaches
    /// persist with `ours == loaded`; treating that as a conflict would fail the
    /// persist and throw away everything the run captured.
    #[test]
    fn test_merge_envelope_for_persist_carried_forward_snapshot_yields_to_sibling_refresh() {
        let loaded = ExternalEnvDoc { bucket_capacities: vec![(1, 10)] };
        let disk_ext = ExternalEnvDoc { bucket_capacities: vec![(1, 20)] };
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "disk")],
            external_env: Some(disk_ext.clone()),
        };
        // No `--bucket-capacity` on this run: the loaded snapshot is carried
        // forward verbatim, so `ours` is byte-identical to `loaded`.
        let ours = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(2, "ours")],
            external_env: Some(loaded.clone()),
        };
        let merged =
            merge_envelope_for_persist(&on_disk, &ours, Some(&loaded), Path::new("capture.json"))
                .expect("a run that expressed no opinion must not conflict");
        assert_eq!(merged.external_env, Some(disk_ext.canonicalized()));
        assert_eq!(merged.cache, vec![kv(1, "disk"), kv(2, "ours")]);
    }

    /// Table row one (`ours = None`): a run with no snapshot of its own keeps
    /// the on-disk one and still merges its cache entries.
    ///
    /// This row was always correct; it is pinned so the table has a test per
    /// row rather than only where a bug was found.
    #[test]
    fn test_merge_envelope_for_persist_no_snapshot_yields_to_sibling_refresh() {
        let disk_ext = ExternalEnvDoc { bucket_capacities: vec![(1, 20)] };
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "disk")],
            external_env: Some(disk_ext.clone()),
        };
        let ours =
            EnvelopeDoc { version: 1, chain_id: 7, cache: vec![kv(2, "ours")], external_env: None };
        let merged = merge_envelope_for_persist(&on_disk, &ours, None, Path::new("capture.json"))
            .expect("a run with no snapshot must not conflict");
        assert_eq!(merged.external_env, Some(disk_ext));
        assert_eq!(merged.cache, vec![kv(1, "disk"), kv(2, "ours")]);
    }

    /// True concurrent conflict: loaded A, ours B, disk now C≠B → hard error naming A/B/C.
    #[test]
    fn test_merge_envelope_for_persist_rejects_true_concurrent_conflict() {
        let loaded = ExternalEnvDoc { bucket_capacities: vec![(1, 10)] };
        let ours_ext = ExternalEnvDoc { bucket_capacities: vec![(1, 99)] };
        let disk_ext = ExternalEnvDoc { bucket_capacities: vec![(1, 42)] };
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "disk")],
            external_env: Some(disk_ext),
        };
        let ours = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(2, "ours")],
            external_env: Some(ours_ext),
        };
        let err =
            merge_envelope_for_persist(&on_disk, &ours, Some(&loaded), Path::new("capture.json"))
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("external_env"), "msg={msg}");
        assert!(msg.contains("loaded"), "msg={msg}");
        assert!(msg.contains("ours"), "msg={msg}");
        assert!(msg.contains("on-disk"), "msg={msg}");
        // All three snapshots named (Debug form of bucket capacities).
        assert!(msg.contains("10") && msg.contains("99") && msg.contains("42"), "msg={msg}");
    }

    /// Loaded none, ours B, disk now C≠B → hard error (file gained a foreign snapshot).
    #[test]
    fn test_merge_envelope_for_persist_rejects_conflict_when_loaded_none() {
        let ours_ext = ExternalEnvDoc { bucket_capacities: vec![(1, 99)] };
        let disk_ext = ExternalEnvDoc { bucket_capacities: vec![(1, 42)] };
        let on_disk =
            EnvelopeDoc { version: 1, chain_id: 7, cache: vec![], external_env: Some(disk_ext) };
        let ours =
            EnvelopeDoc { version: 1, chain_id: 7, cache: vec![], external_env: Some(ours_ext) };
        let err = merge_envelope_for_persist(&on_disk, &ours, None, Path::new("capture.json"))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("external_env"), "msg={msg}");
        assert!(msg.contains("loaded"), "msg={msg}");
        assert!(msg.contains("99") && msg.contains("42"), "msg={msg}");
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
        let merged =
            merge_envelope_for_persist(&on_disk, &ours, Some(&ext), Path::new("x.json")).unwrap();
        assert_eq!(merged.cache, vec![kv(1, "disk"), kv(2, "ours")]);
        assert_eq!(merged.external_env, Some(ext.canonicalized()));
    }

    /// Same capacities in different order are not a conflict (canonical equality).
    #[test]
    fn test_merge_envelope_for_persist_order_insensitive_external_env() {
        let a = ExternalEnvDoc { bucket_capacities: vec![(1, 10), (2, 20)] };
        let b = ExternalEnvDoc { bucket_capacities: vec![(2, 20), (1, 10)] };
        let on_disk = EnvelopeDoc { version: 1, chain_id: 1, cache: vec![], external_env: Some(a) };
        let ours = EnvelopeDoc { version: 1, chain_id: 1, cache: vec![], external_env: Some(b) };
        // Concurrent writer used the same effective capacities in different CLI order.
        let merged = merge_envelope_for_persist(
            &on_disk,
            &ours,
            Some(&ExternalEnvDoc { bucket_capacities: vec![(9, 9)] }),
            Path::new("x.json"),
        )
        .expect("order-only difference must not conflict");
        assert_eq!(
            merged.external_env,
            Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10), (2, 20)] })
        );
    }

    /// Duplicate bucket ids collapse with last-wins before sort.
    #[test]
    fn test_canonicalize_bucket_capacities_last_wins_and_sorts() {
        let caps = canonicalize_bucket_capacities(&[(2, 20), (1, 10), (2, 99), (1, 11)]);
        assert_eq!(caps, vec![(1, 11), (2, 99)]);
        let doc = ExternalEnvDoc { bucket_capacities: vec![(3, 1), (1, 2), (3, 9)] };
        assert_eq!(doc.canonicalized().bucket_capacities, vec![(1, 2), (3, 9)]);
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
        let merged =
            merge_envelope_for_persist(&on_disk, &ours, None, Path::new("x.json")).unwrap();
        assert_eq!(merged.external_env, Some(ext.canonicalized()));

        // Ours Some, disk None → ours kept.
        let on_disk = EnvelopeDoc { version: 1, chain_id: 1, cache: vec![], external_env: None };
        let ours = EnvelopeDoc {
            version: 1,
            chain_id: 1,
            cache: vec![kv(1, "a")],
            external_env: Some(ext.clone()),
        };
        let merged =
            merge_envelope_for_persist(&on_disk, &ours, None, Path::new("x.json")).unwrap();
        assert_eq!(merged.external_env, Some(ext.canonicalized()));
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

    /// CLI merge treats equal capacities in different order as identical.
    #[test]
    fn test_merge_envelopes_cli_order_insensitive_external_env() {
        let a = EnvelopeDoc {
            version: 1,
            chain_id: 1,
            cache: vec![kv(1, "a")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(2, 20), (1, 10)] }),
        };
        let b = EnvelopeDoc {
            version: 1,
            chain_id: 1,
            cache: vec![kv(2, "b")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10), (2, 20)] }),
        };
        let docs = vec![(PathBuf::from("a.json"), a), (PathBuf::from("b.json"), b)];
        let merged = merge_envelopes_cli(&docs).expect("order-only difference must merge");
        assert_eq!(
            merged.external_env,
            Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10), (2, 20)] })
        );
    }

    /// Folding the existing output unions its entries in, with the named inputs
    /// winning on key collision.
    #[test]
    fn test_fold_output_envelope_unions_with_inputs_winning() {
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "output"), kv(9, "concurrent")],
            external_env: None,
        };
        let inputs = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![kv(1, "inputs"), kv(2, "inputs")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(2, 20), (1, 10)] }),
        };
        let folded =
            fold_output_envelope(Path::new("out.json"), on_disk, inputs).expect("fold output");
        assert_eq!(folded.cache, vec![kv(1, "inputs"), kv(2, "inputs"), kv(9, "concurrent")]);
        // The written snapshot is canonical.
        assert_eq!(
            folded.external_env,
            Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10), (2, 20)] })
        );
    }

    /// A snapshot on the existing output that disagrees with the inputs is a
    /// conflict, exactly as it is between two inputs.
    #[test]
    fn test_fold_output_envelope_rejects_conflicting_external_env() {
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 42)] }),
        };
        let inputs = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 99)] }),
        };
        let err =
            fold_output_envelope(Path::new("out.json"), on_disk, inputs).expect_err("conflict");
        let msg = err.to_string();
        assert!(msg.contains("external_env"), "msg={msg}");
        assert!(msg.contains("out.json"), "msg={msg}");
        assert!(msg.contains("42") && msg.contains("99"), "msg={msg}");
    }

    /// Same effective capacities in different order are not a conflict.
    #[test]
    fn test_fold_output_envelope_order_insensitive_external_env() {
        let on_disk = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(2, 20), (1, 10)] }),
        };
        let inputs = EnvelopeDoc {
            version: 1,
            chain_id: 7,
            cache: vec![],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10), (2, 20)] }),
        };
        let folded = fold_output_envelope(Path::new("out.json"), on_disk, inputs)
            .expect("order-only difference must not conflict");
        assert_eq!(
            folded.external_env,
            Some(ExternalEnvDoc { bucket_capacities: vec![(1, 10), (2, 20)] })
        );
    }

    /// Identity failures against the existing output name that file.
    #[test]
    fn test_fold_output_envelope_rejects_identity_mismatch() {
        let inputs = EnvelopeDoc { version: 1, chain_id: 7, cache: vec![], external_env: None };

        let other_chain =
            EnvelopeDoc { version: 1, chain_id: 8, cache: vec![], external_env: None };
        let err = fold_output_envelope(Path::new("out.json"), other_chain, inputs.clone())
            .expect_err("chain_id mismatch");
        let msg = err.to_string();
        assert!(msg.contains("chain_id") && msg.contains("out.json"), "msg={msg}");

        let other_version =
            EnvelopeDoc { version: 2, chain_id: 7, cache: vec![], external_env: None };
        let err = fold_output_envelope(Path::new("out.json"), other_version, inputs)
            .expect_err("version mismatch");
        let msg = err.to_string();
        assert!(msg.contains("version") && msg.contains("out.json"), "msg={msg}");
    }
}
