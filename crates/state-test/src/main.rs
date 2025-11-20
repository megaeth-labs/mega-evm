//! State test crate

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

use clap::Parser;
use state_test::runner::{find_all_json_tests, run, TestError, TestErrorKind};
use std::path::PathBuf;

// Dependencies used by the library
use alloy_eips as _;
use alloy_primitives as _;
use alloy_rlp as _;
use alloy_serde as _;
use hash_db as _;
use indicatif as _;
use k256 as _;
use mega_evm as _;
use plain_hasher as _;
use serde as _;
use serde_json as _;
use thiserror as _;
use triehash as _;
use walkdir as _;

/// `statetest` subcommand
#[derive(Parser, Debug)]
pub struct Cmd {
    /// Path to folder or file containing the tests
    ///
    /// If multiple paths are specified they will be run in sequence.
    ///
    /// Folders will be searched recursively for files with the extension `.json`.
    #[arg(required = true, num_args = 1..)]
    paths: Vec<PathBuf>,
    /// Run tests in a single thread
    #[arg(short = 's', long)]
    single_thread: bool,
    /// Output results in JSON format
    ///
    /// It will stop second run of evm on failure.
    #[arg(long)]
    json: bool,
    /// Output outcome in JSON format
    ///
    /// If `--json` is true, this is implied.
    ///
    /// It will stop second run of EVM on failure.
    #[arg(short = 'o', long)]
    json_outcome: bool,
    /// Keep going after a test failure
    #[arg(long, alias = "no-fail-fast")]
    keep_going: bool,
}

impl Cmd {
    /// Runs `statetest` command.
    pub fn run(&self) -> Result<(), TestError> {
        for path in &self.paths {
            if !path.exists() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::InvalidPath,
                });
            }

            println!("\nRunning tests in {}...", path.display());
            let test_files = find_all_json_tests(path);

            if test_files.is_empty() {
                return Err(TestError {
                    name: "Path validation".to_string(),
                    path: path.display().to_string(),
                    kind: TestErrorKind::NoJsonFiles,
                });
            }

            run(test_files, self.single_thread, self.json, self.json_outcome, self.keep_going)?
        }
        Ok(())
    }
}

fn main() {
    let cmd = Cmd::parse();
    cmd.run().unwrap();
}
