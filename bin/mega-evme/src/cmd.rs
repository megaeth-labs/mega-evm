use std::ffi::OsString;

use clap::{parser::ValueSource, ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use tracing::error;

use crate::{
    common::{
        check_genesis_chain, parse_genesis, use_genesis, EvmeError, GenesisChain, LimitsOverride,
        LogArgs,
    },
    engine::Engine,
};

/// Main CLI for the mega-evme tool
#[derive(Parser, Debug)]
#[command(name = "mega-evme", infer_subcommands = true, version)]
pub struct MainCmd {
    /// Logging configuration
    #[command(flatten)]
    pub log: LogArgs,

    /// Satin only: the genesis file of the chain the command runs on. Its `config` object's
    /// `chainId` and Satin keys (`satinTime`, the registry seeds, the protocol limits) replace the
    /// engine's table for that chain: which blocks run Satin, and the schedule they run under. A
    /// run on another chain, on a legacy spec, before the file's `satinTime`, or on a file without
    /// Satin keys is refused
    #[arg(long = "genesis", global = true, value_name = "FILE", value_parser = parse_genesis)]
    pub genesis: Option<GenesisChain>,

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
/// The one argument added is the default spec of a `run` or `tx` that left `--spec` out.
pub async fn run_cli(args: Vec<OsString>) -> Result<(), Error> {
    let matches = MainCmd::command().get_matches_from(&args);
    let spec_is_default = spec_is_default(&matches);
    let cmd = MainCmd::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if let Some(genesis) = &cmd.genesis {
        if let Err(e) = use_genesis(genesis.clone()) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    // The chain is checked before the engine is picked: a run on another chain than the file's is
    // refused for its chain, whatever its spec.
    if let Some(chain_id) = cmd.command.chain_id() {
        if let Err(e) = check_genesis_chain(chain_id) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }

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
        // The released CLI knows no such flag: its engine holds a transaction to its spec's own
        // limits. Refused here rather than left to its parser.
        Engine::Legacy if cmd.command.limits_override().is_some() => {
            eprintln!(
                "{}",
                EvmeError::InvalidInput(
                    "--override.limits applies to Satin only: the legacy engine holds a \
                     transaction to its spec's own limits"
                        .to_string()
                )
            );
            std::process::exit(1);
        }
        Engine::Legacy if cmd.genesis.is_some() => {
            eprintln!(
                "{}",
                EvmeError::InvalidInput(
                    "--genesis applies to Satin only: the legacy engine runs a chain on its own \
                     table"
                        .to_string()
                )
            );
            std::process::exit(1);
        }
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

    /// The chain `run` or `tx` runs on (`--chain-id`); `None` for `replay`, which learns its
    /// chain from the source it replays.
    pub const fn chain_id(&self) -> Option<u64> {
        match self {
            Self::Run(cmd) => Some(cmd.env_args.chain.chain_id),
            Self::Tx(cmd) => Some(cmd.env_args.chain.chain_id),
            Self::Replay(_) => None,
        }
    }

    /// The protocol limits the command overrides the chain's with (`--override.limits`), if any.
    pub const fn limits_override(&self) -> Option<&LimitsOverride> {
        match self {
            Self::Run(cmd) => cmd.env_args.chain.limits_override.as_ref(),
            Self::Tx(cmd) => cmd.env_args.chain.limits_override.as_ref(),
            Self::Replay(cmd) => cmd.limits_override.as_ref(),
        }
    }
}

impl MainCmd {
    /// Execute the command on the Satin engine.
    pub async fn run(self) -> Result<(), Error> {
        // Initialize logging first
        self.log.init();

        // The command's error is bound first and then reported: a `?` inside an arm would return
        // from this function before the report.
        let result = match self.command {
            Commands::Run(cmd) => cmd.run().await,
            Commands::Tx(cmd) => cmd.run().await,
            Commands::Replay(cmd) => cmd.run().await,
        };
        result.map_err(Error::from).inspect_err(|e| {
            error!(err = ?e, "Error executing command");
            eprintln!("{e}");
            std::process::exit(1);
        })
    }
}
