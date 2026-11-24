use alloy_primitives::{hex::FromHexError, BlockNumber, TxHash, B256};
use alloy_provider::transport::TransportError;
use mega_evm::{
    alloy_evm::block::BlockExecutionError,
    revm::{bytecode::BytecodeDecodeError, database_interface::bal::EvmDatabaseError},
};

/// Stable `Display` prefix of [`EvmeError::RpcTransportError`].
///
/// Pre-block system calls (EIP-2935 / EIP-4788) and similar mega-evm wrappers
/// render a database failure into a message string with `to_string()`, so the
/// typed variant is gone by the time exit classification runs. The classifier
/// recovers the RPC class by stripping recognized outer wrappers and requiring
/// the remainder to start with this exact prefix. The `#[error(...)]` text on
/// the variant must keep the same prefix; the round-trip unit test in `exit`
/// enforces that.
pub const RPC_TRANSPORT_ERROR_PREFIX: &str = "RPC transport error: ";

/// Stable `Display` prefix of [`EvmeError::RpcError`].
///
/// Same recovery contract as [`RPC_TRANSPORT_ERROR_PREFIX`]: fork-state reads
/// map transport/cache failures into this variant, and stringified block errors
/// still start with this prefix (after wrapper strip) so exit classification
/// can treat them as unanswered questions rather than execution failures.
pub const RPC_ERROR_PREFIX: &str = "RPC error: ";

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
    /// A transport error, an offline cache miss, or a served transaction that
    /// fails authentication against the requested hash is the same class as a
    /// null answer: the endpoint failed to deliver a transaction it claimed to
    /// include, rather than answering "unknown hash" about a user query. The
    /// hash is carried so abort output can name the failing fetch.
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

    /// A `replay --verify-receipt` / `--verify-block` run found at least one
    /// replay that did not reproduce the on-chain receipt or the block header.
    ///
    /// Distinct from the infrastructure error variants so a verification
    /// mismatch can be told apart from a target that could not be replayed or
    /// verified at all.
    #[error("{0}")]
    VerificationMismatch(VerificationCounts),

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

/// What a verifying replay compared, and how much of it diverged.
///
/// Receipts and blocks are separate dimensions: a run may verify either or
/// both, and the message names each one that diverged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationCounts {
    /// Verified transactions whose replay did not reproduce the on-chain receipt.
    pub receipts_mismatched: usize,
    /// Transactions compared against their on-chain receipt.
    pub receipts_total: usize,
    /// Verified blocks whose replay did not reproduce the block header.
    pub blocks_mismatched: usize,
    /// Blocks compared against their header.
    pub blocks_total: usize,
}

impl VerificationCounts {
    /// Counts of a run that verified receipts only.
    pub const fn receipts(mismatched: usize, total: usize) -> Self {
        Self {
            receipts_mismatched: mismatched,
            receipts_total: total,
            blocks_mismatched: 0,
            blocks_total: 0,
        }
    }
}

#[cfg(test)]
impl VerificationCounts {
    /// Counts of a run that verified blocks only.
    pub(crate) const fn blocks(mismatched: usize, total: usize) -> Self {
        Self {
            receipts_mismatched: 0,
            receipts_total: 0,
            blocks_mismatched: mismatched,
            blocks_total: total,
        }
    }
}

impl core::fmt::Display for VerificationCounts {
    /// Names every dimension that diverged. A receipt-only finding keeps the
    /// message receipt verification has always printed.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let receipts = (self.receipts_mismatched > 0).then(|| {
            format!(
                "{} of {} verified transaction(s) did not reproduce the on-chain receipt",
                self.receipts_mismatched, self.receipts_total
            )
        });
        let blocks = (self.blocks_mismatched > 0).then(|| {
            format!(
                "{} of {} verified block(s) did not reproduce the block header",
                self.blocks_mismatched, self.blocks_total
            )
        });
        match (receipts, blocks) {
            (Some(receipts), None) => write!(f, "Receipt verification mismatch: {receipts}"),
            (None, Some(blocks)) => write!(f, "Block verification mismatch: {blocks}"),
            (Some(receipts), Some(blocks)) => {
                write!(f, "Verification mismatch: {receipts}; {blocks}")
            }
            // Never constructed: a run only reports a mismatch it counted.
            (None, None) => write!(f, "Verification mismatch: nothing diverged"),
        }
    }
}

/// Exit-code floor contributed by a non-target mid-block abort.
///
/// Per-target failure counters stay strictly about reported targets. When the
/// aborting transaction is not itself a target, this floor still ranks the run
/// exit by the abort's root cause without inflating the "N of M target(s)
/// failed" totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BatchExitFloor {
    /// No non-target abort contributed a floor.
    #[default]
    None,
    /// A non-target abort was execution-class (setup, executor rejection, …).
    Execution,
    /// A non-target abort was rpc-class (transport, cache miss, …).
    Rpc,
}

/// How many targets of a batch replay failed, by failure class.
///
/// A batch run reports every target on its own output line and then fails once
/// with this summary, so the exit-code mapping can apply the batch precedence
/// (execution before RPC before mismatch) without re-reading the per-target
/// lines or parsing an error message.
///
/// [`Self::execution`], [`Self::rpc`], [`Self::mismatched`], and [`Self::total`]
/// count only reported targets, and [`Self::blocks_mismatched`] only verified
/// blocks. [`Self::exit_floor`] is consulted solely for exit ranking when a
/// non-target abort's class is not carried by any target.
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
    /// Blocks whose replay did not reproduce their header (`--verify-block`).
    pub blocks_mismatched: usize,
    /// Blocks whose verdict could not be rendered because the endpoint
    /// contradicted itself about the block (`--verify-block`): a body the header
    /// does not commit to, or a parent that does not link to it. Ranked with the
    /// rpc failures: the question went unanswered.
    pub blocks_unanswered: usize,
    /// Targets the run reported on.
    pub total: usize,
    /// Non-target abort class that floors the run exit without being a target
    /// failure count. Display ignores this for "N of M"; exit mapping uses it.
    pub exit_floor: BatchExitFloor,
}

impl core::fmt::Display for BatchFailureCounts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut parts = Vec::new();
        // Totals stay per-target: a non-target abort floor must not print
        // "3 of 2 target transaction(s) failed". A run whose only failure is a
        // block it could not verify names the block rather than leading with
        // "0 of N target transaction(s) failed".
        if self.execution + self.rpc > 0 || self.blocks_unanswered == 0 {
            parts.push(format!(
                "{} of {} target transaction(s) failed ({} execution, {} rpc)",
                self.execution + self.rpc,
                self.total,
                self.execution,
                self.rpc,
            ));
        }
        if self.blocks_unanswered > 0 {
            parts.push(format!(
                "{} block(s) could not be verified against the block header",
                self.blocks_unanswered,
            ));
        }
        if self.mismatched > 0 {
            parts.push(format!(
                "{} replayed transaction(s) did not reproduce the on-chain receipt",
                self.mismatched,
            ));
        }
        if self.blocks_mismatched > 0 {
            parts.push(format!(
                "{} replayed block(s) did not reproduce the block header",
                self.blocks_mismatched,
            ));
        }
        write!(f, "{}", parts.join("; "))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A receipt-only finding keeps the message receipt verification has always
    /// printed, word for word.
    #[test]
    fn test_verification_counts_receipt_only_message_is_unchanged() {
        assert_eq!(
            EvmeError::VerificationMismatch(VerificationCounts::receipts(1, 3)).to_string(),
            "Receipt verification mismatch: 1 of 3 verified transaction(s) did not reproduce \
             the on-chain receipt"
        );
    }

    /// A block-only finding names the blocks alone, even when the run verified
    /// receipts that all matched.
    #[test]
    fn test_verification_counts_block_only_message() {
        let expected = "Block verification mismatch: 1 of 2 verified block(s) did not \
                        reproduce the block header";
        assert_eq!(VerificationCounts::blocks(1, 2).to_string(), expected);
        let receipts_matched =
            VerificationCounts { receipts_total: 23, ..VerificationCounts::blocks(1, 2) };
        assert_eq!(receipts_matched.to_string(), expected);
    }

    /// When both dimensions diverged, the message names both.
    #[test]
    fn test_verification_counts_names_both_dimensions() {
        let counts = VerificationCounts {
            receipts_mismatched: 10,
            receipts_total: 23,
            blocks_mismatched: 1,
            blocks_total: 1,
        };
        assert_eq!(
            counts.to_string(),
            "Verification mismatch: 10 of 23 verified transaction(s) did not reproduce the \
             on-chain receipt; 1 of 1 verified block(s) did not reproduce the block header"
        );
    }

    /// A block that could not be verified is named after the target counts, or
    /// leads the message when no target failed.
    #[test]
    fn test_batch_failure_counts_name_unverified_blocks() {
        let alone = BatchFailureCounts { blocks_unanswered: 1, total: 23, ..Default::default() };
        assert_eq!(alone.to_string(), "1 block(s) could not be verified against the block header");

        let with_targets =
            BatchFailureCounts { rpc: 23, blocks_unanswered: 1, total: 23, ..Default::default() };
        assert_eq!(
            with_targets.to_string(),
            "23 of 23 target transaction(s) failed (0 execution, 23 rpc); 1 block(s) could not \
             be verified against the block header"
        );
    }

    /// The batch summary names mismatched blocks after the target counts.
    #[test]
    fn test_batch_failure_counts_name_mismatched_blocks() {
        let counts = BatchFailureCounts {
            rpc: 1,
            mismatched: 2,
            blocks_mismatched: 1,
            total: 3,
            ..Default::default()
        };
        assert_eq!(
            counts.to_string(),
            "1 of 3 target transaction(s) failed (0 execution, 1 rpc); 2 replayed \
             transaction(s) did not reproduce the on-chain receipt; 1 replayed block(s) did not \
             reproduce the block header"
        );
    }
}
