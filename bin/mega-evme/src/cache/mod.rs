//! Top-level `cache` subcommand group (`mega-evme cache …`).
//!
//! Currently ships `cache merge` for consolidating per-worker provider-cache
//! files or capture envelopes after historical sharded campaigns.

mod lock;
mod merge;

use std::{fmt, path::PathBuf};

use clap::{Parser, Subcommand};

use crate::common::{EvmeError, Result};

pub(crate) use lock::{acquire_exclusive_lock, lock_sidecar_path};
pub(crate) use merge::{
    detect_shape, merge_envelope_for_persist, merge_provider_entries_capped, merge_provider_lists,
    parse_rpc_cache_filename_chain_id, reread_envelope_for_merge, reread_provider_cache_for_merge,
    write_bytes_atomic, write_envelope_atomic, write_provider_cache_atomic, CacheKv, CacheShape,
    EnvelopeDoc, EnvelopeReread, ExternalEnvDoc, ProviderReread, ENVELOPE_VERSION,
};

// Used by unit tests that assert the provider-array on-disk shape after a merge.
#[cfg(test)]
pub(crate) use merge::read_provider_cache;

use merge::{fold_output_envelope, load_cache_file, merge_envelopes_cli, LoadedCache};
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

/// Emit a diagnostic that protects the user from a silently wrong merge or
/// persist decision, on stderr unconditionally and through the structured log
/// sinks.
///
/// The CLI leaves the tracing filter at `off` unless `-v` flags or `RUST_LOG`
/// raise it, so a `warn!`-only diagnostic reaches nobody on a default command
/// line: a safeguard reporting that it could not run, or a write about to
/// discard data already on disk, would be announced into a disabled subscriber.
/// stderr therefore carries the human line regardless of verbosity, and the
/// tracing event still carries it to a `--log.file` sink. At raised verbosity
/// without `--log.file` both channels land on stderr and the line appears
/// twice, which is preferable to dropping either one.
///
/// Reserved for warnings a user must act on; ordinary progress reporting stays
/// on `tracing` alone.
pub(crate) fn warn_user(message: fmt::Arguments<'_>) {
    eprintln!("warning: {message}");
    warn!("{message}");
}

/// Validate that provider-cache paths agreeing with `rpc-cache-{id}.json` all
/// name the same chain id.
///
/// Paths that do not match the pattern warn the user (chain identity cannot be
/// validated for them) and are otherwise ignored. Two or more matching paths
/// with different ids are a hard error naming the conflicting files.
pub(crate) fn check_provider_cache_chain_identity<'a>(
    paths: impl IntoIterator<Item = &'a std::path::Path>,
) -> Result<()> {
    let mut seen: Option<(u64, PathBuf)> = None;
    for path in paths {
        match parse_rpc_cache_filename_chain_id(path) {
            None => {
                warn_user(format_args!(
                    "Provider-cache path '{}' does not match rpc-cache-{{id}}.json; \
                     chain identity cannot be validated for this file",
                    path.display(),
                ));
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

        if first_shape == CacheShape::Provider {
            // Provider-cache files carry chain identity only in the
            // `rpc-cache-{id}.json` filename. Reject merges that would
            // union different chains; warn when a path cannot be checked.
            // Checked before the output is locked so a doomed merge leaves no
            // sidecar behind.
            check_provider_cache_chain_identity(
                loaded
                    .iter()
                    .map(|(p, _, _)| p.as_path())
                    .chain(std::iter::once(self.output.as_path())),
            )?;
        }

        // The output is a shared file: a live run may be persisting to the same
        // path under the same sidecar lock. Take that lock and hold it across
        // read-merge-rename, so neither side's entries are lost to whichever
        // rename lands last.
        let _output_lock = acquire_exclusive_lock(&self.output).map_err(|e| {
            EvmeError::InvalidInput(format!(
                "Failed to acquire the cache lock {} for output '{}': {e}. \
                 Refusing to merge without it: an unlocked write would silently drop \
                 entries written by a concurrent process.",
                lock_sidecar_path(&self.output).display(),
                self.output.display(),
            ))
        })?;

        // Entries the output file already held when the lock was granted.
        let mut folded_in = 0usize;

        let unique_out = match first_shape {
            CacheShape::Provider => {
                let mut acc = Vec::new();
                for (_, _, data) in loaded {
                    let LoadedCache::Provider(entries) = data else { unreachable!() };
                    // Later inputs win on collision.
                    acc = merge_provider_lists(acc, entries);
                }

                // Whatever is at the output now joins the union as one more
                // input: a concurrent writer may have landed entries there
                // while this merge waited for the lock.
                let on_disk = match reread_provider_cache_for_merge(&self.output) {
                    ProviderReread::Ok(entries) => entries,
                    ProviderReread::Hard(err) => return Err(err),
                    ProviderReread::Degradable(msg) => {
                        // Replacing the output drops whatever it held: the user
                        // must hear about it whatever the verbosity is.
                        warn_user(format_args!(
                            "{msg}. Replacing the existing merge output '{}' with the \
                             merged inputs; any entries it held are discarded",
                            self.output.display(),
                        ));
                        Vec::new()
                    }
                };
                folded_in = on_disk.len();
                // The named inputs win over the output's prior entries.
                let acc = merge_provider_lists(on_disk, acc);

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

                let merged = if self.output.exists() {
                    // Typed classification: identity/schema failures must not be
                    // papered over by overwriting the file we cannot read.
                    match reread_envelope_for_merge(&self.output) {
                        EnvelopeReread::Ok(on_disk) => {
                            folded_in = on_disk.cache.len();
                            fold_output_envelope(&self.output, on_disk, merged)?
                        }
                        EnvelopeReread::Hard(err) => return Err(err),
                        EnvelopeReread::Degradable(msg) => {
                            // Same data loss as the provider shape above.
                            warn_user(format_args!(
                                "{msg}. Replacing the existing merge output '{}' with the \
                                 merged inputs; any entries it held are discarded",
                                self.output.display(),
                            ));
                            merged
                        }
                    }
                } else {
                    merged
                };

                let unique = merged.cache.len();
                write_envelope_atomic(&self.output, &merged)?;
                unique
            }
        };

        // Name the folded-in entries so the arithmetic still adds up when the
        // output already held some.
        let folded = if folded_in > 0 {
            format!(" + {folded_in} already in the output")
        } else {
            String::new()
        };
        println!(
            "Merged {input_count} inputs ({total_in} entries in{folded}) \
             → {unique_out} unique entries out"
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

    /// A provider-shaped output already on disk joins the union as one more
    /// input: entries a concurrent writer left there survive the merge, and the
    /// named inputs win where the keys collide.
    #[test]
    fn test_cache_merge_folds_the_existing_provider_output() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        let out = dir.path().join("out.json");

        write(&a, &serde_json::to_string(&vec![kv(1, "from-a")]).unwrap());
        write(&b, &serde_json::to_string(&vec![kv(2, "from-b")]).unwrap());
        // The output already holds a sibling's entry plus a stale copy of key 2.
        write(
            &out,
            &serde_json::to_string(&vec![kv(2, "from-output"), kv(9, "concurrent")]).unwrap(),
        );

        MergeArgs { inputs: vec![a, b], output: out.clone() }.run().expect("merge");

        let merged: Vec<CacheKv> =
            serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(merged, vec![kv(1, "from-a"), kv(2, "from-b"), kv(9, "concurrent")]);
    }

    /// The envelope shape folds its existing output the same way.
    #[test]
    fn test_cache_merge_folds_the_existing_envelope_output() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let out = dir.path().join("out.json");

        let env_a = EnvelopeDoc {
            version: 1,
            chain_id: 4326,
            cache: vec![kv(1, "from-a"), kv(2, "from-a")],
            external_env: None,
        };
        let existing = EnvelopeDoc {
            version: 1,
            chain_id: 4326,
            cache: vec![kv(2, "from-output"), kv(9, "concurrent")],
            external_env: Some(ExternalEnvDoc { bucket_capacities: vec![(1, 100)] }),
        };
        write(&a, &serde_json::to_string_pretty(&env_a).unwrap());
        write(&out, &serde_json::to_string_pretty(&existing).unwrap());

        MergeArgs { inputs: vec![a], output: out.clone() }.run().expect("merge");

        let merged: EnvelopeDoc = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(merged.cache, vec![kv(1, "from-a"), kv(2, "from-a"), kv(9, "concurrent")]);
        // The output's snapshot is preserved when the inputs carry none.
        assert_eq!(merged.external_env, Some(ExternalEnvDoc { bucket_capacities: vec![(1, 100)] }));
    }

    /// An unreadable provider output degrades to the merged inputs (warned),
    /// matching the persist path's handling of a corrupt on-disk file.
    #[test]
    fn test_cache_merge_replaces_corrupt_provider_output() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let out = dir.path().join("out.json");

        write(&a, &serde_json::to_string(&vec![kv(1, "from-a")]).unwrap());
        write(&out, "not-json{{{");

        MergeArgs { inputs: vec![a], output: out.clone() }.run().expect("merge");

        let merged: Vec<CacheKv> =
            serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(merged, vec![kv(1, "from-a")]);
    }

    /// An existing envelope output on another chain is an identity failure, not
    /// something to overwrite.
    #[test]
    fn test_cache_merge_rejects_existing_envelope_output_on_another_chain() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let out = dir.path().join("out.json");

        write(
            &a,
            &serde_json::to_string_pretty(&EnvelopeDoc {
                version: 1,
                chain_id: 1,
                cache: vec![kv(1, "a")],
                external_env: None,
            })
            .unwrap(),
        );
        let existing = serde_json::to_string_pretty(&EnvelopeDoc {
            version: 1,
            chain_id: 2,
            cache: vec![kv(9, "concurrent")],
            external_env: None,
        })
        .unwrap();
        write(&out, &existing);

        let err = MergeArgs { inputs: vec![a], output: out.clone() }.run().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("chain_id"), "msg={msg}");
        assert!(msg.contains("out.json"), "the output must be named: msg={msg}");
        assert_eq!(fs::read_to_string(&out).unwrap(), existing, "output left untouched");
    }

    /// A capture envelope sitting at a provider merge's output is a hard error
    /// for the same reason as the mirrored case below: a mistyped `--output`
    /// must not destroy a file the merge cannot fold.
    #[test]
    fn test_cache_merge_rejects_wrong_shaped_existing_provider_output() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let out = dir.path().join("out.json");

        write(&a, &serde_json::to_string(&vec![kv(1, "from-a")]).unwrap());
        let existing = serde_json::to_string_pretty(&EnvelopeDoc {
            version: 1,
            chain_id: 1,
            cache: vec![kv(9, "concurrent")],
            external_env: None,
        })
        .unwrap();
        write(&out, &existing);

        let err = MergeArgs { inputs: vec![a], output: out.clone() }.run().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("envelope"), "msg={msg}");
        assert_eq!(fs::read_to_string(&out).unwrap(), existing, "output left untouched");
    }

    /// A provider-shaped file sitting at an envelope merge's output is a hard
    /// error: it cannot be folded, and overwriting it would destroy it.
    #[test]
    fn test_cache_merge_rejects_wrong_shaped_existing_output() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let out = dir.path().join("out.json");

        write(
            &a,
            &serde_json::to_string_pretty(&EnvelopeDoc {
                version: 1,
                chain_id: 1,
                cache: vec![kv(1, "a")],
                external_env: None,
            })
            .unwrap(),
        );
        let existing = serde_json::to_string(&vec![kv(9, "concurrent")]).unwrap();
        write(&out, &existing);

        let err = MergeArgs { inputs: vec![a], output: out.clone() }.run().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("envelope"), "msg={msg}");
        assert_eq!(fs::read_to_string(&out).unwrap(), existing, "output left untouched");
    }

    /// Provider merge fails closed when the output lock cannot be acquired: the
    /// existing output is left exactly as it was.
    #[test]
    fn test_cache_merge_provider_fails_closed_when_the_output_lock_is_unavailable() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let out = dir.path().join("out.json");

        write(&a, &serde_json::to_string(&vec![kv(1, "from-a")]).unwrap());
        let existing = serde_json::to_string(&vec![kv(9, "concurrent")]).unwrap();
        write(&out, &existing);
        // A directory in the sidecar's place makes the lock un-acquirable.
        fs::create_dir(lock_sidecar_path(&out)).expect("occupy sidecar path");

        let err = MergeArgs { inputs: vec![a], output: out.clone() }.run().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("lock"), "msg={msg}");
        assert!(msg.contains("out.json.lock"), "the lock path must be named: msg={msg}");
        assert_eq!(fs::read_to_string(&out).unwrap(), existing, "no unlocked write happened");
    }

    /// Envelope merge fails closed on the same condition.
    #[test]
    fn test_cache_merge_envelope_fails_closed_when_the_output_lock_is_unavailable() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let out = dir.path().join("out.json");

        write(
            &a,
            &serde_json::to_string_pretty(&EnvelopeDoc {
                version: 1,
                chain_id: 1,
                cache: vec![kv(1, "a")],
                external_env: None,
            })
            .unwrap(),
        );
        let existing = serde_json::to_string_pretty(&EnvelopeDoc {
            version: 1,
            chain_id: 1,
            cache: vec![kv(9, "concurrent")],
            external_env: None,
        })
        .unwrap();
        write(&out, &existing);
        fs::create_dir(lock_sidecar_path(&out)).expect("occupy sidecar path");

        let err = MergeArgs { inputs: vec![a], output: out.clone() }.run().unwrap_err();
        assert!(err.to_string().contains("lock"), "msg={err}");
        assert_eq!(fs::read_to_string(&out).unwrap(), existing, "no unlocked write happened");
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
