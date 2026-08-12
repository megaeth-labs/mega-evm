use alloy_primitives::{hex::FromHexError, BlockNumber, TxHash, B256};
use alloy_provider::transport::TransportError;
use mega_evm::{
    alloy_evm::block::BlockExecutionError,
    revm::{bytecode::BytecodeDecodeError, database_interface::bal::EvmDatabaseError},
};

/// Error types for the replay command
#[derive(Debug, thiserror::Error)]
pub enum EvmeError {
    /// RPC transport error
    #[error("RPC transport error: {0}")]
    RpcTransportError(TransportError),

    /// Transaction not found
    #[error("Transaction not found: {0}")]
    TransactionNotFound(TxHash),

    /// The block body listed this hash, but `eth_getTransactionByHash` returned null.
    ///
    /// That answer contradicts data the endpoint already served (the block body),
    /// so the endpoint is inconsistent — typically a reorg or load-balanced
    /// divergent views — rather than a definitive "unknown transaction".
    #[error("Block body lists transaction {0} but the endpoint resolves it to null")]
    BlockBodyTransactionNull(TxHash),

    /// The block body listed this hash, but fetching the transaction failed.
    ///
    /// A transport error or offline cache miss on a body-listed hash is the same
    /// class as a null answer: the endpoint failed to deliver a transaction it
    /// claimed to include, rather than answering "unknown hash" about a user
    /// query. The hash is carried so abort output can name the failing fetch.
    #[error("Block body lists transaction {tx_hash} but fetching it failed: {message}")]
    BlockBodyTransactionFetch {
        /// Hash the block body listed and the lookup failed for.
        tx_hash: TxHash,
        /// Transport or cache-miss detail from the failed lookup.
        message: String,
    },

    /// Block not found
    #[error("Block not found: {0}")]
    BlockNotFound(BlockNumber),

    /// Block execution error
    #[error("Block execution error: {0}")]
    BlockExecutionError(#[from] BlockExecutionError),

    /// Invalid bytecode
    #[error("Invalid bytecode: {0}")]
    InvalidBytecode(#[from] BytecodeDecodeError),

    /// Failed to read file
    #[error("Failed to read file: {0}")]
    FileRead(#[from] std::io::Error),

    /// Invalid hex string
    #[error("Invalid hex string: {0}")]
    InvalidHex(#[from] FromHexError),

    /// EVM execution error
    #[error("EVM execution error: {0}")]
    ExecutionError(String),

    /// Invalid input
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// RPC error
    #[error("RPC error: {0}")]
    RpcError(String),

    /// Fixture envelope error
    #[error("Fixture error: {0}")]
    FixtureError(String),

    /// Unsupported transaction type
    #[error("Unsupported transaction type: {0}")]
    UnsupportedTxType(u8),

    /// A `replay --verify-receipt` run found at least one local replay that did
    /// not reproduce the on-chain receipt.
    ///
    /// Distinct from the infrastructure error variants so a verification
    /// mismatch can be told apart from a target that could not be replayed or
    /// verified at all.
    #[error(
        "Receipt verification mismatch: {mismatched} of {total} verified transaction(s) did \
         not reproduce the on-chain receipt"
    )]
    VerificationMismatch {
        /// Number of verified transactions whose replay diverged.
        mismatched: usize,
        /// Number of transactions that were verified.
        total: usize,
    },

    /// A batch replay in which at least one target did not come out clean.
    ///
    /// Carries the counts by failure class so the exit-code mapping resolves the
    /// batch precedence from data instead of parsing this message.
    #[error("{0}")]
    BatchFailed(BatchFailureCounts),

    /// Code hash mismatch
    #[error("Code hash mismatch: expected {expected}, computed {computed}")]
    CodeHashMismatch {
        /// Expected code hash from prestate
        expected: B256,
        /// Computed code hash from bytecode
        computed: B256,
    },

    /// Other error
    #[error("Other error: {0}")]
    Other(String),
}

/// How many targets of a batch replay failed, by failure class.
///
/// A batch run reports every target on its own output line and then fails once
/// with this summary, so the exit-code mapping can apply the batch precedence
/// (execution before RPC before mismatch) without re-reading the per-target
/// lines or parsing an error message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchFailureCounts {
    /// Targets that failed for an execution, setup, or definitive-answer reason
    /// (unknown or pending transaction, block executor rejection, a fixture the
    /// run was asked to write and could not).
    pub execution: usize,
    /// Targets whose question went unanswered because an RPC call failed.
    pub rpc: usize,
    /// Targets that replayed but did not reproduce their on-chain receipt.
    pub mismatched: usize,
    /// Targets the run reported on.
    pub total: usize,
}

impl core::fmt::Display for BatchFailureCounts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} of {} target transaction(s) failed ({} execution, {} rpc)",
            self.execution + self.rpc,
            self.total,
            self.execution,
            self.rpc,
        )?;
        if self.mismatched > 0 {
            write!(
                f,
                "; {} replayed transaction(s) did not reproduce the on-chain receipt",
                self.mismatched,
            )?;
        }
        Ok(())
    }
}

// Implement DBErrorMarker to allow EvmeError to be used as Database error type
impl mega_evm::revm::database::DBErrorMarker for EvmeError {}

impl From<EvmDatabaseError<Self>> for EvmeError {
    fn from(err: EvmDatabaseError<Self>) -> Self {
        match err {
            EvmDatabaseError::Database(e) => e,
            EvmDatabaseError::Bal(e) => Self::Other(format!("BAL error: {e}")),
        }
    }
}

/// Result type for the mega-evme command
pub type Result<T> = std::result::Result<T, EvmeError>;
