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
use mega_evme::{cmd::MainCmd, report_command_result, set_thread_panic_hook};

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
            return if err.use_stderr() {
                ExitCode::from(mega_evme::ExitCode::ExecutionError)
            } else {
                ExitCode::SUCCESS
            };
        }
    };

    // Read the output mode before `run` consumes the command.
    let json = cmd.json_output();
    let result = cmd.run().await;
    ExitCode::from(report_command_result(result, json))
}
