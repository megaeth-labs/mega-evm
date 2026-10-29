//! Runs the execution-spec state tests on the Satin engine.
//!
//! `state-test --fork Osaka <paths>` executes every Osaka entry of the fixtures under `paths` in
//! equivalence mode and exits non-zero when a failure is not explained by a registered deviation;
//! `--mode satin` runs the same entries under Satin's own configuration and only reports. See the
//! `mega-state-test` crate for what the two modes are.
//!
//! `state-test btest <paths>` imports the blockchain tests through Satin's block executor
//! ([`btest`]).

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod btest;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use state_test::{
    deviations,
    runner::{find_json_files, run, Config, DigestForm, Outcome, Report, Summary, TestResult},
    EvmTxRuntimeLimits, Fork, Mode,
};

/// How many unattributed failures, and how many unreproduced entries, the summary lists.
const LISTED_FAILURES: usize = 50;

/// Command-line arguments.
#[derive(Parser, Debug)]
#[command(
    name = "state-test",
    about = "Runs execution-spec state tests on the Satin engine",
    after_help = "`state-test btest --help` describes the blockchain tests' subcommand."
)]
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
    /// Print one JSON line per executed transaction on standard error: the test's identity and the
    /// hash of the canonical record of the transaction's whole outcome (result, state, gas by
    /// ledger, usage, logs, oracle reads), whatever the fixture's verdict.
    #[arg(long)]
    json_digest: bool,
    /// As `--json-digest`, with the canonical record in each line, to see which field differs.
    #[arg(long)]
    json_digest_full: bool,
    /// Print one JSON line per executed transaction on standard error, beside its digest line when
    /// both are asked for: the test's identity and the hash of the canonical record of everything
    /// the transaction read below the journal (the database's accounts, codes, slots and block
    /// hashes, and the SALT buckets and Oracle slots the external environment answered).
    #[arg(long)]
    json_witness: bool,
    /// As `--json-witness`, with the canonical record of the reads in each line.
    #[arg(long)]
    json_witness_full: bool,
    /// Satin mode: hold every transaction to this data-size limit, in bytes, instead of the
    /// spec's; the frame budget follows it unless `--limit-frame-data-size` names one.
    #[arg(long, value_name = "BYTES")]
    limit_data_size: Option<u64>,
    /// Satin mode: cap every frame's data-size budget at this many bytes.
    #[arg(long, value_name = "BYTES")]
    limit_frame_data_size: Option<u64>,
    /// Satin mode: gas detention's two caps, the compute a transaction may still spend once it
    /// read volatile data, instead of the spec's.
    #[arg(long, value_name = "GAS")]
    limit_compute_gas: Option<u64>,
    /// Satin mode: hold every transaction to this many write records, its KV count, instead of
    /// the spec's; every frame's budget is its share of what its parent has left.
    #[arg(long, value_name = "RECORDS")]
    limit_kv_updates: Option<u64>,
    /// Satin mode: hold every transaction to this many new accounts and new slots, its state
    /// growth, instead of the spec's; every frame's budget is its share of what its parent has
    /// left.
    #[arg(long, value_name = "UNITS")]
    limit_state_growth: Option<u64>,
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
    /// Equivalence mode: fail unless every entry a registered deviation lists for the fork fails
    /// exactly as listed, with the hashes listed.
    #[arg(long)]
    expect_deviations: bool,
}

fn main() -> ExitCode {
    // `btest` names the blockchain tests' subcommand; anything else is the state tests' command,
    // whose arguments are unchanged.
    let mut args = std::env::args_os();
    if args.nth(1).is_some_and(|first| first == "btest") {
        return btest::main(std::env::args_os().skip(1));
    }
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
    let limits = match tightened_limits(&cmd) {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let digest = digest_form(cmd.json_digest, cmd.json_digest_full);
    let witness = digest_form(cmd.json_witness, cmd.json_witness_full);
    let config = Config {
        mode: cmd.mode,
        fork: cmd.fork,
        threads,
        json_outcome: cmd.json_outcome,
        trace: cmd.trace,
        digest,
        witness,
        limits,
        deviations: deviations::DEVIATIONS,
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
            let problems =
                report.gate(cmd.expect_executed, cmd.expect_skipped, cmd.expect_deviations);
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

/// The form a compact and a full flag ask for: the full one when both are given.
pub(crate) const fn digest_form(compact: bool, full: bool) -> Option<DigestForm> {
    match (compact, full) {
        (_, true) => Some(DigestForm::Full),
        (true, false) => Some(DigestForm::Compact),
        (false, false) => None,
    }
}

/// The limits the `--limit-*` options name, over the spec's own, or none when no option is given.
fn tightened_limits(cmd: &Cmd) -> Result<Option<EvmTxRuntimeLimits>, String> {
    if cmd.limit_data_size.is_none() &&
        cmd.limit_frame_data_size.is_none() &&
        cmd.limit_compute_gas.is_none() &&
        cmd.limit_kv_updates.is_none() &&
        cmd.limit_state_growth.is_none()
    {
        return Ok(None);
    }
    if cmd.mode == Mode::Equivalence {
        return Err(
            "the --limit-* options need --mode satin: equivalence mode holds no limit".into()
        );
    }
    let mut limits = EvmTxRuntimeLimits::default();
    if let Some(bytes) = cmd.limit_data_size {
        limits = limits.with_tx_data_size_limit(bytes).with_frame_data_size_limit(bytes);
    }
    if let Some(bytes) = cmd.limit_frame_data_size {
        limits = limits.with_frame_data_size_limit(bytes);
    }
    if let Some(gas) = cmd.limit_compute_gas {
        limits = limits
            .with_block_env_access_compute_gas_limit(gas)
            .with_oracle_access_compute_gas_limit(gas);
    }
    if let Some(records) = cmd.limit_kv_updates {
        limits = limits.with_tx_kv_update_limit(records);
    }
    if let Some(units) = cmd.limit_state_growth {
        limits = limits.with_tx_state_growth_limit(units);
    }
    Ok(Some(limits))
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
        for deviation in report.deviations {
            let listed = deviation.listed(report.fork).len();
            if listed > 0 {
                let explained = summary.deviated.get(deviation.id).copied().unwrap_or(0);
                println!("  deviated {:<34} {explained} (listed {listed})", deviation.id);
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
                "    {}\n      {} lists {}, {}; {}",
                entry.entry,
                entry.deviation.id,
                entry.entry.produced.kind().name(),
                entry.entry.produced,
                seen(&entry.seen)
            );
        }
        if unreproduced > LISTED_FAILURES {
            println!("    ... and {} more", unreproduced - LISTED_FAILURES);
        }
    }
    for (path, failure) in &report.file_failures {
        println!("  unreadable {path}\n      {}", failure.detail);
    }
}

/// What a run did with a listed entry, given the results it has for it.
fn seen(results: &[&TestResult]) -> String {
    match results {
        [] => "the run did not execute it".into(),
        [result] => match &result.outcome {
            Outcome::Passed => "it passed".into(),
            Outcome::Skipped { reason } => format!("it was skipped: {}", reason.name()),
            Outcome::Failed(failure) => {
                format!("it failed: {}: {}", failure.kind.name(), failure.detail)
            }
        },
        results => format!("{} results of the run are the entry's", results.len()),
    }
}
