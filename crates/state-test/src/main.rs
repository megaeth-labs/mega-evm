//! Runs the execution-spec state tests on the Satin engine.
//!
//! `state-test --fork Osaka <paths>` executes every Osaka entry of the fixtures under `paths` in
//! equivalence mode and exits non-zero when a failure is not explained by a registered deviation;
//! `--mode satin` runs the same entries under Satin's own configuration and only reports. See the
//! `mega-state-test` crate for what the two modes are.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use state_test::{
    deviations,
    runner::{find_json_files, run, Config, Report, Summary},
    Fork, Mode,
};

/// How many unattributed failures the summary lists.
const LISTED_FAILURES: usize = 50;

/// Command-line arguments.
#[derive(Parser, Debug)]
#[command(name = "state-test", about = "Runs execution-spec state tests on the Satin engine")]
struct Cmd {
    /// Fixture files, or directories searched recursively for `.json` files.
    #[arg(required = true, num_args = 1..)]
    paths: Vec<PathBuf>,
    /// The configuration the tests run under: `equivalence` (a gate) or `satin` (a report).
    #[arg(long, default_value = "equivalence")]
    mode: Mode,
    /// The fork whose fixture entries run: `Osaka` or `Amsterdam`.
    #[arg(long)]
    fork: Fork,
    /// Worker threads (default: one per core).
    #[arg(long)]
    threads: Option<usize>,
    /// Print one JSON line per test to standard error.
    #[arg(long)]
    json_outcome: bool,
    /// Trace every test with an EIP-3155 tracer on standard error; runs on one thread.
    #[arg(long)]
    trace: bool,
    /// Write the summary, as JSON, to this file.
    #[arg(long, value_name = "FILE")]
    summary_json: Option<PathBuf>,
    /// Equivalence mode: fail unless exactly this many tests execute.
    #[arg(long, value_name = "N")]
    expect_executed: Option<usize>,
    /// Equivalence mode: fail unless exactly this many tests are skipped.
    #[arg(long, value_name = "N")]
    expect_skipped: Option<usize>,
    /// Equivalence mode: fail unless every registered deviation explains exactly as many failed
    /// tests as it pins for the fork.
    #[arg(long)]
    expect_deviations: bool,
}

fn main() -> ExitCode {
    let cmd = Cmd::parse();
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

    let threads = if cmd.trace {
        1
    } else {
        cmd.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
    };
    let config = Config {
        mode: cmd.mode,
        fork: cmd.fork,
        threads,
        json_outcome: cmd.json_outcome,
        trace: cmd.trace,
    };
    let report = run(&files, config);
    let summary = report.summary();
    print_summary(&report, &summary);

    if let Some(path) = &cmd.summary_json {
        let json =
            serde_json::json!({ "mode": report.mode, "fork": report.fork, "summary": summary });
        let json = serde_json::to_string_pretty(&json).expect("a summary serializes");
        if let Err(error) = std::fs::write(path, json + "\n") {
            eprintln!("error: writing {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    }

    match cmd.mode {
        Mode::Equivalence => {
            let problems = summary.gate(
                cmd.fork,
                cmd.expect_executed,
                cmd.expect_skipped,
                cmd.expect_deviations,
            );
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
        // A report: it fails only when a fixture could not be run at all, which is the runner's
        // failure rather than Satin's.
        Mode::Satin if summary.file_failures > 0 => ExitCode::FAILURE,
        Mode::Satin => ExitCode::SUCCESS,
    }
}

fn print_summary(report: &Report, summary: &Summary) {
    println!("{} mode, {} fixtures: {} files", report.mode, report.fork, summary.files);
    println!(
        "  defined {}  executed {}  passed {}  failed {}  skipped {}",
        summary.defined,
        summary.executed,
        summary.passed,
        summary.failed_total(),
        summary.skipped_total()
    );
    for (reason, count) in &summary.skipped {
        println!("  skipped  {:<34} {count}", reason.name());
    }
    for (kind, count) in &summary.failed {
        println!("  failed   {:<34} {count}", kind.name());
    }
    if report.mode == Mode::Equivalence {
        for (id, count) in &summary.deviated {
            let pinned = deviations::by_id(id).map_or(0, |d| d.pinned(report.fork));
            println!("  deviated {id:<34} {count} (pinned {pinned})");
        }
        println!("  unattributed {}", summary.unattributed);
        for (id, failure) in report.unattributed().take(LISTED_FAILURES) {
            println!("    {id}\n      {}: {}", failure.kind.name(), failure.detail);
        }
        if summary.unattributed > LISTED_FAILURES {
            println!("    ... and {} more", summary.unattributed - LISTED_FAILURES);
        }
    }
    for (path, failure) in &report.file_failures {
        println!("  unreadable {path}\n      {}", failure.detail);
    }
}
