//! What a block counts of its transactions, and how it refuses one.

use core::fmt;

use alloy_evm::InvalidTxError;
use revm::context::result::InvalidTransaction;

use crate::MegaGasUsage;

/// A block's gas, on the three ledgers its transactions spend on.
///
/// The block executor fills it transaction by transaction with [`record`](Self::record) and
/// holds each counter to its own block limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockGasCounters {
    /// Regular gas: each transaction's regular ledger, at least its EIP-7623 floor.
    pub execution: u64,
    /// State gas.
    pub state: u64,
    /// History gas.
    pub history: u64,
}

impl BlockGasCounters {
    /// Adds one transaction's gas.
    pub fn record(&mut self, gas: &MegaGasUsage) {
        self.execution = self.execution.saturating_add(gas.block_execution_gas());
        self.state = self.state.saturating_add(gas.state);
        self.history = self.history.saturating_add(gas.history);
    }
}

/// A transaction that exceeds a limit of its own.
///
/// It can never be included — in this block or any other — so a builder drops it rather than
/// trying it again later. Every variant reports what the transaction
/// [used](Self::usage) and the [limit](Self::limit) it exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MegaTxLimitExceededError {
    /// The transaction declares more gas than one transaction may.
    TransactionGasLimit {
        /// The gas limit the transaction declares.
        tx_gas_limit: u64,
        /// The most gas a transaction may declare.
        limit: u64,
    },
    /// The transaction's encoding is larger than one transaction's may be.
    TransactionEncodeSizeLimit {
        /// The transaction's encoded size, in bytes.
        tx_size: u64,
        /// The most bytes a transaction's encoding may take.
        limit: u64,
    },
    /// The transaction's data-availability size is larger than one transaction's may be.
    DataAvailabilitySizeLimit {
        /// The transaction's data-availability size, in bytes.
        da_size: u64,
        /// The most data-availability bytes a transaction may take.
        limit: u64,
    },
}

impl MegaTxLimitExceededError {
    /// What the transaction used of the resource.
    pub const fn usage(&self) -> u64 {
        match self {
            Self::TransactionGasLimit { tx_gas_limit, .. } => *tx_gas_limit,
            Self::TransactionEncodeSizeLimit { tx_size, .. } => *tx_size,
            Self::DataAvailabilitySizeLimit { da_size, .. } => *da_size,
        }
    }

    /// The limit of the resource.
    pub const fn limit(&self) -> u64 {
        match self {
            Self::TransactionGasLimit { limit, .. } |
            Self::TransactionEncodeSizeLimit { limit, .. } |
            Self::DataAvailabilitySizeLimit { limit, .. } => *limit,
        }
    }
}

impl fmt::Display for MegaTxLimitExceededError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransactionGasLimit { tx_gas_limit, limit } => {
                write!(
                    f,
                    "Transaction gas limit exceeded: tx_gas_limit={tx_gas_limit} > limit={limit}"
                )
            }
            Self::TransactionEncodeSizeLimit { tx_size, limit } => {
                write!(
                    f,
                    "Transaction encode size limit exceeded: tx_size={tx_size} > limit={limit}"
                )
            }
            Self::DataAvailabilitySizeLimit { da_size, limit } => {
                write!(
                    f,
                    "Transaction data availability size limit exceeded: da_size={da_size} > limit={limit}"
                )
            }
        }
    }
}

impl core::error::Error for MegaTxLimitExceededError {}

impl InvalidTxError for MegaTxLimitExceededError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }

    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        // A `MegaETH` resource limit has no upstream `InvalidTransaction` counterpart.
        None
    }
}

/// A block with no room left for one more transaction.
///
/// Unlike [`MegaTxLimitExceededError`] this says nothing about the transaction: a builder tries
/// the next one, and the same transaction may well fit in the next block. Every variant reports
/// what the block [used](Self::block_used) and the [limit](Self::limit) it is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MegaBlockLimitExceededError {
    /// The block's transactions have spent their execution gas.
    ExecutionGasLimit {
        /// The execution gas the block has spent.
        block_used: u64,
        /// The block's execution-gas limit.
        limit: u64,
    },
    /// The block's transactions have kept their data-size bytes.
    TransactionDataLimit {
        /// The data-size bytes the block has kept.
        block_used: u64,
        /// The block's data-size limit.
        limit: u64,
    },
    /// The transaction's body does not fit in what the block has left.
    TransactionEncodeSizeLimit {
        /// The bytes the block's transaction bodies already take.
        block_used: u64,
        /// The bytes this transaction's body would add.
        tx_used: u64,
        /// The block's encoded-size limit.
        limit: u64,
    },
    /// The transaction's data-availability size does not fit in what the block has left.
    DataAvailabilitySizeLimit {
        /// The data-availability bytes the block already takes.
        block_used: u64,
        /// The data-availability bytes this transaction would add.
        tx_used: u64,
        /// The block's data-availability size limit.
        limit: u64,
    },
}

impl MegaBlockLimitExceededError {
    /// What the block used of the resource.
    pub const fn block_used(&self) -> u64 {
        match self {
            Self::ExecutionGasLimit { block_used, .. } |
            Self::TransactionDataLimit { block_used, .. } |
            Self::TransactionEncodeSizeLimit { block_used, .. } |
            Self::DataAvailabilitySizeLimit { block_used, .. } => *block_used,
        }
    }

    /// The limit of the resource.
    pub const fn limit(&self) -> u64 {
        match self {
            Self::ExecutionGasLimit { limit, .. } |
            Self::TransactionDataLimit { limit, .. } |
            Self::TransactionEncodeSizeLimit { limit, .. } |
            Self::DataAvailabilitySizeLimit { limit, .. } => *limit,
        }
    }
}

impl fmt::Display for MegaBlockLimitExceededError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecutionGasLimit { block_used, limit } => {
                write!(f, "Block execution gas limit reached: block_used={block_used} >= limit={limit}")
            }
            Self::TransactionDataLimit { block_used, limit } => {
                write!(f, "Block transactions data limit reached: block_used={block_used} >= limit={limit}")
            }
            Self::TransactionEncodeSizeLimit { block_used, tx_used, limit } => write!(
                f,
                "Block transactions encode size limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}"
            ),
            Self::DataAvailabilitySizeLimit { block_used, tx_used, limit } => write!(
                f,
                "Block data availability size limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}"
            ),
        }
    }
}

impl core::error::Error for MegaBlockLimitExceededError {}

impl InvalidTxError for MegaBlockLimitExceededError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }

    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        // A full block has no upstream `InvalidTransaction` counterpart.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_counters_add_each_ledger() {
        let mut block = BlockGasCounters::default();
        let first =
            MegaGasUsage { regular: 10, state: 20, history: 30, floor: 0, ..Default::default() };
        let floored =
            MegaGasUsage { regular: 5, state: 1, history: 2, floor: 50, ..Default::default() };
        block.record(&first);
        block.record(&floored);
        assert_eq!(block, BlockGasCounters { execution: 60, state: 21, history: 32 });
    }

    #[test]
    fn test_transaction_limit_error_reports_usage_and_limit() {
        let cases = [
            (MegaTxLimitExceededError::TransactionGasLimit { tx_gas_limit: 31, limit: 30 }, 31, 30),
            (
                MegaTxLimitExceededError::TransactionEncodeSizeLimit { tx_size: 101, limit: 100 },
                101,
                100,
            ),
            (
                MegaTxLimitExceededError::DataAvailabilitySizeLimit { da_size: 11, limit: 10 },
                11,
                10,
            ),
        ];

        for (error, expected_usage, expected_limit) in cases {
            assert_eq!(error.usage(), expected_usage);
            assert_eq!(error.limit(), expected_limit);
            assert!(!error.is_nonce_too_low());
            // A resource limit of this engine has no upstream counterpart, so classification
            // downstream reads "drop this transaction", not "abort the payload".
            assert!(error.as_invalid_tx_err().is_none(), "no counterpart for {error}");
        }
    }

    #[test]
    fn test_block_limit_error_reports_block_usage_and_limit() {
        let cases = [
            (MegaBlockLimitExceededError::ExecutionGasLimit { block_used: 3, limit: 12 }, 3, 12),
            (MegaBlockLimitExceededError::TransactionDataLimit { block_used: 1, limit: 10 }, 1, 10),
            (
                MegaBlockLimitExceededError::TransactionEncodeSizeLimit {
                    block_used: 4,
                    tx_used: 1,
                    limit: 13,
                },
                4,
                13,
            ),
            (
                MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                    block_used: 5,
                    tx_used: 1,
                    limit: 14,
                },
                5,
                14,
            ),
        ];

        for (error, expected_block_used, expected_limit) in cases {
            assert_eq!(error.block_used(), expected_block_used);
            assert_eq!(error.limit(), expected_limit);
            assert!(!error.is_nonce_too_low());
            assert!(error.as_invalid_tx_err().is_none(), "no counterpart for {error}");
        }
    }

    #[test]
    fn test_block_counters_saturate() {
        let mut block =
            BlockGasCounters { execution: u64::MAX, state: u64::MAX, history: u64::MAX };
        block.record(&MegaGasUsage { regular: 1, state: 1, history: 1, ..Default::default() });
        assert_eq!(
            block,
            BlockGasCounters { execution: u64::MAX, state: u64::MAX, history: u64::MAX }
        );
    }
}
