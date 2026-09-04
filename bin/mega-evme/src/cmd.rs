use clap::{Parser, Subcommand};

use crate::common::LogArgs;

/// Main CLI for the mega-evme tool
#[derive(Parser, Debug)]
#[command(name = "mega-evme", infer_subcommands = true, version = "0.1")]
pub struct MainCmd {
    /// Logging configuration
    #[command(flatten)]
    pub log: LogArgs,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Commands,
}

/// Available subcommands
#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// Run arbitrary EVM bytecode
    Run(crate::run::Cmd),
    /// Run arbitrary transaction
    Tx(crate::tx::Cmd),
    /// Replay a transaction from RPC
    Replay(crate::replay::Cmd),
    /// Offline RPC cache utilities (`cache merge`, …)
    Cache(crate::cache::Cmd),
}

/// Error types for the main command system
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Custom error with static message
    #[error("Custom error: {0}")]
    Custom(&'static str),
    /// Evme error (used by run, tx, and replay commands)
    #[error("{0}")]
    Evme(#[from] crate::common::EvmeError),
}

impl MainCmd {
    /// Execute the main command.
    ///
    /// Failures are returned, never reported here: the binary hands the result
    /// to [`crate::common::report_command_result`], which owns the single
    /// failure report and the process exit code.
    pub async fn run(self) -> Result<(), Error> {
        // Initialize logging first
        self.log.init();

        match self.command {
            Commands::Run(cmd) => cmd.run().await.map_err(Error::from),
            Commands::Tx(cmd) => cmd.run().await.map_err(Error::from),
            Commands::Replay(cmd) => cmd.run().await.map_err(Error::from),
            Commands::Cache(cmd) => cmd.run().map_err(Error::from),
        }
    }

    /// Whether the selected subcommand was asked for machine-readable output.
    ///
    /// Read before [`Self::run`] consumes the command, so a failure is reported
    /// in the output mode the user asked for. `cache` has no `--json` mode.
    pub const fn json_output(&self) -> bool {
        match &self.command {
            Commands::Run(cmd) => cmd.output_args.json,
            Commands::Tx(cmd) => cmd.output_args.json,
            Commands::Replay(cmd) => cmd.output_args.json,
            Commands::Cache(_) => false,
        }
    }
}
