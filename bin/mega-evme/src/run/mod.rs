//! Run module for executing arbitrary EVM bytecode
//!
//! This module provides functionality to execute arbitrary bytecode snippets
//! similar to go-ethereum's `evm run` command.

mod cmd;

pub use cmd::Cmd;

use std::{collections::HashMap, fs, io::Read};

use alloy_primitives::{hex, Address, Bytes, B256, U256};

/// Error types for the run command
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// Failed to read file
    #[error("Failed to read file: {0}")]
    FileRead(#[from] std::io::Error),

    /// Invalid hex string
    #[error("Invalid hex string: {0}")]
    InvalidHex(#[from] hex::FromHexError),

    /// EVM execution error
    #[error("EVM execution error: {0}")]
    ExecutionError(String),

    /// Invalid input
    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

/// Result type for the run command
pub type Result<T> = std::result::Result<T, RunError>;

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

/// Parse bucket capacity string in format "`bucket_id:capacity`"
/// Returns (`bucket_id`, capacity) tuple
pub fn parse_bucket_capacity(s: &str) -> Result<(u32, u64)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(RunError::InvalidInput(format!(
            "Invalid bucket capacity format: '{}'. Expected format: 'bucket_id:capacity'",
            s
        )));
    }

    let bucket_id = parts[0]
        .parse::<u32>()
        .map_err(|e| RunError::InvalidInput(format!("Invalid bucket ID '{}': {}", parts[0], e)))?;

    let capacity = parts[1]
        .parse::<u64>()
        .map_err(|e| RunError::InvalidInput(format!("Invalid capacity '{}': {}", parts[1], e)))?;

    Ok((bucket_id, capacity))
}

/// State dump/prestate format
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StateDump {
    /// Map of address to account state
    #[serde(flatten)]
    pub accounts: HashMap<Address, AccountState>,
}

impl StateDump {
    /// Create a `StateDump` from EVM state
    pub fn from_evm_state(evm_state: &revm::state::EvmState) -> Self {
        let mut accounts = HashMap::new();

        for (address, account) in evm_state {
            let code =
                account.info.code.as_ref().map(|c| c.bytecode().to_vec()).unwrap_or_default();

            let storage =
                account.storage.iter().map(|(slot, value)| (*slot, value.present_value)).collect();

            let account_state = AccountState {
                balance: account.info.balance,
                nonce: account.info.nonce,
                code: code.into(),
                code_hash: account.info.code_hash,
                storage,
            };

            accounts.insert(*address, account_state);
        }

        Self { accounts }
    }
}

/// Account state information
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    /// Account balance
    /// U256 from ruint already uses quantity format (0x-prefixed hex without leading zeros)
    pub balance: U256,
    /// Account nonce (uses `alloy_serde::quantity` for standard Ethereum format)
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// Account code (hex string with 0x prefix)
    pub code: Bytes,
    /// Code hash
    /// B256 already uses hex format with 0x prefix (always 32 bytes)
    pub code_hash: B256,
    /// Storage slots (uses quantity format for keys and values)
    pub storage: HashMap<U256, U256>,
}
