use clap::Parser;

/// Main command enumeration for the mega-evme CLI tool
#[derive(Parser, Debug)]
#[command(infer_subcommands = true, version = "0.1")]
#[allow(clippy::large_enum_variant)]
pub enum MainCmd {
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
    /// T8n tool error (wrapped in EvmeError::Other)
    #[error("T8n error: {0}")]
    T8n(#[from] crate::t8n::T8nError),
}

impl MainCmd {
    /// Execute the main command
    pub async fn run(&self) -> Result<(), Error> {
        match self {
            Self::T8n(cmd) => {
                cmd.run().map_err(|e| Error::Evme(crate::common::EvmeError::Other(e.to_string())))?;
                Ok(())
            }
            Self::Run(cmd) => {
                cmd.run().await?;
                Ok(())
            }
            Self::Tx(cmd) => {
                cmd.run().await?;
                Ok(())
            }
            Self::Replay(cmd) => {
                cmd.run().await?;
                Ok(())
            }
        }
    }
}
