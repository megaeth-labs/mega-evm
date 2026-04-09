//! `mega-evme` CLI binary.
//!
//! All business logic lives in the `mega_evme` library crate (`src/lib.rs`).
//! This binary is intentionally minimal: parse CLI arguments, install the panic
//! hook, dispatch to the parsed command, and exit.

use clap::Parser;
use mega_evme::{
    cmd::{Error, MainCmd},
    set_thread_panic_hook,
};

#[tokio::main]
async fn main() -> std::result::Result<(), Error> {
    set_thread_panic_hook();
    MainCmd::parse().run().await.inspect_err(|e| println!("{e:?}"))
}
