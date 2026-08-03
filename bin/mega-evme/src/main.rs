//! `mega-evme` CLI binary.
//!
//! All business logic lives in the `mega_evme` library crate (`src/lib.rs`).
//! This binary is intentionally minimal: parse CLI arguments, install the panic
//! hook, dispatch to the parsed command, and exit.
//!
//! This is the only place a finished command becomes a process status; the
//! taxonomy of statuses lives in `common::exit`.

use std::process::ExitCode;

use clap::Parser;
use mega_evme::{cmd::MainCmd, print_json_error, report_command_result, set_thread_panic_hook};

#[tokio::main]
async fn main() -> ExitCode {
    set_thread_panic_hook();

    let cmd = match MainCmd::try_parse() {
        Ok(cmd) => cmd,
        Err(err) => {
            // `--help` / `--version` are not failures: clap writes them to
            // stdout and the run exits 0. A usage error is bad input, which the
            // taxonomy classifies with the other input errors.
            let _ = err.print();
            if !err.use_stderr() {
                return ExitCode::SUCCESS;
            }
            let code = mega_evme::ExitCode::ExecutionError;
            // The parsed command does not exist yet, so the output mode is read
            // off the raw arguments: a `--json` run must end its stdout with the
            // structured error object even when it never got as far as running.
            if wants_json_output() {
                print_json_error(code, &parse_error_summary(&err));
            }
            return ExitCode::from(code);
        }
    };

    // Read the output mode before `run` consumes the command.
    let json = cmd.json_output();
    let result = cmd.run().await;
    ExitCode::from(report_command_result(result, json))
}

/// Whether the raw arguments ask for machine-readable output.
fn wants_json_output() -> bool {
    std::env::args_os().any(|arg| arg == "--json")
}

/// One-line summary of an argument parsing failure.
///
/// `clap` renders a multi-line report — the message, the usage block, and the
/// help hint — of which only the leading message paragraph describes what went
/// wrong; it is joined into the single line the structured object carries.
fn parse_error_summary(err: &clap::Error) -> String {
    let rendered = err.to_string();
    let summary = rendered
        .lines()
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let summary = summary.strip_prefix("error: ").unwrap_or(&summary).trim();
    if summary.is_empty() {
        "invalid command-line arguments".to_string()
    } else {
        summary.to_string()
    }
}
