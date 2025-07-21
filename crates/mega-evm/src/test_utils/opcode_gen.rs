//! This module provides utility functions to generate EVM bytecode.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use revm::bytecode::opcode::{MSTORE, PUSH0};

use crate::right_pad_bytes;

/// Generates a PUSH opcode and the bytes to push.
pub fn push_bytes(code: &mut Vec<u8>, bytes: impl AsRef<[u8]>) {
    let bytes = bytes.as_ref();
    assert!(bytes.len() <= 32);
    code.push(PUSH0 + bytes.len() as u8);
    code.extend(bytes.to_vec());
}

/// Generates a PUSH opcode and the bytes to push.
pub fn push_number<T: Into<u128> + Copy>(code: &mut Vec<u8>, number: T) {
    let num = number.into();
    let bytes = match core::mem::size_of::<T>() {
        1 => (num as u8).to_be_bytes().to_vec(),
        2 => (num as u16).to_be_bytes().to_vec(),
        8 => (num as u64).to_be_bytes().to_vec(),
        16 => num.to_be_bytes().to_vec(),
        _ => panic!("Unsupported integer size"),
    };
    push_bytes(code, bytes);
}

/// Generates a MSTORE opcode and the bytes to store.
pub fn store_memory_bytes(code: &mut Vec<u8>, offset: usize, bytes: impl AsRef<[u8]>) {
    let bytes = bytes.as_ref().to_vec();
    let padded_bytes = right_pad_bytes(bytes, 32);
    for (i, chunk) in padded_bytes.chunks(32).enumerate() {
        push_bytes(code, chunk);
        push_number(code, (offset + i * 32) as u64);
        code.push(MSTORE);
    }
}
