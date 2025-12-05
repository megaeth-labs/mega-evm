//! Hex loading utilities for mega-evme

use std::{fs, io::Read};

use alloy_primitives::{hex, Bytes};

use super::{EvmeError, Result};

/// Load hex-encoded bytes from an argument or a file. If the file is a dash (-), read from stdin.
/// Priority: arg > file. Returns `None` if neither is provided.
pub fn load_hex(arg: Option<String>, file: Option<String>) -> Result<Option<Bytes>> {
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
        return Ok(None);
    };

    decode_hex(&hex_string).map(|bytes| Some(Bytes::from(bytes)))
}

/// Decode hex string, handling optional 0x prefix
fn decode_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();

    if s.is_empty() {
        return Ok(Vec::new());
    }

    let hex_str = if s.starts_with("0x") || s.starts_with("0X") { &s[2..] } else { s };

    if hex_str.len() % 2 != 0 {
        return Err(EvmeError::InvalidInput(format!(
            "Invalid hex string length: {} (must be even)",
            hex_str.len()
        )));
    }

    Ok(hex::decode(hex_str)?)
}
