//! `state-test btest`: the execution-spec blockchain tests, imported through Satin's block
//! executor.
//!
//! `state-test btest <paths>` imports every Osaka test of the blockchain fixtures under `paths`
//! and exits non-zero when a failure is not explained by a registered deviation, when a fixture
//! file cannot be read, or when a count differs from the pin it is given. See the
//! `mega-state-test` crate's `blockchain` module for how a test is judged.

use std::{collections::BTreeMap, path::PathBuf, process::ExitCode};

use clap::Parser;
use state_test::{
    blockchain::{run, Config, Outcome, Report, SkipReason, Summary, TestResult, NETWORK},
    deviations,
    runner::find_json_files,
};

/// How many unattributed failures, and how many unreproduced tests, the summary lists.
const LISTED_FAILURES: usize = 50;

/// Command-line arguments of `state-test btest`.
#[derive(Parser, Debug)]
#[command(
    name = "state-test btest",
    about = "Imports the execution-spec blockchain tests through Satin's block executor"
)]
pub(crate) struct Cmd {
    /// Fixture files, or directories searched recursively for `.json` files.
    #[arg(required = true, num_args = 1..)]
    paths: Vec<PathBuf>,
    /// Worker threads (default: one per core).
    #[arg(long)]
    threads: Option<usize>,
    /// Print one JSON line per test to standard error.
    #[arg(long)]
    json_outcome: bool,
    /// Write the summary, as JSON, to this file.
    #[arg(long, value_name = "FILE")]
    summary_json: Option<PathBuf>,
    /// Fail unless exactly this many tests execute.
    #[arg(long, value_name = "N")]
    expect_executed: Option<usize>,
    /// Fail unless exactly this many tests are skipped for this reason; once one is given, every
    /// reason not given is pinned at zero. Repeat it for each reason.
    #[arg(long, value_name = "REASON=N", value_parser = parse_pin)]
    expect_skipped: Vec<(SkipReason, usize)>,
    /// Fail unless every blockchain test a registered deviation lists fails exactly as listed.
    #[arg(long)]
    expect_deviations: bool,
}

/// A `REASON=N` pin.
fn parse_pin(pin: &str) -> Result<(SkipReason, usize), String> {
    let (reason, count) =
        pin.split_once('=').ok_or_else(|| format!("expected REASON=N, got {pin:?}"))?;
    let count = count.parse().map_err(|error| format!("{count:?}: {error}"))?;
    Ok((reason.parse()?, count))
}

/// Runs `state-test btest` with `args`, the subcommand's name first.
pub(crate) fn main(args: impl IntoIterator<Item = std::ffi::OsString>) -> ExitCode {
    let cmd = Cmd::parse_from(args);
    let mut pins = BTreeMap::new();
    for (reason, count) in &cmd.expect_skipped {
        if pins.insert(*reason, *count).is_some() {
            eprintln!("error: --expect-skipped pins {} twice", reason.name());
            return ExitCode::FAILURE;
        }
    }
    let mut files = Vec::new();
    for path in &cmd.paths {
        if !path.exists() {
            eprintln!("error: {} does not exist", path.display());
            return ExitCode::FAILURE;
        }
        let found = find_json_files(path);
        if found.is_empty() {
            eprintln!("error: no JSON fixtures under {}", path.display());
            return ExitCode::FAILURE;
        }
        files.extend(found);
    }

    let threads =
        cmd.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let config =
        Config { threads, json_outcome: cmd.json_outcome, deviations: deviations::DEVIATIONS };
    let report = run(&files, config);
    let summary = report.summary();
    print_summary(&report, &summary);

    if let Some(path) = &cmd.summary_json {
        let json = serde_json::json!({
            "suite": "blockchain",
            "fork": NETWORK,
            "summary": summary,
            "skip_reasons": skip_reasons(),
        });
        let json = serde_json::to_string_pretty(&json).expect("a summary serializes");
        if let Err(error) = std::fs::write(path, json + "\n") {
            eprintln!("error: writing {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    }

    let pins = (!pins.is_empty()).then_some(&pins);
    let problems = report.gate(cmd.expect_executed, pins, cmd.expect_deviations);
    for problem in &problems {
        println!("gate: {problem}");
    }
    if problems.is_empty() {
        println!("gate: passed");
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Every skip class's reason, by its name.
fn skip_reasons() -> BTreeMap<&'static str, &'static str> {
    SkipReason::ALL.iter().map(|reason| (reason.name(), reason.reason())).collect()
}

fn print_summary(report: &Report, summary: &Summary) {
    println!("blockchain tests, {NETWORK} fixtures: {} files", summary.files);
    println!(
        "  defined {}  executed {}  passed {}  deviated {}  failed {}  skipped {}",
        summary.defined,
        summary.executed,
        summary.passed,
        summary.deviated_total(),
        summary.failed_total(),
        summary.skipped_total()
    );
    let blocks = summary.blocks;
    println!(
        "  blocks checked against Ethereum {} (matched {}, refused as expected {})  matched to a \
         deviation {}",
        blocks.checked(),
        blocks.matched,
        blocks.refused,
        blocks.deviated
    );
    for (reason, count) in &summary.skipped {
        println!("  skipped  {:<34} {count:<6} {}", reason.name(), reason.reason());
    }
    for (kind, count) in &summary.failed {
        println!("  failed   {:<34} {count}", kind.name());
    }
    for deviation in report.deviations {
        let listed = deviation.blockchain_entries.len();
        if listed > 0 {
            let deviated = summary.deviated.get(deviation.id).copied().unwrap_or(0);
            println!("  deviated {:<34} {deviated} (listed {listed})", deviation.id);
        }
    }
    println!("  unattributed {}", summary.unattributed);
    for (id, failure) in report.unattributed().take(LISTED_FAILURES) {
        println!("    {id}\n      {}: {}", failure.kind.name(), failure.detail);
    }
    if summary.unattributed > LISTED_FAILURES {
        println!("    ... and {} more", summary.unattributed - LISTED_FAILURES);
    }
    let unreproduced: usize = summary.unreproduced.values().sum();
    println!("  unreproduced {unreproduced}");
    for entry in report.unreproduced().take(LISTED_FAILURES) {
        println!(
            "    {}\n      {} lists {} blocks; {}",
            entry.entry,
            entry.deviation.id,
            entry.entry.blocks.len(),
            seen(&entry.seen)
        );
    }
    if unreproduced > LISTED_FAILURES {
        println!("    ... and {} more", unreproduced - LISTED_FAILURES);
    }
    for (path, failure) in &report.file_failures {
        println!("  unreadable {path}\n      {}", failure.detail);
    }
}

/// What a run did with a listed test, given the results it has for it.
fn seen(results: &[&TestResult]) -> String {
    match results {
        [] => "the run did not execute it".into(),
        [result] => match &result.outcome {
            Outcome::Passed => "it passed".into(),
            Outcome::Deviated { deviation } => format!("it deviated under {deviation}"),
            Outcome::Skipped { reason } => format!("it was skipped: {}", reason.name()),
            Outcome::Failed(failure) => {
                format!("it failed: {}: {}", failure.kind.name(), failure.detail)
            }
        },
        results => format!("{} results of the run are the test's", results.len()),
    }
}
