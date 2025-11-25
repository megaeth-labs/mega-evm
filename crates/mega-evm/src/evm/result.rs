pub use alloy_evm::InvalidTxError;
use alloy_primitives::Address;
pub use op_revm::{OpHaltReason, OpTransactionError};
use revm::{context::result::ExecutionResult, state::EvmState};
pub use revm::{
    context::result::{EVMError, InvalidTransaction},
    context_interface::{
        result::HaltReason as EthHaltReason, transaction::TransactionError as TransactionErrorTr,
    },
};
use serde::{Deserialize, Serialize};

use crate::VolatileDataAccess;

/// The execution outcome of a transaction in `MegaETH`.
///
/// This struct contains additional information about the transaction execution on top of the
/// standard EVM's execution result and state.
#[derive(Debug, Clone)]
pub struct MegaTransactionOutcome {
    /// The transaction execution result.
    pub result: ExecutionResult<MegaHaltReason>,
    /// The post-execution evm state.
    pub state: EvmState,
    /// The data size usage in bytes.
    pub data_size: u64,
    /// The number of KV updates.
    pub kv_updates: u64,
    /// The compute gas used.
    pub compute_gas_used: u64,
    /// The state growth used.
    pub state_growth_used: u64,
}

/// `MegaETH` transaction validation error type.
///
/// TODO: This is currently a type alias due to constraints from `op_revm::OpHandler`.
/// `OpHandler` requires `ERROR: From<OpTransactionError>`, but we cannot satisfy this
/// for `EVMError<DBError, MegaTransactionError>` due to Rust's orphan rules.
///
/// To add custom transaction error variants, we need to:
/// 1. Stop using `OpHandler` directly
/// 2. Implement all handler methods manually without delegating to `OpHandler`
/// 3. Then we can use a custom enum like: ``` pub enum MegaTransactionError {
///    Base(OpTransactionError), CustomVariant, } ```
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
    /// Compute gas limit exceeded
    ComputeGasLimitExceeded {
        /// The configured compute gas limit
        limit: u64,
        /// The actual compute gas usage
        actual: u64,
    },
    /// State growth limit exceeded
    StateGrowthLimitExceeded {
        /// The configured state growth limit
        limit: u64,
        /// The actual state growth usage
        actual: u64,
    },
    /// System transaction's callee is not in the whitelist
    SystemTxInvalidCallee {
        /// address called
        callee: Address,
    },
    /// Out of gas due to volatile data access limit enforcement.
    /// The transaction exceeded the compute gas limit imposed after accessing volatile data
    /// (block environment, beneficiary, or oracle contract).
    VolatileDataAccessOutOfGas {
        /// Bitflags indicating which volatile data was accessed.
        /// Can check specific accesses using `has_block_env_access()`,
        /// `has_beneficiary_balance_access()`, `has_oracle_access()`
        access_type: VolatileDataAccess,
        /// The compute gas limit that was enforced after volatile data access
        limit: u64,
        /// The actual compute gas usage
        actual: u64,
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
            MegaHaltReason::ComputeGasLimitExceeded { .. } |
            MegaHaltReason::StateGrowthLimitExceeded { .. } |
            MegaHaltReason::SystemTxInvalidCallee { .. } |
            MegaHaltReason::VolatileDataAccessOutOfGas { .. } => Err(value),
        }
    }
}
