pub use alloy_evm::InvalidTxError;
use alloy_primitives::Address;
pub use op_revm::{OpHaltReason, OpTransactionError};
use revm::{context::result::ResultAndState, state::EvmState};
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
/// The standard EVM's execution result and post-state, plus the four `MegaETH` resource
/// dimensions. The deref target is the upstream pair, so `outcome.result` and `outcome.state`
/// read straight through; the pair is one field rather than two so every carrier of this type
/// hands it around whole instead of re-plumbing its parts.
#[derive(Debug, Clone, derive_more::Deref, derive_more::DerefMut)]
pub struct MegaTransactionOutcome {
    /// The transaction execution result and post-execution EVM state.
    #[deref]
    #[deref_mut]
    pub result_and_state: ResultAndState<MegaHaltReason>,
    /// The data size usage in bytes.
    pub data_size: u64,
    /// The number of KV updates.
    pub kv_updates: u64,
    /// The compute gas used.
    ///
    /// This is the transaction's full reported total, which under Rex7+ also carries whatever an
    /// exceptionally halted frame destroyed rather than performed. It is the number to report and
    /// to accumulate into block-level compute accounting; it is not the number to compare against
    /// a limit — see [`compute_gas_destroyed`](Self::compute_gas_destroyed).
    ///
    /// These two fields are the uninspected execution's split.
    /// An inspector that rewrites a frame result or edits the interpreter gas counter will make
    /// them diverge from that path: the burn split is settled before `frame_end`, and the
    /// plain-segment delta is read from the interpreter counter.
    pub compute_gas_used: u64,
    /// The part of [`compute_gas_used`](Self::compute_gas_used) that exceptionally halted frames
    /// destroyed rather than performed (Rex7+, always 0 before).
    ///
    /// Destroyed gas is not work the network did, so no resource limit is evaluated against it.
    /// The transaction's own limits already excluded it while executing; a consumer that
    /// accumulates this outcome into a further limit — today the block compute-gas counter — must
    /// subtract it too, and compare `compute_gas_used - compute_gas_destroyed` instead.
    ///
    /// Same inspector caveat as [`compute_gas_used`](Self::compute_gas_used): the field is the
    /// uninspected split, and a rewriting inspector will move it.
    pub compute_gas_destroyed: u64,
    /// The state growth used.
    pub state_growth_used: u64,
}

/// Identifies which stage of block execution produced a state change.
///
/// The state commit hook no longer carries this label, so it is kept for `MegaETH`'s own
/// pre/post-block bookkeeping and for the ordering guarantees documented on the block executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateChangeSource {
    /// Change produced by the transaction at the given index within the block.
    Transaction(usize),
    /// Change produced before any transaction ran.
    PreBlock(StateChangePreBlockSource),
    /// Change produced after every transaction ran.
    PostBlock(StateChangePostBlockSource),
}

/// The pre-block stage that produced a state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateChangePreBlockSource {
    /// EIP-4788 beacon block root contract call.
    BeaconRootContract,
    /// EIP-2935 block hashes contract call.
    BlockHashesContract,
}

/// The post-block stage that produced a state change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateChangePostBlockSource {
    /// Block reward and withdrawal balance increments.
    BalanceIncrements,
}

/// The execution outcome of system call in `MegaETH`.
#[derive(Debug, Clone)]
pub struct MegaSystemCallOutcome {
    /// Source of the state change
    pub source: StateChangeSource,
    /// The post-call evm state
    pub state: EvmState,
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
        /// The configured compute gas limit that was exceeded.
        ///
        /// Relation to `actual` depends on the enforcement model:
        /// - Per-opcode enforcement (through Rex6): the crossing opcode has already recorded its
        ///   cost, so `actual > limit`.
        /// - Gas-clamp enforcement (Rex7+): the crossing opcode is stopped before it executes and
        ///   its cost is not recorded, so the *enforced* usage stays at or below `limit`. `actual`
        ///   is the transaction's full reported total, which also carries the remainders of any
        ///   frame that halted exceptionally earlier in the transaction — those are reported but
        ///   never enforced, and they can push `actual` above `limit`.
        limit: u64,
        /// The actual compute gas usage at the halt.
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
        /// The effective detained compute gas limit that was exceeded.
        /// In REX4+ this is `usage_at_access + cap` (relative); pre-REX4 it equals the raw cap
        /// (absolute).
        ///
        /// Relation to `actual` depends on the enforcement model:
        /// - Per-opcode enforcement (through Rex6): the crossing opcode has already recorded its
        ///   cost, so `actual > limit`.
        /// - Gas-clamp enforcement (Rex7+): the crossing opcode is stopped before it executes and
        ///   its cost is not recorded, so the *enforced* usage stays at or below `limit`. `actual`
        ///   is the transaction's full reported total, which also carries the remainders of any
        ///   frame that halted exceptionally earlier in the transaction — those are reported but
        ///   never enforced, and they can push `actual` above `limit`.
        limit: u64,
        /// The actual compute gas usage at the halt.
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

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::result::OutOfGasError;

    #[test]
    fn test_base_halt_reasons_convert_roundtrip() {
        // `EthHaltReason` is no longer `Copy`, so clone at each use.
        let eth_reason = EthHaltReason::OutOfGas(OutOfGasError::Basic);
        let mega_from_eth = MegaHaltReason::from(eth_reason.clone());
        let mega_from_op = MegaHaltReason::from(OpHaltReason::Base(eth_reason.clone()));

        assert_eq!(mega_from_eth, MegaHaltReason::Base(OpHaltReason::Base(eth_reason.clone())));
        assert_eq!(mega_from_op, MegaHaltReason::Base(OpHaltReason::Base(eth_reason.clone())));
        assert_eq!(EthHaltReason::try_from(mega_from_eth).unwrap(), eth_reason);
        assert_eq!(EthHaltReason::try_from(mega_from_op).unwrap(), eth_reason);
    }

    #[test]
    fn test_mega_specific_halt_reasons_do_not_convert_to_eth() {
        let reason = MegaHaltReason::VolatileDataAccessOutOfGas {
            access_type: VolatileDataAccess::ORACLE,
            limit: 100,
            actual: 101,
        };

        assert_eq!(EthHaltReason::try_from(reason.clone()), Err(reason));
    }

    #[test]
    fn test_op_specific_halt_reason_does_not_convert_to_eth() {
        let op_reason = OpHaltReason::FailedDeposit;
        let mega = MegaHaltReason::from(op_reason.clone());
        assert_eq!(mega, MegaHaltReason::Base(op_reason));
        assert!(EthHaltReason::try_from(mega).is_err());
    }

    #[test]
    fn test_all_mega_specific_variants_fail_eth_conversion() {
        let variants: Vec<MegaHaltReason> = vec![
            MegaHaltReason::DataLimitExceeded { limit: 1, actual: 2 },
            MegaHaltReason::KVUpdateLimitExceeded { limit: 1, actual: 2 },
            MegaHaltReason::ComputeGasLimitExceeded { limit: 1, actual: 2 },
            MegaHaltReason::StateGrowthLimitExceeded { limit: 1, actual: 2 },
            MegaHaltReason::SystemTxInvalidCallee { callee: Address::ZERO },
        ];
        for variant in variants {
            assert!(
                EthHaltReason::try_from(variant).is_err(),
                "mega-specific variant should not convert to EthHaltReason"
            );
        }
    }
}
