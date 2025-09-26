use clap::Parser;

/// Main command enumeration for the mega-evme CLI tool
#[derive(Parser, Debug)]
#[command(infer_subcommands = true, version = "0.1")]
#[allow(clippy::large_enum_variant)]
pub enum MainCmd {
    /// State transition tool
    T8n(crate::t8n::Cmd),
}

/// Error types for the main command system
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Custom error with static message
    #[error("Custom error: {0}")]
    Custom(&'static str),
    /// T8n tool error
    #[error("T8n error: {0}")]
    T8n(#[from] crate::t8n::T8nError),
}

impl MainCmd {
    /// Execute the main command
    pub fn run(&self) -> Result<(), Error> {
        match self {
            Self::T8n(cmd) => {
                cmd.run()?;
                Ok(())
            }
        }
    }
}
