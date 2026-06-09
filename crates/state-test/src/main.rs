//! State test crate

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use clap::Parser;
use state_test::{
    runner::{bench_test_suite, find_all_json_tests, run, TestError, TestErrorKind, UnitBench},
    types::SpecName,
};
use std::{path::PathBuf, str::FromStr};

use mega_evm::MegaSpecId;
use serde_json::json;

// Dependencies used by the library
use alloy_eips as _;
use alloy_primitives as _;
use alloy_rlp as _;
use alloy_serde as _;
use hash_db as _;
use indicatif as _;
use k256 as _;
use plain_hasher as _;
use serde as _;
use thiserror as _;
use triehash as _;
use walkdir as _;

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
    /// Spec to benchmark under (default: the fixture's single `post` spec).
    #[arg(long, value_name = "SPEC")]
    bench_spec: Option<String>,
}

impl Cmd {
    /// Runs `statetest` command.
    pub fn run(&self) -> Result<(), TestError> {
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

    /// Benchmark every fixture under the given paths and print the results as JSON.
    ///
    /// A single benchmarked unit prints one object `{ gas_used, success, bench }`
    /// — the same shape `mega-evme replay --bench-runs --json` emits — so the
    /// replay-bench driver parses captured-tx and fixture cases identically.
    /// Multiple units print a JSON array of `{ name, ... }` objects.
    fn run_bench(&self) -> Result<(), TestError> {
        let spec_override = self
            .bench_spec
            .as_deref()
            .map(|s| {
                MegaSpecId::from_str(s).map(SpecName::from_mega_spec).map_err(|e| TestError {
                    name: "bench".to_string(),
                    path: s.to_string(),
                    kind: TestErrorKind::FixtureError(format!("invalid --bench-spec {s:?}: {e:?}")),
                })
            })
            .transpose()?;

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
    cmd.run().unwrap();
}
