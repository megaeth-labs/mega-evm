use mega_evm::revm::primitives::B256;
use thiserror::Error;

/// Errors that can occur during test setup and execution
#[derive(Debug, Error)]
pub enum TestError {
    /// Unknown private key.
    #[error("unknown private key: {0:?}")]
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
    /// Unexpected exception.
    #[error("unexpected exception: got {got_exception:?}, expected {expected_exception:?}")]
    UnexpectedException {
        /// Expected exception.
        expected_exception: Option<String>,
        /// Got exception.
        got_exception: Option<String>,
    },
}
