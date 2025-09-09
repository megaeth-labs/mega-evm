use core::fmt::Display;

pub use alloy_evm::InvalidTxError;
pub use op_revm::{OpHaltReason, OpTransactionError};
pub use revm::{
    context::result::{EVMError, InvalidTransaction},
    context_interface::{
        result::HaltReason as EthHaltReason, transaction::TransactionError as TransactionErrorTr,
    },
};

/// `MegaETH` transaction validation error type.
pub type TransactionError = OpTransactionError;

/// `MegaETH` halt reason type, with additional MegaETH-specific halt reasons.
///
/// It is a wrapper around `OpHaltReason`, which internally wraps `EthHaltReason`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From)]
pub enum MegaHaltReason {
    /// Base [`OpHaltReason`]
    Base(#[from] OpHaltReason),
    /// Data limit exceeded
    DataLimitExceeded,
    /// KV update limit exceeded
    KVUpdateLimitExceeded,
}

impl From<EthHaltReason> for MegaHaltReason {
    fn from(value: EthHaltReason) -> Self {
        Self::Base(OpHaltReason::Base(value))
    }
}

impl TryFrom<MegaHaltReason> for EthHaltReason {
    type Error = MegaHaltReason;

    fn try_from(value: MegaHaltReason) -> Result<Self, Self::Error> {
        match value {
            MegaHaltReason::Base(reason) => Ok(reason.try_into()?),
            MegaHaltReason::DataLimitExceeded | MegaHaltReason::KVUpdateLimitExceeded => Err(value),
        }
    }
}
