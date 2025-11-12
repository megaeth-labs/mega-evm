use alloy_evm::InvalidTxError;
use revm::state::AccountInfo;

use crate::MegaTransactionOutcome;

/// The execution outcome of a transaction in `MegaETH`.
///
/// This struct contains additional information about the transaction execution on top of the
/// standard EVM's execution result and state.
#[derive(Debug, Clone, derive_more::Deref, derive_more::DerefMut)]
pub struct BlockMegaTransactionOutcome<T> {
    /// The transaction.
    pub tx: T,
    /// The transaction size in bytes.
    pub tx_size: u64,
    /// The transaction data availability size in bytes.
    pub da_size: u64,
    /// The depositor account info.
    pub depositor: Option<AccountInfo>,
    /// The transaction execution outcome.
    #[deref]
    #[deref_mut]
    pub inner: MegaTransactionOutcome,
}

/// Error type for additional reasons of an invalid transaction. If one transaction is invalid, it
/// will never be able to be included in a block and should be discarded.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MegaTxLimitExceededError {
    /// Transaction gas limit exceeded.
    #[error("Transaction gas limit exceeded: tx_gas_limit={tx_gas_limit} > limit={limit}")]
    TransactionGasLimit {
        /// Transaction gas limit used by current transaction
        tx_gas_limit: u64,
        /// Transaction gas limit limit
        limit: u64,
    },
    /// Transaction size limit exceeded.
    #[error("Transaction size limit exceeded: tx_size={tx_size} > limit={limit}")]
    TransactionSizeLimit {
        /// Transaction size used by current transaction
        tx_size: u64,
        /// Transaction size limit
        limit: u64,
    },

    /// Transaction data availability size limit exceeded.
    #[error(
        "Transaction data availability size limit exceeded: da_size={da_size} > limit={limit}"
    )]
    DataAvailabilitySizeLimit {
        /// Data availability size used by current transaction
        da_size: u64,
        /// Data availability size limit
        limit: u64,
    },
}

impl InvalidTxError for MegaTxLimitExceededError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }
}

/// Error type for block-level limit exceeded. These errors are only thrown after the transaction
/// execution but before any changes are committed to the database.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MegaBlockLimitExceededError {
    /// Block data limit exceeded.
    #[error(
        "Block data limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}"
    )]
    DataLimit {
        /// Data used by block so far
        block_used: u64,
        /// Data used by current transaction
        tx_used: u64,
        /// Block data limit
        limit: u64,
    },

    /// Block KV update limit exceeded.
    #[error("Block KV update limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    KVUpdateLimit {
        /// KV updates used by block so far
        block_used: u64,
        /// KV updates used by current transaction
        tx_used: u64,
        /// Block KV update limit
        limit: u64,
    },

    /// Block compute gas limit exceeded.
    #[error("Block compute gas limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    ComputeGasLimit {
        /// Compute gas used by block so far
        block_used: u64,
        /// Compute gas used by current transaction
        tx_used: u64,
        /// Block compute gas limit
        limit: u64,
    },

    /// Transaction size limit exceeded.
    #[error("Transaction size limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    TransactionSizeLimit {
        /// Transaction size used by block so far
        block_used: u64,
        /// Transaction size used by current transaction
        tx_used: u64,
        /// Transaction size limit
        limit: u64,
    },

    /// Block data availability size limit exceeded.
    #[error("Block data availability size limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    DataAvailabilitySizeLimit {
        /// Data availability size used by block so far
        block_used: u64,
        /// Data availability size used by current transaction
        tx_used: u64,
        /// Block data availability size limit
        limit: u64,
    },
}

impl InvalidTxError for MegaBlockLimitExceededError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }
}
