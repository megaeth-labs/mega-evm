#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

/// Pads the bytes to the right with 0s to make it a multiple of the length.
///
/// A multiple of zero is not a width, so the bytes are returned unchanged.
pub fn right_pad_bytes(bytes: impl AsRef<[u8]>, multiple_of: usize) -> Vec<u8> {
    let bytes = bytes.as_ref().to_vec();
    if multiple_of == 0 {
        return bytes;
    }
    let padding = (multiple_of - (bytes.len() % multiple_of)) % multiple_of;
    [bytes, vec![0u8; padding]].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_right_pad_bytes_leaves_a_zero_multiple_unchanged() {
        assert_eq!(right_pad_bytes([1, 2, 3], 0), vec![1, 2, 3]);
        assert_eq!(right_pad_bytes([1u8; 0], 0), Vec::<u8>::new());
    }

    #[test]
    fn test_right_pad_bytes_pads_up_to_the_multiple() {
        assert_eq!(right_pad_bytes([1, 2], 4), vec![1, 2, 0, 0]);
        assert_eq!(right_pad_bytes([1, 2, 3, 4], 4), vec![1, 2, 3, 4]);
    }
}
