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
    /// Transaction encode size limit exceeded.
    #[error("Transaction encode size limit exceeded: tx_size={tx_size} > limit={limit}")]
    TransactionEncodeSizeLimit {
        /// Transaction encode size used by current transaction
        tx_size: u64,
        /// Transaction encode size limit
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

impl MegaTxLimitExceededError {
    /// The amount of the resource used by the current transaction.
    pub fn usage(&self) -> u64 {
        match self {
            Self::TransactionGasLimit { tx_gas_limit, .. } => *tx_gas_limit,
            Self::TransactionEncodeSizeLimit { tx_size, .. } => *tx_size,
            Self::DataAvailabilitySizeLimit { da_size, .. } => *da_size,
        }
    }

    /// The limit of the resource.
    pub fn limit(&self) -> u64 {
        match self {
            Self::TransactionGasLimit { limit, .. } |
            Self::TransactionEncodeSizeLimit { limit, .. } |
            Self::DataAvailabilitySizeLimit { limit, .. } => *limit,
        }
    }
}

impl InvalidTxError for MegaTxLimitExceededError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }
}

/// Error type for block-level limit exceeded. These errors are thrown when the block has already
/// exceeded its limit (from a previous transaction) and no more transactions can be added.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MegaBlockLimitExceededError {
    /// Block transactions data limit reached.
    #[error("Block transactions data limit reached: block_used={block_used} >= limit={limit}")]
    TransactionDataLimit {
        /// Transaction data used by block so far
        block_used: u64,
        /// Block transactions data limit
        limit: u64,
    },

    /// Block KV update limit reached.
    #[error("Block KV update limit reached: block_used={block_used} >= limit={limit}")]
    KVUpdateLimit {
        /// KV updates used by block so far
        block_used: u64,
        /// Block KV update limit
        limit: u64,
    },

    /// Block compute gas limit reached.
    #[error("Block compute gas limit reached: block_used={block_used} >= limit={limit}")]
    ComputeGasLimit {
        /// Compute gas used by block so far
        block_used: u64,
        /// Block compute gas limit
        limit: u64,
    },

    /// Block transactions encode size limit exceeded.
    #[error("Block transactions encode size limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    TransactionEncodeSizeLimit {
        /// Transaction encode size used by block so far
        block_used: u64,
        /// Transaction encode size used by current transaction
        tx_used: u64,
        /// Block transactions encode size limit
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

    /// Block state growth limit reached.
    #[error("Block state growth limit reached: block_used={block_used} >= limit={limit}")]
    StateGrowthLimit {
        /// State growth used by block so far
        block_used: u64,
        /// Block state growth limit
        limit: u64,
    },
}

impl MegaBlockLimitExceededError {
    /// The total amount of the resource used by the block so far.
    pub fn block_used(&self) -> u64 {
        match self {
            Self::TransactionDataLimit { block_used, .. } |
            Self::KVUpdateLimit { block_used, .. } |
            Self::ComputeGasLimit { block_used, .. } |
            Self::TransactionEncodeSizeLimit { block_used, .. } |
            Self::DataAvailabilitySizeLimit { block_used, .. } |
            Self::StateGrowthLimit { block_used, .. } => *block_used,
        }
    }

    /// The limit of the resource.
    pub fn limit(&self) -> u64 {
        match self {
            Self::TransactionDataLimit { limit, .. } |
            Self::KVUpdateLimit { limit, .. } |
            Self::ComputeGasLimit { limit, .. } |
            Self::TransactionEncodeSizeLimit { limit, .. } |
            Self::DataAvailabilitySizeLimit { limit, .. } |
            Self::StateGrowthLimit { limit, .. } => *limit,
        }
    }
}

impl InvalidTxError for MegaBlockLimitExceededError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }
}
