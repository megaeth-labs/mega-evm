//! Top-level `cache` subcommand group (`mega-evme cache …`).
//!
//! Currently ships `cache merge` for consolidating per-worker cache envelopes
//! after historical sharded campaigns.

mod lock;
mod merge;

use std::{fmt, path::PathBuf};

use clap::{Parser, Subcommand};

use crate::common::{EvmeError, Result};

pub(crate) use lock::{acquire_exclusive_lock, lock_sidecar_path};
pub(crate) use merge::{
    detect_shape, merge_cache_entries_capped, merge_envelope_for_persist,
    reread_envelope_for_merge, write_bytes_atomic, write_envelope_atomic, CacheKv, CacheShape,
    EnvelopeDoc, EnvelopeReread, ExternalEnvDoc, ENVELOPE_VERSION,
};

use merge::{fold_output_envelope, load_cache_file, merge_envelopes_cli};
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
    /// Merge cache envelopes into one output file.
    Merge(MergeArgs),
}

/// Arguments for `mega-evme cache merge`.
#[derive(Parser, Debug)]
pub struct MergeArgs {
    /// Input cache envelopes (`{version, chain_id, cache, ...}` files, as written
    /// by `--rpc.capture-file` and `--rpc.cache-dir`).
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

impl MergeArgs {
    /// Merge inputs into `--output` and print a one-line summary.
    pub fn run(self) -> Result<()> {
        if self.inputs.is_empty() {
            return Err(EvmeError::InvalidInput(
                "cache merge requires at least one input file".into(),
            ));
        }

        // Every input is loaded (and therefore shape-checked) before the output
        // is locked, so a doomed merge leaves no sidecar behind. Chain identity
        // needs no separate pass: it is carried inside each envelope and checked
        // by `merge_envelopes_cli`.
        let mut loaded: Vec<(PathBuf, EnvelopeDoc)> = Vec::with_capacity(self.inputs.len());
        for path in &self.inputs {
            loaded.push((path.clone(), load_cache_file(path)?));
        }

        let total_in: usize = loaded.iter().map(|(_, doc)| doc.cache.len()).sum();
        let input_count = loaded.len();

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

        let merged = merge_envelopes_cli(&loaded)?;

        // Whatever is at the output now joins the union as one more input: a
        // concurrent writer may have landed entries there while this merge
        // waited for the lock.
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
                    // Replacing the output drops whatever it held: the user must
                    // hear about it whatever the verbosity is.
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

        let unique_out = merged.cache.len();
        write_envelope_atomic(&self.output, &merged)?;

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

    /// A non-envelope input is refused with the one diagnostic that names what
    /// the file probably is and what to do about it. Refused before the output
    /// is locked, so a doomed merge leaves no sidecar behind.
    #[test]
    fn test_cache_merge_rejects_a_non_envelope_input() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("rpc-cache-4326.json");
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
        // The bare array a retired build wrote.
        write(&b, &serde_json::to_string(&vec![kv(1, "legacy")]).unwrap());

        let err = MergeArgs { inputs: vec![a, b], output: out.clone() }.run().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not a cache envelope"), "msg={msg}");
        assert!(msg.contains("bare JSON array"), "the likely cause must be named: msg={msg}");
        assert!(msg.contains("Delete it"), "the way forward must be named: msg={msg}");
        assert!(msg.contains("rpc-cache-4326.json"), "the offending file must be named: msg={msg}");
        assert!(!out.exists(), "a refused merge writes nothing");
        assert!(!lock_sidecar_path(&out).exists(), "a refused merge locks nothing");
    }

    /// The same refusal for structured JSON that is neither shape: the merge
    /// reads envelopes, and everything else gets the same actionable answer.
    #[test]
    fn test_cache_merge_rejects_an_unrecognized_input_shape() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.json");
        let out = dir.path().join("out.json");

        write(&a, r#"{"something":"else"}"#);

        let err = MergeArgs { inputs: vec![a], output: out.clone() }.run().unwrap_err();
        assert!(err.to_string().contains("not a cache envelope"), "msg={err}");
        assert!(!out.exists(), "a refused merge writes nothing");
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

    /// An envelope output already on disk joins the union as one more input:
    /// entries a concurrent writer left there survive the merge, and the named
    /// inputs win where the keys collide.
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

    /// A file in the retired array format sitting at the merge's output is a
    /// hard error: it cannot be folded, and overwriting it would destroy it.
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

    /// The merge fails closed when the output lock cannot be acquired: the
    /// existing output is left exactly as it was, and the failure names the lock.
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
        let msg = err.to_string();
        assert!(msg.contains("lock"), "msg={msg}");
        assert!(msg.contains("out.json.lock"), "the lock path must be named: msg={msg}");
        assert_eq!(fs::read_to_string(&out).unwrap(), existing, "no unlocked write happened");
    }
}
