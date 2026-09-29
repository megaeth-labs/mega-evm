use std::ffi::OsString;

use clap::{parser::ValueSource, ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use tracing::error;

use crate::{
    common::{EvmeError, LogArgs},
    engine::Engine,
};

/// Main CLI for the mega-evme tool
#[derive(Parser, Debug)]
#[command(name = "mega-evme", infer_subcommands = true, version)]
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
    /// Replay a transaction, or whole blocks, from RPC or a block cache
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
    Evme(#[from] EvmeError),
}

/// Parses `args` (the program name first), picks the engine the command's spec names and runs the
/// command on it.
///
/// A command on a legacy spec is handed, with `args` as they are, to the released 1.7.1 CLI,
/// which parses them again and runs on the legacy engine; everything else runs here, on Satin.
pub async fn run_cli(args: Vec<OsString>) -> Result<(), Error> {
    let matches = MainCmd::command().get_matches_from(&args);
    let spec_is_default = spec_is_default(&matches);
    let cmd = MainCmd::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    // Whole-block replay runs here for both engines, choosing one per block.
    let engine = match &cmd.command {
        Commands::Replay(replay) if replay.replays_blocks() => Ok(Engine::Satin),
        command => command.engine().await,
    };
    let engine = match engine {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    match engine {
        Engine::Legacy => {
            if let Err(e) = crate::engine::run_legacy(args, spec_is_default).await {
                eprintln!("{e}");
                std::process::exit(1);
            }
            Ok(())
        }
        Engine::Satin => cmd.run().await,
    }
}

/// Whether `run`'s or `tx`'s `--spec` was left at its default. Always `false` for `replay`, which
/// has no `--spec`.
fn spec_is_default(matches: &ArgMatches) -> bool {
    match matches.subcommand() {
        Some(("run" | "tx", sub)) => sub.value_source("spec") != Some(ValueSource::CommandLine),
        _ => false,
    }
}

impl Commands {
    /// The engine this command runs on.
    pub async fn engine(&self) -> Result<Engine, EvmeError> {
        match self {
            Self::Run(cmd) => Engine::of_spec(&cmd.env_args.chain.spec),
            Self::Tx(cmd) => Engine::of_spec(&cmd.env_args.chain.spec),
            Self::Replay(cmd) => cmd.engine().await,
        }
    }
}

impl MainCmd {
    /// Execute the command on the Satin engine.
    pub async fn run(self) -> Result<(), Error> {
        // Initialize logging first
        self.log.init();

        match self.command {
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
