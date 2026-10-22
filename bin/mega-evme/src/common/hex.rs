//! Hex loading utilities for mega-evme

use std::{fs, io::Read};

use alloy_primitives::{hex, Bytes};

use super::{EvmeError, Result};

/// Largest hex input or cache file this tool reads whole, in bytes.
pub(crate) const MAX_TEXT_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Load hex-encoded bytes from an argument or a file. If the file is a dash (-), read from stdin.
/// Priority: arg > file. Returns `None` if neither is provided.
pub fn load_hex(arg: Option<String>, file: Option<String>) -> Result<Option<Bytes>> {
    let hex_string = if let Some(arg) = arg {
        arg
    } else if let Some(file) = file {
        if file == "-" {
            read_bounded_text(std::io::stdin(), None)?
        } else {
            let mut file = fs::File::open(&file)?;
            let len = file.metadata()?.len();
            read_bounded_text(&mut file, Some(len))?
        }
    } else {
        return Ok(None);
    };

    if hex_string.len() as u64 > MAX_TEXT_INPUT_BYTES {
        return Err(hex_input_too_large(hex_string.len() as u64));
    }
    decode_hex(&hex_string).map(|bytes| Some(Bytes::from(bytes)))
}

/// Reads `reader` only up to [`MAX_TEXT_INPUT_BYTES`]. `known_len` rejects a file before the
/// read when its size is already over the limit.
fn read_bounded_text(reader: impl Read, known_len: Option<u64>) -> Result<String> {
    if let Some(len) = known_len.filter(|len| *len > MAX_TEXT_INPUT_BYTES) {
        return Err(hex_input_too_large(len));
    }
    let mut text = String::new();
    reader.take(MAX_TEXT_INPUT_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_TEXT_INPUT_BYTES {
        return Err(hex_input_too_large(text.len() as u64));
    }
    Ok(text)
}

fn hex_input_too_large(len: u64) -> EvmeError {
    EvmeError::InvalidInput(format!(
        "hex input is {len} bytes; the limit is {MAX_TEXT_INPUT_BYTES} bytes"
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_hex_reads_a_file_within_the_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.hex");
        fs::write(&path, "0x00").expect("write");
        let bytes =
            load_hex(None, Some(path.to_string_lossy().into())).expect("load").expect("some");
        assert_eq!(bytes.as_ref(), &[0x00]);
    }

    #[test]
    fn test_load_hex_rejects_an_oversized_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.hex");
        fs::File::create(&path)
            .expect("create")
            .set_len(MAX_TEXT_INPUT_BYTES + 1)
            .expect("set_len");
        let err = load_hex(None, Some(path.to_string_lossy().into())).expect_err("over the limit");
        let msg = err.to_string();
        assert!(msg.contains(&MAX_TEXT_INPUT_BYTES.to_string()), "{msg}");
        assert!(msg.contains(&(MAX_TEXT_INPUT_BYTES + 1).to_string()), "{msg}");
    }
}
