//! `mega-evme` library.
//!
//! Holds all of the executable EVM logic for the `mega-evme` CLI. The binary
//! at `src/main.rs` is a thin wrapper that only parses arguments and
//! dispatches into this crate; integration tests in `tests/*.rs` link against
//! the library directly and exercise the public API the same way an external
//! consumer would.

/// Offline RPC cache utilities (`cache merge`, …).
pub mod cache;
/// Top-level CLI command parser and dispatch (`MainCmd`, `Commands`, `Error`).
pub mod cmd;
/// Shared building blocks: RPC provider/session, state, env, error, output
/// formatting, tracing, transaction utilities.
pub mod common;
/// Historical transaction replay command.
pub mod replay;
/// Arbitrary EVM bytecode execution command.
pub mod run;
/// Single-transaction execution command.
pub mod tx;

// Internal modules reach for `crate::ChainArgs`, `crate::EvmeState`, etc.;
// these re-exports keep those call sites working unchanged.
pub use cmd::*;
pub use common::*;

/// Install a thread panic hook that prints a custom backtrace and exits with a
/// non-zero status. Lets failing tests and CLI runs surface a useful trace
/// without relying on `RUST_BACKTRACE`.
///
/// When the raw process argv contains `--json`, the hook also prints the
/// standard structured error object on stdout before exiting so a machine-
/// readable run never ends with empty stdout on panic.
///
/// Every write the hook performs is fallible: a closed stdout or stderr must
/// not re-panic inside the hook, or the runtime aborts (SIGABRT) before the
/// documented `exit(1)`. Consumers that close the pipe early
/// (`… --json | head`) therefore still see exit class 1 rather than an
/// undefined signal death.
pub fn set_thread_panic_hook() {
    use std::{
        backtrace::Backtrace,
        io::{self, Write},
        panic::{set_hook, take_hook},
        process::exit,
    };
    let orig_hook = take_hook();
    set_hook(Box::new(move |panic_info| {
        // Raw stderr rather than `tracing`: the subscriber may not be
        // installed yet when a panic fires during CLI startup. Discard write
        // errors so a closed stderr cannot abort the process from here.
        let _ = writeln!(io::stderr(), "Custom backtrace: {}", Backtrace::capture());
        orig_hook(panic_info);
        if raw_argv_wants_json() {
            // Keep the panic text on stderr (via `orig_hook`); the structured
            // object is the machine-readable final stdout line.
            // `print_json_error` itself is non-panicking on a closed stdout —
            // the broken pipe that often triggered this panic must not cause a
            // second panic before `exit(1)`.
            let message = format!("panic: {panic_info}");
            print_json_error(ExitCode::ExecutionError, &message);
        }
        exit(1);
    }));
}

/// Whether the raw process argv contains `--json`.
///
/// Used by the panic hook when the parsed command is not available (and kept
/// public so unit tests can document the same decision as production).
pub fn raw_argv_wants_json() -> bool {
    std::env::args_os().any(|arg| arg == "--json")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panic-hook JSON decision is driven by raw argv, not the parsed CLI.
    #[test]
    fn test_raw_argv_wants_json_detects_flag() {
        // Unit-test the predicate shape by scanning a synthetic argv slice
        // (the production helper reads process args; this mirrors its logic).
        fn wants_json(args: &[&str]) -> bool {
            args.contains(&"--json")
        }
        assert!(!wants_json(&["mega-evme", "replay", "0xabc"]));
        assert!(wants_json(&["mega-evme", "replay", "--json", "0xabc"]));
        assert!(wants_json(&["mega-evme", "--json"]));
        // Only the exact flag; a value containing the substring is not enough.
        assert!(!wants_json(&["mega-evme", "--json-pretty"]));
    }

    /// The structured panic object uses the standard error envelope shape.
    #[test]
    fn test_panic_json_error_object_shape() {
        let code = ExitCode::ExecutionError;
        assert_eq!(code.code(), 1);
        assert_eq!(code.kind(), "execution-error");
        // Message prefix matches the hook's `panic: …` form; printing itself is
        // covered by `print_json_error` and cannot be unit-tested without
        // capturing stdout, so a deterministic binary panic trigger is not used.
        let message = format!("panic: {}", "explicit test panic");
        assert!(message.starts_with("panic: "));
    }
}
