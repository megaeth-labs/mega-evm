//! `MegaEVM` CLI tool for Ethereum state transitions and testing
//!
//! This tool provides a command-line interface for executing Ethereum state transitions
//! using the `MegaEVM` implementation.

use clap::Parser;

mod cmd;
mod common;
/// Replay module for fetching and executing transactions from RPC
pub mod replay;
/// Run module for executing arbitrary EVM bytecode
pub mod run;
/// T8N (state transition) module containing all transition-related functionality
pub mod t8n;
/// TX module for executing arbitrary transactions
pub mod tx;

pub use cmd::*;
pub use common::*;

#[tokio::main]
async fn main() -> std::result::Result<(), Error> {
    set_thread_panic_hook();
    MainCmd::parse().run().await.inspect_err(|e| println!("{e:?}"))
}

/// Sets thread panic hook, useful for having tests that panic.
fn set_thread_panic_hook() {
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
