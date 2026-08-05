//! Top-level `cache` subcommand group (`mega-evme cache …`).
//!
//! Currently ships `cache merge` for consolidating per-worker provider-cache
//! files or capture envelopes after historical sharded campaigns.

mod merge;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::common::{EvmeError, Result};

pub(crate) use merge::{
    lock_sidecar_path, merge_envelope_for_persist, merge_provider_entries_capped,
    merge_provider_lists, parse_rpc_cache_filename_chain_id, read_provider_cache,
    reread_envelope_for_merge, write_bytes_atomic, write_envelope_atomic,
    write_provider_cache_atomic, CacheKv, EnvelopeDoc, EnvelopeReread, ExternalEnvDoc,
    ENVELOPE_VERSION,
};

use merge::{load_cache_file, merge_envelopes_cli, CacheShape, LoadedCache};
use tracing::warn;

/// `mega-evme cache` — offline cache-file utilities.
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Cache utility subcommand (`merge`, …).
    #[command(subcommand)]
    pub command: CacheCommands,
}

/// Cache utility subcommands.
#[derive(Subcommand, Debug)]
pub enum CacheCommands {
    /// Merge provider-cache files or capture envelopes into one output file.
    Merge(MergeArgs),
}

/// Arguments for `mega-evme cache merge`.
#[derive(Parser, Debug)]
pub struct MergeArgs {
    /// Input cache files (provider-cache arrays or capture envelopes; not mixed).
    #[arg(required = true, num_args = 1.., value_name = "INPUT")]
    pub inputs: Vec<PathBuf>,

    /// Destination path for the merged file (written atomically via temp + rename).
    #[arg(long, short = 'o', value_name = "FILE")]
    pub output: PathBuf,
}

impl Cmd {
    /// Dispatch the cache subcommand.
    pub fn run(self) -> Result<()> {
        match self.command {
            CacheCommands::Merge(args) => args.run(),
        }
    }
}

/// Validate that provider-cache paths agreeing with `rpc-cache-{id}.json` all
/// name the same chain id.
///
/// Paths that do not match the pattern emit a `warn!` (chain identity cannot be
/// validated for them) and are otherwise ignored. Two or more matching paths
/// with different ids are a hard error naming the conflicting files.
pub(crate) fn check_provider_cache_chain_identity<'a>(
    paths: impl IntoIterator<Item = &'a std::path::Path>,
) -> Result<()> {
    let mut seen: Option<(u64, PathBuf)> = None;
    for path in paths {
        match parse_rpc_cache_filename_chain_id(path) {
            None => {
                warn!(
                    path = %path.display(),
                    "Provider-cache path does not match rpc-cache-{{id}}.json; \
                     chain identity cannot be validated for this file",
                );
            }
            Some(id) => match &seen {
                None => seen = Some((id, path.to_path_buf())),
                Some((prev_id, prev_path)) if *prev_id != id => {
                    return Err(EvmeError::InvalidInput(format!(
                        "Provider-cache chain identity mismatch: '{}' is chain {prev_id}, \
                         but '{}' is chain {id}. Merge only caches from the same chain.",
                        prev_path.display(),
                        path.display(),
                    )));
                }
                Some(_) => {}
            },
        }
    }
    Ok(())
}

impl MergeArgs {
    /// Merge inputs into `--output` and print a one-line summary.
    pub fn run(self) -> Result<()> {
        if self.inputs.is_empty() {
            return Err(EvmeError::InvalidInput(
                "cache merge requires at least one input file".into(),
            ));
        }

        let mut loaded: Vec<(PathBuf, CacheShape, LoadedCache)> =
            Vec::with_capacity(self.inputs.len());
        for path in &self.inputs {
            let (shape, data) = load_cache_file(path)?;
            loaded.push((path.clone(), shape, data));
        }

        let first_shape = loaded[0].1;
        for (path, shape, _) in &loaded {
            if *shape != first_shape {
                return Err(EvmeError::InvalidInput(format!(
                    "Mixed cache file shapes: '{}' is {:?}, but the first input is {:?}. \
                     Merge provider-cache files and capture envelopes in separate invocations.",
                    path.display(),
                    shape,
                    first_shape,
                )));
            }
        }

        let total_in: usize = loaded.iter().map(|(_, _, d)| d.entry_count()).sum();
        let input_count = loaded.len();

        let unique_out = match first_shape {
            CacheShape::Provider => {
                // Provider-cache files carry chain identity only in the
                // `rpc-cache-{id}.json` filename. Reject merges that would
                // union different chains; warn when a path cannot be checked.
                check_provider_cache_chain_identity(
                    loaded
                        .iter()
                        .map(|(p, _, _)| p.as_path())
                        .chain(std::iter::once(self.output.as_path())),
                )?;

                let mut acc = Vec::new();
                for (_, _, data) in loaded {
                    let LoadedCache::Provider(entries) = data else { unreachable!() };
                    // Later inputs win on collision.
                    acc = merge_provider_lists(acc, entries);
                }
                let unique = acc.len();
                write_provider_cache_atomic(&self.output, &acc)?;
                unique
            }
            CacheShape::Envelope => {
                let docs: Vec<(PathBuf, EnvelopeDoc)> = loaded
                    .into_iter()
                    .map(|(path, _, data)| {
                        let LoadedCache::Envelope(doc) = data else { unreachable!() };
                        (path, doc)
                    })
                    .collect();
                let merged = merge_envelopes_cli(&docs)?;
                let unique = merged.cache.len();
                write_envelope_atomic(&self.output, &merged)?;
                unique
            }
        };

        println!(
            "Merged {input_count} inputs ({total_in} entries in) → {unique_out} unique entries out"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use alloy_primitives::B256;
    use tempfile::tempdir;

    use super::*;
    use crate::cache::merge::{merge_envelopes_cli, CacheKv, EnvelopeDoc, ExternalEnvDoc};

    fn write(path: &std::path::Path, content: &str) {
        fs::write(path, content).expect("write");
    }

    fn kv(byte: u8, val: &str) -> CacheKv {
        CacheKv { key: B256::repeat_byte(byte), value: val.to_string() }
    }

    #[test]
    fn test_cache_merge_provider_union_later_wins() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        let out = dir.path().join("out.json");

        let entries_a = vec![kv(1, "from-a"), kv(2, "a")];
        let entries_b = vec![kv(2, "from-b"), kv(3, "b")];
        write(&a, &serde_json::to_string(&entries_a).unwrap());
        write(&b, &serde_json::to_string(&entries_b).unwrap());

        MergeArgs { inputs: vec![a, b], output: out.clone() }.run().expect("merge");

        let merged: Vec<CacheKv> =
            serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(merged, vec![kv(1, "from-a"), kv(2, "from-b"), kv(3, "b")]);
    }

    #[test]
    fn test_cache_merge_envelope_union() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        let out = dir.path().join("out.json");

        let env_a = EnvelopeDoc {
            version: 1,
            chain_id: 4326,
            cache: vec![kv(1, "a")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 100)] }),
        };
        let env_b = EnvelopeDoc {
            version: 1,
            chain_id: 4326,
            cache: vec![kv(1, "b"), kv(2, "b")],
            external_env: None,
        };
        write(&a, &serde_json::to_string_pretty(&env_a).unwrap());
        write(&b, &serde_json::to_string_pretty(&env_b).unwrap());

        MergeArgs { inputs: vec![a, b], output: out.clone() }.run().expect("merge");

        let merged: EnvelopeDoc = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(merged.chain_id, 4326);
        assert_eq!(merged.cache, vec![kv(1, "b"), kv(2, "b")]);
        assert_eq!(merged.external_env, Some(ExternalEnvDoc { bucket_capacities: vec![(1, 100)] }));
    }

    #[test]
    fn test_cache_merge_rejects_mixed_shapes() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        let out = dir.path().join("out.json");

        write(&a, &serde_json::to_string(&vec![kv(1, "a")]).unwrap());
        write(
            &b,
            &serde_json::to_string_pretty(&EnvelopeDoc {
                version: 1,
                chain_id: 1,
                cache: vec![],
                external_env: None,
            })
            .unwrap(),
        );

        let err = MergeArgs { inputs: vec![a, b], output: out }.run().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Mixed") || msg.contains("shape"), "msg={msg}");
    }

    #[test]
    fn test_cache_merge_rejects_chain_id_mismatch() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        let out = dir.path().join("out.json");

        write(
            &a,
            &serde_json::to_string_pretty(&EnvelopeDoc {
                version: 1,
                chain_id: 1,
                cache: vec![],
                external_env: None,
            })
            .unwrap(),
        );
        write(
            &b,
            &serde_json::to_string_pretty(&EnvelopeDoc {
                version: 1,
                chain_id: 2,
                cache: vec![],
                external_env: None,
            })
            .unwrap(),
        );

        let err = MergeArgs { inputs: vec![a, b], output: out }.run().unwrap_err();
        assert!(err.to_string().contains("chain_id"));
    }

    #[test]
    fn test_cache_merge_rejects_version_mismatch() {
        let docs = vec![
            (
                PathBuf::from("a.json"),
                EnvelopeDoc { version: 1, chain_id: 1, cache: vec![], external_env: None },
            ),
            (
                PathBuf::from("b.json"),
                EnvelopeDoc { version: 2, chain_id: 1, cache: vec![], external_env: None },
            ),
        ];
        let err = merge_envelopes_cli(&docs).unwrap_err();
        assert!(err.to_string().contains("version"));
    }

    /// Mismatched `rpc-cache-{id}.json` filenames hard-error naming both files.
    #[test]
    fn test_cache_merge_rejects_provider_chain_id_filename_mismatch() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("rpc-cache-1.json");
        let b = dir.path().join("rpc-cache-4326.json");
        let out = dir.path().join("rpc-cache-4326-out.json");

        write(&a, &serde_json::to_string(&vec![kv(1, "a")]).unwrap());
        write(&b, &serde_json::to_string(&vec![kv(2, "b")]).unwrap());

        let err = MergeArgs { inputs: vec![a.clone(), b.clone()], output: out }.run().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("chain identity") || msg.contains("chain"), "msg={msg}");
        assert!(msg.contains("chain 1") && msg.contains("chain 4326"), "msg={msg}");
        assert!(
            msg.contains(a.file_name().unwrap().to_str().unwrap()) ||
                msg.contains("rpc-cache-1.json"),
            "msg={msg}"
        );
        assert!(
            msg.contains(b.file_name().unwrap().to_str().unwrap()) ||
                msg.contains("rpc-cache-4326.json"),
            "msg={msg}"
        );
    }

    /// Output path is included in the chain-identity check.
    #[test]
    fn test_cache_merge_rejects_provider_output_chain_id_mismatch() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("rpc-cache-1.json");
        let out = dir.path().join("rpc-cache-4326.json");
        write(&a, &serde_json::to_string(&vec![kv(1, "a")]).unwrap());

        let err = MergeArgs { inputs: vec![a], output: out }.run().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("chain 1") && msg.contains("chain 4326"), "msg={msg}");
    }

    /// Unit-testable predicate: non-matching filename cannot supply chain identity.
    #[test]
    fn test_provider_cache_chain_identity_non_matching_filename_is_none() {
        assert_eq!(parse_rpc_cache_filename_chain_id(std::path::Path::new("merged.json")), None);
        assert_eq!(
            parse_rpc_cache_filename_chain_id(std::path::Path::new("rpc-cache-1.json")),
            Some(1)
        );
    }

    /// Same chain id across matching names is accepted (including output).
    #[test]
    fn test_check_provider_cache_chain_identity_same_id_ok() {
        let paths = [
            std::path::Path::new("worker0/rpc-cache-4326.json"),
            std::path::Path::new("worker1/rpc-cache-4326.json"),
            std::path::Path::new("rpc-cache-4326.json"),
        ];
        check_provider_cache_chain_identity(paths).expect("same chain ok");
    }

    /// Different ids hard-error; non-matching names alone do not.
    #[test]
    fn test_check_provider_cache_chain_identity_mismatch_and_non_matching() {
        let paths = [
            std::path::Path::new("rpc-cache-1.json"),
            std::path::Path::new("out.json"), // non-matching → warn only
            std::path::Path::new("rpc-cache-4326.json"),
        ];
        let err = check_provider_cache_chain_identity(paths).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rpc-cache-1.json") && msg.contains("rpc-cache-4326.json"), "{msg}");

        // Only non-matching names: no chain id to disagree on → ok (with warns).
        let only_free = [
            std::path::Path::new("a.json"),
            std::path::Path::new("b.json"),
            std::path::Path::new("merged.json"),
        ];
        check_provider_cache_chain_identity(only_free).expect("no ids to conflict");
    }
}
