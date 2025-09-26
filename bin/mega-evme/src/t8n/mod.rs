//! T8N (state transition) tool implementation
//!
//! This module contains all the functionality for running Ethereum state transitions,
//! including command parsing, I/O operations, utility functions, and type definitions.

/// Command-line interface and main logic for T8N tool
pub mod cmd;
/// Error types and handling for T8N operations
mod error;
/// Input/output operations for loading and saving state data
mod io;
/// Type definitions for T8N data structures
mod types;
/// Utility functions for state calculations and conversions
mod utils;

pub use cmd::*;
pub use error::*;
pub use io::*;
pub use types::*;
pub use utils::*;
