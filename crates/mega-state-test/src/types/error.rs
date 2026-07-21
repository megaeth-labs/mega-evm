use mega_evm::revm::primitives::{B256, U256};
use thiserror::Error;

/// Errors that can occur during test setup and execution
#[derive(Debug, Error)]
pub enum TestError {
    /// Unknown private key.
    ///
    /// The raw key is kept for programmatic use but deliberately redacted from
    /// the `Display` output: fixture `secretKey` values flow through error
    /// messages into logs and CI output.
    #[error("unable to recover sender from fixture secretKey (key redacted)")]
    UnknownPrivateKey(B256),
    /// Invalid transaction type.
    #[error("invalid transaction type")]
    InvalidTransactionType,
    /// A transaction part index points past the end of its array.
    #[error("transaction part index {index} out of bounds for `{part}` (len {len})")]
    PartIndexOutOfBounds {
        /// Name of the indexed transaction part (`gasLimit`, `data`, or `value`).
        part: &'static str,
        /// The requested index.
        index: usize,
        /// The array's length.
        len: usize,
    },
    /// A transaction field's fixture value does not fit the integer width the
    /// EVM uses for it (e.g. a `nonce` beyond `u64::MAX`).
    #[error("transaction `{field}` value {value} exceeds the supported range")]
    ValueOutOfRange {
        /// Name of the out-of-range transaction field.
        field: &'static str,
        /// The fixture value that did not fit.
        value: U256,
    },
    /// Unexpected exception.
    #[error("unexpected exception: got {got_exception:?}, expected {expected_exception:?}")]
    UnexpectedException {
        /// Expected exception.
        expected_exception: Option<String>,
        /// Got exception.
        got_exception: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_private_key_display_redacts_key_material() {
        // The fixture secret key must never appear in the message.
        let key = B256::repeat_byte(0xab);
        let msg = TestError::UnknownPrivateKey(key).to_string();
        assert!(!msg.contains("abab"), "display leaks key material: {msg}");
        assert!(!msg.contains(&format!("{key:?}")), "display leaks key material: {msg}");
        assert!(msg.contains("redacted"), "display should say redacted: {msg}");
    }

    #[test]
    fn value_out_of_range_display_names_field_and_value() {
        let msg = TestError::ValueOutOfRange { field: "nonce", value: U256::MAX }.to_string();
        assert!(msg.contains("nonce"), "missing field name: {msg}");
        assert!(msg.contains(&U256::MAX.to_string()), "missing value: {msg}");
    }
}
