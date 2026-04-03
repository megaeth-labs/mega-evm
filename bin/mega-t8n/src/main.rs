//! `MegaETH` state transition (t8n) tool

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use clap::Parser;

mod cmd;
mod error;
mod io;
mod types;
mod utils;

pub use cmd::*;
pub use error::*;
pub use io::*;
pub use types::*;
pub use utils::*;

// Dependencies used by submodules
use alloy_consensus as _;
use alloy_serde as _;
use mega_evm as _;
use serde as _;
use serde_json as _;
use state_test as _;
use thiserror as _;

fn main() {
    let cmd = Cmd::parse();
    if let Err(e) = cmd.run() {
        eprintln!("{e:?}");
        std::process::exit(1);
    }
}
