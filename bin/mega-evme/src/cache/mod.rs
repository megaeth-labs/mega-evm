//! Top-level `cache` subcommand group (`mega-evme cache …`).
//!
//! Currently ships `cache merge` for consolidating per-worker provider-cache
//! files or capture envelopes after historical sharded campaigns.

mod merge;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::common::{EvmeError, Result};

pub(crate) use merge::{
    lock_sidecar_path, merge_envelope_for_persist, merge_kv_entries, merge_provider_lists,
    read_envelope, read_provider_cache, write_bytes_atomic, write_envelope_atomic,
    write_provider_cache_atomic, CacheKv, EnvelopeDoc, ExternalEnvDoc, ENVELOPE_VERSION,
};

use merge::{load_cache_file, merge_envelopes_cli, CacheShape, LoadedCache};

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
}
