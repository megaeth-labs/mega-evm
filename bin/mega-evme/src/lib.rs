//! `mega-evme` library.
//!
//! Holds all of the executable EVM logic for the `mega-evme` CLI. The binary
//! at `src/main.rs` is a thin wrapper that only parses arguments and
//! dispatches into this crate; integration tests in `tests/*.rs` link against
//! the library directly and exercise the public API the same way an external
//! consumer would.

/// Top-level CLI command parser and dispatch (`MainCmd`, `Commands`, `Error`).
pub mod cmd;
/// Shared building blocks: RPC provider/session, state, env, error, output
/// formatting, tracing, transaction utilities.
pub mod common;
/// Custom hasher used by the env-derived bucket capacity tables.
pub mod hasher;
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
pub fn set_thread_panic_hook() {
    use std::{
        backtrace::Backtrace,
        panic::{set_hook, take_hook},
        process::exit,
    };
    let orig_hook = take_hook();
    set_hook(Box::new(move |panic_info| {
        println!("Custom backtrace: {}", Backtrace::capture());
        orig_hook(panic_info);
        exit(1);
    }));
}
