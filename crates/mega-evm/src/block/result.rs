//! What a block counts of its transactions, and how it refuses one.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::fmt;
use std::vec::Vec;

use alloy_evm::{
    block::{BlockExecutionResult, TxResult},
    InvalidTxError,
};
use alloy_primitives::TxHash;
use revm::context::result::{InvalidTransaction, ResultAndState};

use crate::{BlockUsage, LimitUsage, MegaGasUsage, MegaHaltReason, MegaTransactionOutcome};

/// A block's gas, on the three ledgers its transactions spend on, and the history bytes they
/// appended.
///
/// The block executor fills it transaction by transaction with [`record`](Self::record) and
/// holds each counter to its own block limit, where it has one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockGasCounters {
    /// Regular gas: each transaction's regular ledger, at least its EIP-7623 floor.
    pub execution: u64,
    /// State gas.
    pub state: u64,
    /// History gas.
    pub history: u64,
    /// The history bytes the block's transactions appended, whoever paid for them. At the cost per
    /// history byte they are worth [`history`](Self::history) plus what the history allowances of
    /// value transfers paid, which no gas ledger carries.
    pub history_bytes: u64,
}

impl BlockGasCounters {
    /// Adds one transaction's gas.
    pub fn record(&mut self, gas: &MegaGasUsage) {
        self.execution = self.execution.saturating_add(gas.block_execution_gas());
        self.state = self.state.saturating_add(gas.state);
        self.history = self.history.saturating_add(gas.history);
        self.history_bytes = self.history_bytes.saturating_add(gas.history_bytes);
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
    /// The block's transactions have reached their state gas, and this transaction adds more.
    ///
    /// Only a transaction's own execution tells whether it adds state gas, so this one was
    /// executed before it was refused; a transaction that adds none still fits the block, and a
    /// deposit is never refused this way.
    StateGasLimit {
        /// The state gas the block has spent.
        block_used: u64,
        /// The state gas this transaction would add.
        tx_used: u64,
        /// The block's state-gas limit.
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
            Self::StateGasLimit { block_used, .. } |
            Self::TransactionDataLimit { block_used, .. } |
            Self::TransactionEncodeSizeLimit { block_used, .. } |
            Self::DataAvailabilitySizeLimit { block_used, .. } => *block_used,
        }
    }

    /// The limit of the resource.
    pub const fn limit(&self) -> u64 {
        match self {
            Self::ExecutionGasLimit { limit, .. } |
            Self::StateGasLimit { limit, .. } |
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
            Self::StateGasLimit { block_used, tx_used, limit } => write!(
                f,
                "Block state gas limit reached: block_used={block_used} >= limit={limit}, tx_used={tx_used}"
            ),
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

/// What executing one transaction of a block produced.
///
/// This is the shape [`BlockExecutor::Result`](alloy_evm::block::BlockExecutor) is instantiated
/// with, and an associated type cannot carry the transaction: a transaction enters
/// `execute_transaction_without_commit` as a method-level type parameter, which an associated
/// type — fixed once per implementation — cannot name. So this carries what the commit path
/// needs of it instead: the type byte the receipt builder reads, and the hash, gas limit and
/// sizes the block-level admission re-reads at commit.
///
/// Everything execution itself produced travels as the embedded [`MegaTransactionOutcome`], so a
/// dimension added there flows through without being declared again here.
#[derive(Clone, Debug)]
pub struct MegaBlockTxResult<T> {
    /// The transaction's type, which is what the receipt builder needs of it.
    pub tx_type: T,
    /// The transaction's hash, which names it if block admission refuses it at commit.
    pub tx_hash: TxHash,
    /// The gas the transaction declared.
    pub gas_limit: u64,
    /// The transaction's EIP-2718 encoded size, in bytes.
    pub tx_size: u64,
    /// The transaction's data-availability size, in bytes.
    pub da_size: u64,
    /// The transaction's data-availability footprint, in gas.
    pub da_footprint: u64,
    /// Whether the transaction is a deposit.
    pub is_deposit: bool,
    /// The nonce the depositor had before execution, which the deposit receipt reports. `Some`
    /// only for a deposit.
    pub depositor_nonce: Option<u64>,
    /// The execution result, the post-state and what `MegaETH` counted, as execution produced
    /// them.
    pub inner: MegaTransactionOutcome,
}

impl<T> MegaBlockTxResult<T> {
    /// What this transaction adds to the block's counters.
    pub const fn block_usage(&self) -> BlockUsage {
        BlockUsage {
            gas_used: self.inner.gas.gas_used,
            gas: self.inner.gas,
            usage: self.inner.usage,
            tx_size: self.tx_size,
            da_size: self.da_size,
            da_footprint: self.da_footprint,
            is_deposit: self.is_deposit,
        }
    }
}

impl<T> core::ops::Deref for MegaBlockTxResult<T> {
    type Target = MegaTransactionOutcome;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> core::ops::DerefMut for MegaBlockTxResult<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T: Send + 'static> TxResult for MegaBlockTxResult<T> {
    type HaltReason = MegaHaltReason;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.inner.result_and_state
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.inner.result_and_state
    }
}

/// What executing a whole block produced.
///
/// The receipts, requests and the two header figures are alloy-evm's
/// [`BlockExecutionResult`], which the node consumes as it does for any chain; the block's three
/// gas ledgers and the data-size and write-record counts are `MegaETH`'s own, and no upstream
/// type has a place for them.
#[derive(Clone, Debug)]
pub struct MegaBlockExecutionResult<R> {
    /// The receipts, the requests, the gas used and the blob gas used.
    pub inner: BlockExecutionResult<R>,
    /// The gas the block's transactions spent, by ledger.
    pub gas: BlockGasCounters,
    /// The data-size bytes and write records the block's transactions kept. The KV count a node
    /// reports is [`LimitUsage::write_records`].
    pub usage: LimitUsage,
}

impl<R> MegaBlockExecutionResult<R> {
    /// The receipts of the block's transactions.
    pub fn receipts(&self) -> &[R] {
        &self.inner.receipts
    }

    /// Consumes the result and returns the receipts.
    pub fn into_receipts(self) -> Vec<R> {
        self.inner.receipts
    }
}

impl<R> core::ops::Deref for MegaBlockExecutionResult<R> {
    type Target = BlockExecutionResult<R>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_counters_add_each_ledger() {
        let mut block = BlockGasCounters::default();
        let first = MegaGasUsage {
            regular: 10,
            state: 20,
            history: 30,
            history_bytes: 7,
            floor: 0,
            ..Default::default()
        };
        let floored = MegaGasUsage {
            regular: 5,
            state: 1,
            history: 2,
            history_bytes: 4,
            floor: 50,
            ..Default::default()
        };
        block.record(&first);
        block.record(&floored);
        assert_eq!(
            block,
            BlockGasCounters { execution: 60, state: 21, history: 32, history_bytes: 11 }
        );
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
            (
                MegaBlockLimitExceededError::StateGasLimit { block_used: 6, tx_used: 2, limit: 15 },
                6,
                15,
            ),
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
        let full = BlockGasCounters {
            execution: u64::MAX,
            state: u64::MAX,
            history: u64::MAX,
            history_bytes: u64::MAX,
        };
        let mut block = full;
        block.record(&MegaGasUsage {
            regular: 1,
            state: 1,
            history: 1,
            history_bytes: 1,
            ..Default::default()
        });
        assert_eq!(block, full);
    }
}
