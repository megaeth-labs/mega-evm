//! `mega-evme` CLI binary.
//!
//! All business logic lives in the `mega_evme` library crate (`src/lib.rs`).
//! This binary is intentionally minimal: parse CLI arguments, install the panic
//! hook, hand the arguments to the engine the spec names, and exit.

use mega_evme::set_thread_panic_hook;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    set_thread_panic_hook();
    if let Err(error) = mega_evme::cmd::run_cli(std::env::args_os().collect()).await {
        mega_evme::cmd::report_cli_error(&error);
    }
}
