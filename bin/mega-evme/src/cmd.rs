use clap::{Parser, Subcommand};
use tracing::error;

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
    /// State transition tool
    T8n(crate::t8n::Cmd),
    /// Run arbitrary EVM bytecode
    Run(crate::run::Cmd),
    /// Run arbitrary transaction
    Tx(crate::tx::Cmd),
    /// Replay a transaction from RPC
    Replay(crate::replay::Cmd),
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
    /// T8n tool error (wrapped in `EvmeError::Other`)
    #[error("T8n error: {0}")]
    T8n(#[from] crate::t8n::T8nError),
}

impl MainCmd {
    /// Execute the main command
    pub async fn run(self) -> Result<(), Error> {
        // Initialize logging first
        self.log.init();

        match self.command {
            Commands::T8n(cmd) => {
                cmd.run()
                    .map_err(|e| Error::Evme(crate::common::EvmeError::Other(e.to_string())))?;
                Ok(())
            }
            Commands::Run(cmd) => {
                cmd.run().await?;
                Ok(())
            }
            Commands::Tx(cmd) => {
                cmd.run().await?;
                Ok(())
            }
            Commands::Replay(cmd) => {
                cmd.run().await?;
                Ok(())
            }
        }
        .inspect_err(|e| {
            error!(err = ?e, "Error executing command");
            eprintln!("{e}");
            std::process::exit(1);
        })
    }
}
