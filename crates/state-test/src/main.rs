//! State test crate

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use clap::Parser;
use state_test::{
    runner::{
        bench_test_suite, fill_test_suite, find_all_json_tests, run, TestError, TestErrorKind,
        UnitBench,
    },
    types::SpecName,
};
use std::{path::PathBuf, str::FromStr};

use mega_evm::MegaSpecId;
use serde_json::json;

/// `statetest` subcommand
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Path to folder or file containing the tests
    ///
    /// If multiple paths are specified they will be run in sequence.
    ///
    /// Folders will be searched recursively for files with the extension `.json`.
    #[arg(required = true, num_args = 1..)]
    paths: Vec<PathBuf>,
    /// Run tests in a single thread
    #[arg(short = 's', long)]
    single_thread: bool,
    /// Output results in JSON format
    ///
    /// It will stop second run of evm on failure.
    #[arg(long)]
    json: bool,
    /// Output outcome in JSON format
    ///
    /// If `--json` is true, this is implied.
    ///
    /// It will stop second run of EVM on failure.
    #[arg(short = 'o', long)]
    json_outcome: bool,
    /// Keep going after a test failure
    #[arg(long, alias = "no-fail-fast")]
    keep_going: bool,
    /// Benchmark each fixture's isolated EVM execution instead of validating it.
    ///
    /// Emits per-unit timing (min/median/mean) and Mgas/s as JSON. The fixture
    /// is self-contained, so this needs no RPC — any state-test fixture (a
    /// dumped replay, a prestate snapshot, a hand-crafted case) can be measured.
    #[arg(long)]
    bench: bool,
    /// Timed iterations per unit when `--bench` is set.
    #[arg(long, default_value_t = 50)]
    bench_runs: u32,
    /// Discarded warmup iterations before timing when `--bench` is set.
    #[arg(long, default_value_t = 5)]
    bench_warmup: u32,
    /// Spec to benchmark / fill under (default: the fixture's single `post` spec).
    #[arg(long, value_name = "SPEC")]
    bench_spec: Option<String>,
    /// Compute and write each fixture's `post` expectation in place.
    ///
    /// The offline analog of `--dump-fixture`'s post-fill: makes a fixture that
    /// has no `post` (a hand-built or prestate-snapshot case) self-validating.
    /// Use `--bench-spec` to choose the spec when the fixture has no `post` yet.
    /// Refuses fixtures that already have a `post` unless `--force` is set.
    #[arg(long, conflicts_with_all = ["bench", "bench_runs", "bench_warmup"])]
    fill: bool,
    /// Overwrite an existing non-empty `post` when filling with `--fill`.
    #[arg(long, requires = "fill")]
    force: bool,
}

impl Cmd {
    /// Runs `statetest` command.
    pub fn run(&self) -> Result<(), TestError> {
        if self.fill {
            return self.run_fill();
        }
        if self.bench {
            return self.run_bench();
        }
        for path in &self.paths {
            if !path.exists() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::InvalidPath,
                });
            }

            println!("\nRunning tests in {}...", path.display());
            let test_files = find_all_json_tests(path);

            if test_files.is_empty() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::NoJsonFiles,
                });
            }

            run(test_files, self.single_thread, self.json, self.json_outcome, self.keep_going)?
        }
        Ok(())
    }

    /// Parse `--bench-spec` into a [`SpecName`], if given.
    fn resolve_spec(&self) -> Result<Option<SpecName>, TestError> {
        self.bench_spec
            .as_deref()
            .map(|s| {
                let invalid_spec = || TestError {
                    name: "spec".to_string(),
                    path: s.to_string(),
                    kind: TestErrorKind::FixtureError(format!(
                        "invalid --bench-spec {s:?}; expected one of: {}",
                        [
                            mega_evm::name::EQUIVALENCE,
                            mega_evm::name::MINI_REX,
                            mega_evm::name::REX,
                            mega_evm::name::REX1,
                            mega_evm::name::REX2,
                            mega_evm::name::REX3,
                            mega_evm::name::REX4,
                            mega_evm::name::REX5,
                        ]
                        .join(", ")
                    )),
                };
                let spec = MegaSpecId::from_str(s)
                    .map(SpecName::from_mega_spec)
                    .map_err(|_| invalid_spec())?;
                // A spec id that parses but has no fixture-facing name (a
                // future `MegaSpecId` this crate does not map yet) would
                // otherwise fail much later, deep inside execution — reject it
                // here with the same actionable message.
                if spec == SpecName::Unknown {
                    return Err(invalid_spec());
                }
                Ok(spec)
            })
            .transpose()
    }

    /// Fill every fixture's `post` expectation in place (see `--fill`).
    fn run_fill(&self) -> Result<(), TestError> {
        let spec_override = self.resolve_spec()?;
        for path in &self.paths {
            if !path.exists() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::InvalidPath,
                });
            }
            for file in find_all_json_tests(path) {
                let n = fill_test_suite(&file, spec_override, self.force)?;
                println!("Filled post for {n} unit(s) in {}", file.display());
            }
        }
        Ok(())
    }

    /// Benchmark every fixture under the given paths and print the results as JSON.
    ///
    /// A single benchmarked unit prints one object `{ gas_used, success, bench }`;
    /// multiple units print a JSON array of `{ name, ... }` objects. The
    /// replay-bench driver (`bench/replay/run.py`) consumes this output.
    fn run_bench(&self) -> Result<(), TestError> {
        let spec_override = self.resolve_spec()?;

        let mut all: Vec<UnitBench> = Vec::new();
        for path in &self.paths {
            if !path.exists() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::InvalidPath,
                });
            }
            for file in find_all_json_tests(path) {
                all.extend(bench_test_suite(
                    &file,
                    self.bench_runs,
                    self.bench_warmup,
                    spec_override,
                )?);
            }
        }

        let bench_json = |u: &UnitBench| {
            json!({
                "runs": u.runs,
                "gasUsed": u.gas_used,
                "minNs": u.min.as_nanos(),
                "medianNs": u.median.as_nanos(),
                "meanNs": u.mean.as_nanos(),
                "mgasPerSec": u.mgas_per_sec(),
            })
        };
        let output = if all.len() == 1 {
            let u = &all[0];
            json!({ "gas_used": u.gas_used, "success": u.success, "bench": bench_json(u) })
        } else {
            json!(all
                .iter()
                .map(|u| json!({
                    "name": u.name,
                    "gas_used": u.gas_used,
                    "success": u.success,
                    "bench": bench_json(u),
                }))
                .collect::<Vec<_>>())
        };
        println!("{}", serde_json::to_string_pretty(&output).expect("serialize bench output"));
        Ok(())
    }
}

fn main() {
    let cmd = Cmd::parse();
    // CI exit-code contract: any error — including `TestsFailed` when tests
    // fail under `--keep-going` — prints to stderr and exits with code 1.
    if let Err(e) = cmd.run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd_with_bench_spec(spec: &str) -> Cmd {
        Cmd::parse_from(["state-test", "fixture.json", "--bench-spec", spec])
    }

    #[test]
    fn resolve_spec_none_when_absent() {
        let cmd = Cmd::parse_from(["state-test", "fixture.json"]);
        assert_eq!(cmd.resolve_spec().expect("no spec is fine"), None);
    }

    #[test]
    fn resolve_spec_accepts_every_known_spec() {
        for (s, expected) in [
            (mega_evm::name::EQUIVALENCE, SpecName::Equivalence),
            (mega_evm::name::MINI_REX, SpecName::MiniRex),
            (mega_evm::name::REX, SpecName::Rex),
            (mega_evm::name::REX1, SpecName::Rex1),
            (mega_evm::name::REX2, SpecName::Rex2),
            (mega_evm::name::REX3, SpecName::Rex3),
            (mega_evm::name::REX4, SpecName::Rex4),
            (mega_evm::name::REX5, SpecName::Rex5),
        ] {
            let spec = cmd_with_bench_spec(s).resolve_spec().expect("valid spec").expect("present");
            assert_eq!(spec, expected, "--bench-spec {s}");
            // No accepted spec may slip through as Unknown and fail later.
            assert_ne!(spec, SpecName::Unknown, "--bench-spec {s}");
        }
    }

    #[test]
    fn resolve_spec_rejects_unparseable_string() {
        let err = cmd_with_bench_spec("FutureFork9000")
            .resolve_spec()
            .expect_err("unknown spec string must be rejected");
        assert!(
            err.to_string().contains("invalid --bench-spec"),
            "error should be actionable: {err}"
        );
    }
}
