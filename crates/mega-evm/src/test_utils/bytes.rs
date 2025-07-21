#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

/// Pads the bytes to the right with 0s to make it a multiple of the length.
pub fn right_pad_bytes(bytes: impl AsRef<[u8]>, multiple_of: usize) -> Vec<u8> {
    let bytes = bytes.as_ref().to_vec();
    let padding = (multiple_of - (bytes.len() % multiple_of)) % multiple_of;
    [bytes, vec![0u8; padding]].concat()
}
