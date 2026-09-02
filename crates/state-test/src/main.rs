//! State test crate

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use clap::Parser;
use state_test::{
    chaos::{run_chaos, ChaosClass, ChaosRunConfig, ChaosShape, ChaosSweepTally, ShapeFilter},
    diff::{collect_fixture_files, run_diff, DiffClass, DiffRunConfig, DiffSpecs, DiffTally},
    runner::{
        bench_test_suite, fill_test_suite, fill_test_suite_keep_going, find_all_json_tests,
        is_skipped_fixture, run, TestError, TestErrorKind, UnitBench, UnitStatus,
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
    /// Execute each fixture under this spec as well and report how the two differ.
    ///
    /// The spec under test is `--bench-spec`, which is therefore required: a differential run
    /// compares two named specs, and taking the target from each fixture's own `post` would make
    /// the comparison mean something different from one unit to the next. Nothing is written —
    /// the comparison is between the two executions, not against a recorded expectation, which is
    /// what lets an unstable spec with no expectations be checked at all.
    #[arg(long, value_name = "SPEC", requires = "bench_spec", conflicts_with_all = ["bench", "fill"])]
    diff_spec: Option<String>,
    /// Write the differential run's tally and every flagged unit to this file, as JSON.
    #[arg(long, value_name = "FILE", requires = "diff_spec")]
    diff_report: Option<PathBuf>,
    /// Skip the inspected second pass that collects per-frame evidence for a difference the
    /// cheap evidence did not explain.
    ///
    /// Only useful for measuring the cost of that pass: without it, a difference caused by an
    /// inner frame the transaction's own result hides is reported as unexplained.
    #[arg(long, requires = "diff_spec")]
    diff_no_frame_evidence: bool,
    /// Execute each fixture three times — with no inspector, with a read-only one, and with a
    /// deterministic rewriting one seeded from this value and the vector's own identity — and
    /// report how the three came out.
    ///
    /// The spec every run executes under is `--bench-spec`, which is therefore required. Nothing
    /// is written and nothing is compared against a recorded expectation: the read-only run is
    /// judged against the run with no inspector, and the rewriting run is judged by whether the
    /// execution's own gas-accounting cross-checks survive it. Those cross-checks are debug
    /// assertions, so this mode is only meaningful in a build that keeps them.
    #[arg(long, value_name = "SEED", requires = "bench_spec", conflicts_with_all = ["bench", "fill", "diff_spec"])]
    chaos_seed: Option<u64>,
    /// Write the chaos run's tally and every flagged vector to this file, as JSON.
    #[arg(long, value_name = "FILE", requires = "chaos_seed")]
    chaos_report: Option<PathBuf>,
    /// Restrict the rewriting run to these shapes (comma-separated labels).
    ///
    /// For triage: a flagged vector is re-run with the list narrowed until the smallest set that
    /// still reproduces it is found. Narrowing does not reshuffle the decision stream, so each
    /// surviving mutation stays where the full run put it; it does leave the mutation budget
    /// unspent on rejected draws, so a narrowed run can reach further into a transaction.
    #[arg(long, value_name = "SHAPES", value_delimiter = ',', requires = "chaos_seed")]
    chaos_shapes: Vec<String>,
}

impl Cmd {
    /// Runs `statetest` command.
    pub fn run(&self) -> Result<(), TestError> {
        if self.diff_spec.is_some() {
            return self.run_diff();
        }
        if self.chaos_seed.is_some() {
            return self.run_chaos();
        }
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
            let scan = find_all_json_tests(path);
            // A directory the walk could not descend into contributes no fixtures, which looks
            // exactly like a directory that holds none. Fail rather than run the part that was
            // readable and report it as the whole.
            if let Some(err) = scan.errors.first() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::FixtureError(format!(
                        "{} path(s) could not be read; first: {err}",
                        scan.errors.len()
                    )),
                });
            }

            if scan.files.is_empty() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::NoJsonFiles,
                });
            }

            run(scan.files, self.single_thread, self.json, self.json_outcome, self.keep_going)?
        }
        Ok(())
    }

    /// Parse `--bench-spec` into a [`SpecName`], if given.
    fn resolve_spec(&self) -> Result<Option<SpecName>, TestError> {
        self.bench_spec.as_deref().map(|s| parse_spec("--bench-spec", s)).transpose()
    }

    /// Parse `--diff-spec` into a [`SpecName`], if given.
    fn resolve_diff_spec(&self) -> Result<Option<SpecName>, TestError> {
        self.diff_spec.as_deref().map(|s| parse_spec("--diff-spec", s)).transpose()
    }

    /// Fill every fixture's `post` expectation in place (see `--fill`).
    fn run_fill(&self) -> Result<(), TestError> {
        let spec_override = self.resolve_spec()?;
        let (mut filled, mut errors, mut panics) = (0usize, 0usize, 0usize);
        let (mut file_errors, mut skipped_files) = (0usize, 0usize);
        for path in &self.paths {
            if !path.exists() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::InvalidPath,
                });
            }
            let scan = find_all_json_tests(path);
            // Same hole as in the other modes, reported the way this mode reports a file it could
            // not read: as a FILE_ERR that the tally's gate counts.
            for err in &scan.errors {
                println!("FILE_ERR\t{}\t{}", path.display(), err.replace('\n', " "));
                file_errors += 1;
            }
            if !self.keep_going && !scan.errors.is_empty() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::FixtureError(format!(
                        "{} path(s) could not be read",
                        scan.errors.len()
                    )),
                });
            }
            for file in scan.files {
                if self.keep_going {
                    // A file the runner declines as a whole (an unreadable fixture, a filename on
                    // the validation skip list) must not end the sweep either: record it and move
                    // on, the same way a declined unit is recorded.
                    if is_skipped_fixture(&file) {
                        println!("SKIP_FILE\t{}", file.display());
                        skipped_files += 1;
                        continue;
                    }
                    let report = match fill_test_suite_keep_going(&file, spec_override, self.force)
                    {
                        Ok(report) => report,
                        Err(e) => {
                            println!(
                                "FILE_ERR\t{}\t{}",
                                file.display(),
                                e.to_string().replace('\n', " ")
                            );
                            file_errors += 1;
                            continue;
                        }
                    };
                    for vector in &report.vectors {
                        match &vector.status {
                            UnitStatus::Ok => {}
                            UnitStatus::Error(m) => {
                                println!("ERR\t{}::{}\t{m}", file.display(), vector.name);
                                errors += 1;
                            }
                            UnitStatus::Panic(m) => {
                                println!(
                                    "PANIC\t{}::{}\t{}",
                                    file.display(),
                                    vector.name,
                                    m.replace('\n', " ")
                                );
                                panics += 1;
                            }
                        }
                    }
                    filled += report.filled();
                } else {
                    let n = fill_test_suite(&file, spec_override, self.force)?;
                    println!("Filled post for {n} transaction vector(s) in {}", file.display());
                    filled += n;
                }
            }
        }
        // A sweep that filled and declined nothing reached no transaction vector at all: an empty
        // corpus, or one whose every file was unreadable or skipped. Its zeroes are truthful and
        // meaningless, so they must not read as a pass — in either mode. Without `--keep-going`
        // the run stops at the first failure, which says nothing about the case where there was
        // no work to fail at.
        let total = filled + errors + panics;
        if self.keep_going {
            println!(
                "Fill tally: OK={filled} ERR={errors} PANIC={panics} FILE_ERR={file_errors} \
                 SKIP_FILE={skipped_files} TOTAL={total}"
            );
        }
        if total == 0 {
            return Err(TestError {
                name: "fill summary".to_string(),
                path: String::new(),
                kind: TestErrorKind::FixtureError(
                    "no transaction vector was filled; the corpus is empty or unreachable"
                        .to_string(),
                ),
            });
        }
        if !self.keep_going {
            return Ok(());
        }
        if errors + panics + file_errors == 0 {
            return Ok(());
        }
        // `--keep-going` changes when the run stops, not whether it failed: the CLI's exit-code
        // contract still reports a unit that did not fill.
        Err(TestError {
            name: "fill summary".to_string(),
            path: String::new(),
            kind: TestErrorKind::TestsFailed { failed: errors + panics + file_errors, total },
        })
    }

    /// Execute every fixture under both specs and report how they differ (see `--diff-spec`).
    fn run_diff(&self) -> Result<(), TestError> {
        let base = self.resolve_diff_spec()?.expect("run_diff is only reached with --diff-spec");
        // Clap's `requires = "bench_spec"` makes the target explicit before this point.
        let target = self.resolve_spec()?.expect("--diff-spec requires --bench-spec");
        // The comparison is decided by Rex7's precision invariant, which relates Rex7 to Rex6 and
        // states nothing about any other pair; running it over one would apply that licence where
        // none was granted.
        let specs = DiffSpecs::new(target, base).map_err(|detail| TestError {
            name: "spec pair".to_string(),
            path: String::new(),
            kind: TestErrorKind::FixtureError(detail),
        })?;
        let scan = collect_fixture_files(&self.paths)?;

        let tally = run_diff(
            scan,
            DiffRunConfig {
                specs,
                single_thread: self.single_thread,
                collect_evidence: !self.diff_no_frame_evidence,
                progress: !self.json,
            },
        );

        print_diff_tally(&tally, target, base);
        if let Some(report) = &self.diff_report {
            let json = serde_json::to_string_pretty(&diff_report_json(&tally, target, base))
                .expect("serialize diff report");
            std::fs::write(report, json).map_err(|e| TestError {
                name: "diff report".to_string(),
                path: report.display().to_string(),
                kind: TestErrorKind::FixtureError(format!("write: {e}")),
            })?;
        }

        if !tally.is_failure() {
            return Ok(());
        }
        Err(TestError {
            name: "diff summary".to_string(),
            path: String::new(),
            kind: TestErrorKind::TestsFailed {
                failed: tally.count(DiffClass::Unexplained) + tally.count(DiffClass::Panic),
                total: tally.total(),
            },
        })
    }

    /// Build the chaos run's shape filter from `--chaos-shapes`.
    fn resolve_chaos_filter(&self) -> Result<ShapeFilter, TestError> {
        if self.chaos_shapes.is_empty() {
            return Ok(ShapeFilter::default());
        }
        let shapes = self
            .chaos_shapes
            .iter()
            .map(|label| ChaosShape::parse(label))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|detail| TestError {
                name: "--chaos-shapes".to_string(),
                path: String::new(),
                kind: TestErrorKind::FixtureError(detail),
            })?;
        Ok(ShapeFilter::only(&shapes))
    }

    /// Sweep the corpus under a deterministic rewriting inspector (see `--chaos-seed`).
    fn run_chaos(&self) -> Result<(), TestError> {
        let seed = self.chaos_seed.expect("run_chaos is only reached with --chaos-seed");
        // Clap's `requires = "bench_spec"` makes the spec explicit before this point.
        let spec = self.resolve_spec()?.expect("--chaos-seed requires --bench-spec");
        let filter = self.resolve_chaos_filter()?;
        let scan = collect_fixture_files(&self.paths)?;

        let tally = run_chaos(
            scan,
            ChaosRunConfig {
                spec,
                seed,
                filter,
                single_thread: self.single_thread,
                progress: !self.json,
            },
        );

        print_chaos_tally(&tally, spec, seed, filter);
        if let Some(report) = &self.chaos_report {
            let json = serde_json::to_string_pretty(&chaos_report_json(&tally, spec, seed, filter))
                .expect("serialize chaos report");
            std::fs::write(report, json).map_err(|e| TestError {
                name: "chaos report".to_string(),
                path: report.display().to_string(),
                kind: TestErrorKind::FixtureError(format!("write: {e}")),
            })?;
        }

        if !tally.is_failure() {
            return Ok(());
        }
        Err(TestError {
            name: "chaos summary".to_string(),
            path: String::new(),
            kind: TestErrorKind::TestsFailed {
                failed: tally.flagged.len().max(1),
                total: tally.total(),
            },
        })
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
            let scan = find_all_json_tests(path);
            if let Some(err) = scan.errors.first() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::FixtureError(format!(
                        "{} path(s) could not be read; first: {err}",
                        scan.errors.len()
                    )),
                });
            }
            for file in scan.files {
                all.extend(bench_test_suite(
                    &file,
                    self.bench_runs,
                    self.bench_warmup,
                    spec_override,
                )?);
            }
        }

        if all.is_empty() {
            return Err(TestError {
                name: "bench".to_string(),
                path: self
                    .paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                kind: TestErrorKind::FixtureError(
                    "no unit was benchmarked; the corpus is empty or unreachable".to_string(),
                ),
            });
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

/// Parse a spec-name flag value into a [`SpecName`].
///
/// Rejects both an unparseable string and a `MegaSpecId` this crate has no fixture-facing name
/// for; either would otherwise fail much later, deep inside execution.
fn parse_spec(flag: &str, value: &str) -> Result<SpecName, TestError> {
    let invalid_spec = || TestError {
        name: "spec".to_string(),
        path: value.to_string(),
        kind: TestErrorKind::FixtureError(format!(
            "invalid {flag} {value:?}; expected one of: {}",
            [
                mega_evm::name::EQUIVALENCE,
                mega_evm::name::MINI_REX,
                mega_evm::name::REX,
                mega_evm::name::REX1,
                mega_evm::name::REX2,
                mega_evm::name::REX3,
                mega_evm::name::REX4,
                mega_evm::name::REX5,
                mega_evm::name::REX6,
                mega_evm::name::REX7,
            ]
            .join(", ")
        )),
    };
    let spec =
        MegaSpecId::from_str(value).map(SpecName::from_mega_spec).map_err(|_| invalid_spec())?;
    if spec == SpecName::Unknown {
        return Err(invalid_spec());
    }
    Ok(spec)
}

/// Every class a differential run can produce, in report order.
const DIFF_CLASSES: [DiffClass; 5] = [
    DiffClass::Pass,
    DiffClass::Explained,
    DiffClass::Unexplained,
    DiffClass::Skipped,
    DiffClass::Panic,
];

/// Prints the differential run's tally, mechanism distribution, and every flagged unit.
fn print_diff_tally(tally: &DiffTally, target: SpecName, base: SpecName) {
    println!("\nDifferential run: {target:?} vs {base:?} over {} unit(s)", tally.total());
    for class in DIFF_CLASSES {
        println!("  {:<12} {}", class.label(), tally.count(class));
    }
    if tally.skipped_files > 0 {
        println!("  ({} file(s) skipped by filename, no unit of them judged)", tally.skipped_files);
    }
    if !tally.mechanisms.is_empty() {
        println!("Mechanisms over explained differences:");
        for (label, count) in &tally.mechanisms {
            println!("  {label:<28} {count}");
        }
    }
    if !tally.explained_fields.is_empty() {
        println!("Shapes of explained differences (disagreeing quantities):");
        for (shape, count) in &tally.explained_fields {
            println!("  {shape:<48} {count}");
        }
    }
    for diff in &tally.flagged {
        println!(
            "{}\t{}::{}\t{}\t{}",
            diff.class.label(),
            diff.path,
            diff.name,
            diff.fields.iter().map(|f| f.label()).collect::<Vec<_>>().join(","),
            diff.detail.as_deref().unwrap_or("-").replace('\n', " ")
        );
    }
    for error in &tally.file_errors {
        println!("FILE_ERROR\t{}", error.replace('\n', " "));
    }
}

/// Every chaos verdict, in the order a reader wants them.
const CHAOS_CLASSES: [ChaosClass; 7] = [
    ChaosClass::Pass,
    ChaosClass::Refused,
    ChaosClass::ControlDrift,
    ChaosClass::ChaosRejected,
    ChaosClass::LedgerBlind,
    ChaosClass::Skipped,
    ChaosClass::Panic,
];

/// Prints the chaos run's tally, plus every vector that needs a human.
fn print_chaos_tally(tally: &ChaosSweepTally, spec: SpecName, seed: u64, filter: ShapeFilter) {
    println!("\nChaos run: {spec:?} under seed {seed} over {} vector(s)", tally.total());
    if !filter.is_complete() {
        println!("  (shape filter: {})", chaos_filter_label(filter));
    }
    for class in CHAOS_CLASSES {
        println!("  {:<16} {}", class.label(), tally.count(class));
    }
    if tally.skipped_files > 0 {
        println!(
            "  ({} file(s) skipped by filename, no vector of them judged)",
            tally.skipped_files
        );
    }
    println!(
        "Mutations applied: {} over {} callback(s)",
        tally.shapes.total(),
        tally.shapes.callbacks
    );
    for (shape, count) in &tally.shapes.applied {
        println!("  {shape:<20} {count}");
    }
    for verdict in &tally.flagged {
        println!(
            "{}\t{}::{}\tseed={}\t{}",
            verdict.class.label(),
            verdict.path,
            verdict.name,
            verdict.seed,
            verdict.detail.as_deref().unwrap_or("-").replace('\n', " ")
        );
    }
    for error in &tally.file_errors {
        println!("FILE_ERROR\t{}", error.replace('\n', " "));
    }
}

/// The machine-readable form of [`print_chaos_tally`], for `--chaos-report`.
fn chaos_report_json(
    tally: &ChaosSweepTally,
    spec: SpecName,
    seed: u64,
    filter: ShapeFilter,
) -> serde_json::Value {
    json!({
        "spec": format!("{spec:?}"),
        "seed": seed,
        "shapeFilter": chaos_filter_label(filter),
        "total": tally.total(),
        "classes": CHAOS_CLASSES
            .iter()
            .map(|c| (c.label().to_string(), json!(tally.count(*c))))
            .collect::<serde_json::Map<_, _>>(),
        "callbacks": tally.shapes.callbacks,
        "mutations": tally.shapes.total(),
        "mutationsByShape": tally.shapes.applied,
        "fileErrors": tally.file_errors,
        "skippedFiles": tally.skipped_files,
        "flagged": tally
            .flagged
            .iter()
            .map(|v| json!({
                "class": v.class.label(),
                "path": v.path,
                "name": v.name,
                "seed": v.seed,
                "mutations": v.mutations,
                "detail": v.detail,
            }))
            .collect::<Vec<_>>(),
    })
}

/// What a chaos run's shape filter allows, as one line.
fn chaos_filter_label(filter: ShapeFilter) -> String {
    ChaosShape::ALL
        .into_iter()
        .filter(|shape| filter.allows(*shape))
        .map(|shape| shape.label().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The machine-readable form of [`print_diff_tally`], for `--diff-report`.
fn diff_report_json(tally: &DiffTally, target: SpecName, base: SpecName) -> serde_json::Value {
    json!({
        "targetSpec": format!("{target:?}"),
        "baseSpec": format!("{base:?}"),
        "total": tally.total(),
        "classes": DIFF_CLASSES
            .iter()
            .map(|c| (c.label().to_string(), json!(tally.count(*c))))
            .collect::<serde_json::Map<_, _>>(),
        "mechanisms": tally.mechanisms,
        "explainedFields": tally.explained_fields,
        "fileErrors": tally.file_errors,
        "skippedFiles": tally.skipped_files,
        "flagged": tally
            .flagged
            .iter()
            .map(|d| json!({
                "class": d.class.label(),
                "path": d.path,
                "name": d.name,
                "fields": d.fields.iter().map(|f| f.label()).collect::<Vec<_>>(),
                "mechanisms": d.mechanisms.iter().map(|m| m.label()).collect::<Vec<_>>(),
                "detail": d.detail,
            }))
            .collect::<Vec<_>>(),
    })
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
