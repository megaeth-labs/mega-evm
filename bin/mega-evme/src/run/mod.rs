//! Run module for executing arbitrary EVM bytecode
//!
//! This module provides functionality to execute arbitrary bytecode snippets
//! similar to go-ethereum's `evm run` command.

mod cmd;
mod common;

pub use cmd::*;
pub use common::*;

// Re-export from common module
pub use crate::common::{AccountState, EvmeState};

use std::{fs, io::Read};

use alloy_primitives::hex;

// Re-export EvmeError and Result from common module
pub use crate::common::{EvmeError as RunError, Result};

/// Load code bytes from an argument or a file. If the file is a dash (-), read from stdin.
/// Priority: arg (positional) > file. Returns error if neither is provided.
pub fn load_code(arg: Option<String>, file: Option<String>) -> Result<Vec<u8>> {
    let hex_string = if let Some(arg) = arg {
        arg
    } else if let Some(file) = file {
        if file == "-" {
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            buffer
        } else {
            fs::read_to_string(file)?
        }
    } else {
        return Err(RunError::InvalidInput(
            "No code provided. Use --code, --codefile, or provide code as argument".to_string(),
        ));
    };

    decode_hex(&hex_string)
}

/// Load input bytes from an argument or a file. If the file is a dash (-), read from stdin.
/// Priority: arg > file. Returns empty vec if neither is provided (input is optional).
pub fn load_input(arg: Option<String>, file: Option<String>) -> Result<Vec<u8>> {
    let hex_string = if let Some(arg) = arg {
        arg
    } else if let Some(file) = file {
        if file == "-" {
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            buffer
        } else {
            fs::read_to_string(file)?
        }
    } else {
        // Input is optional, return empty
        return Ok(Vec::new());
    };

    decode_hex(&hex_string)
}

/// Decode hex string, handling optional 0x prefix
fn decode_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();

    if s.is_empty() {
        return Ok(Vec::new());
    }

    // Check for even length
    let hex_str = if s.starts_with("0x") || s.starts_with("0X") { &s[2..] } else { s };

    if hex_str.len() % 2 != 0 {
        return Err(RunError::InvalidInput(format!(
            "Invalid hex string length: {} (must be even)",
            hex_str.len()
        )));
    }

    Ok(hex::decode(hex_str)?)
}
