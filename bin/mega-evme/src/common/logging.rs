//! Logging configuration for the mega-evme CLI tool.
//!
//! Provides CLI arguments for configuring tracing/logging output with support for:
//! - Verbosity levels via `-v/-vv/-vvv` flags
//! - Custom log filters via `RUST_LOG` environment variable
//! - Log file output via `--log.file` flag
//! - Colorful console output via `--log.color` flag

use std::path::PathBuf;

use clap::Parser;
use tracing::Level;
use tracing_subscriber::{fmt, EnvFilter};

/// Logging configuration arguments.
#[derive(Debug, Clone, Default, Parser)]
pub struct LogArgs {
    /// Increase logging verbosity (-v = error, -vv = warn, -vvv = info, -vvvv = debug, -vvvvv =
    /// trace)
    #[arg(short = 'v', action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Log file path. If specified, logs are written to this file instead of stderr.
    #[arg(long = "log.file", visible_aliases = ["log-file"], global = true)]
    pub log_file: Option<PathBuf>,

    /// Disable colorful console logging. Only applies when logging to stderr (no --log.file).
    #[arg(long = "log.no-color", visible_aliases = ["log-no-color"], global = true)]
    pub log_no_color: bool,
}

impl LogArgs {
    /// Initialize the tracing subscriber based on the logging configuration.
    ///
    /// The log level is determined in the following order of precedence:
    /// 1. `RUST_LOG` environment variable (if set)
    /// 2. `-v` flags (increases from ERROR to WARN/INFO/DEBUG/TRACE)
    /// 3. Default is no logging (OFF)
    ///
    /// Log target is only shown for DEBUG level and above.
    /// If `--log.file` is specified, logs are written to the file instead of stderr.
    pub fn init(&self) {
        let filter = if std::env::var("RUST_LOG").is_ok() {
            // Use RUST_LOG if set
            EnvFilter::from_default_env()
        } else if self.verbose == 0 {
            // No verbosity: no logs
            EnvFilter::new("off")
        } else {
            // Verbosity-based level
            let level = match self.verbose {
                1 => Level::ERROR,
                2 => Level::WARN,
                3 => Level::INFO,
                4 => Level::DEBUG,
                _ => Level::TRACE,
            };
            EnvFilter::new(format!("mega_evme={level},mega_evm={level}"))
        };

        // Show target only for DEBUG level and above (verbose >= 4)
        let show_target = self.verbose >= 4;

        if let Some(ref log_file) = self.log_file {
            // Write logs to file (always without ANSI colors)
            let file = std::fs::File::create(log_file).expect("Failed to create log file");
            fmt()
                .with_env_filter(filter)
                .with_target(show_target)
                .with_writer(file)
                .with_ansi(false)
                .init();
        } else {
            // Write logs to stderr
            fmt()
                .with_env_filter(filter)
                .with_target(show_target)
                .with_writer(std::io::stderr)
                .with_ansi(!self.log_no_color)
                .init();
        }
    }
}
