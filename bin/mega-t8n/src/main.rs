//! `MegaETH` state transition (t8n) tool

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use clap::Parser;

mod cmd;
mod error;
mod io;
mod types;
mod utils;

use cmd::*;
use error::*;
use io::*;
use types::*;
use utils::*;

fn main() {
    let cmd = Cmd::parse();
    if let Err(e) = cmd.run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
