pub use alloy_evm::InvalidTxError;
use alloy_primitives::Address;
pub use op_revm::{OpHaltReason, OpTransactionError};
pub use revm::{
    context::result::{EVMError, InvalidTransaction},
    context_interface::{
        result::HaltReason as EthHaltReason, transaction::TransactionError as TransactionErrorTr,
    },
};
use serde::{Deserialize, Serialize};

/// `MegaETH` transaction validation error type.
pub type MegaTransactionError = OpTransactionError;

/// `MegaETH` halt reason type, with additional MegaETH-specific halt reasons.
///
/// It is a wrapper around `OpHaltReason`, which internally wraps `EthHaltReason`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MegaHaltReason {
    /// Base [`OpHaltReason`]
    Base(OpHaltReason),
    /// Data limit exceeded
    DataLimitExceeded {
        /// The configured data limit
        limit: u64,
        /// The actual data generated
        actual: u64,
    },
    /// KV update limit exceeded
    KVUpdateLimitExceeded {
        /// The configured KV update limit
        limit: u64,
        /// The actual KV update count
        actual: u64,
    },
    /// System transaction's callee is not in the whitelist
    SystemTxInvalidCallee {
        /// address called
        callee: Address,
    },
}

impl From<EthHaltReason> for MegaHaltReason {
    fn from(value: EthHaltReason) -> Self {
        Self::Base(OpHaltReason::Base(value))
    }
}

impl From<OpHaltReason> for MegaHaltReason {
    fn from(value: OpHaltReason) -> Self {
        Self::Base(value)
    }
}

impl TryFrom<MegaHaltReason> for EthHaltReason {
    type Error = MegaHaltReason;

    fn try_from(value: MegaHaltReason) -> Result<Self, Self::Error> {
        match value {
            MegaHaltReason::Base(reason) => Ok(reason.try_into()?),
            MegaHaltReason::DataLimitExceeded { .. } |
            MegaHaltReason::KVUpdateLimitExceeded { .. } |
            MegaHaltReason::SystemTxInvalidCallee { .. } => Err(value),
        }
    }
}
