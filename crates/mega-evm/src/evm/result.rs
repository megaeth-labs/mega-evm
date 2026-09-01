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
    /// These two fields are the uninspected execution's split, and an observation-only inspector
    /// leaves them exactly there. An inspector's edits to interpreter gas counters and to frame
    /// gas limits are measured at the callback boundary and kept out of the split, and an edit to
    /// a returning frame's result is booked at that frame's settlement — all three are reported on
    /// [`inspector_ledger`](Self::inspector_ledger), which is how a consumer tells an execution
    /// the EVM produced alone from one an inspector took part in.
    pub compute_gas_used: u64,
    /// The part of [`compute_gas_used`](Self::compute_gas_used) the transaction destroyed rather
    /// than performed (Rex7+, always 0 before).
    ///
    /// Derived from what the transaction spent, not summed from the sites that destroyed it: gas
    /// the transaction burnt is either work the trackers recorded, `MegaETH` storage gas, or a
    /// budget something threw away without executing anything for it, and this field is the last
    /// of the three read off as the remainder.
    ///
    /// The derivation runs once, where the transaction's gas envelope is final. A transaction that
    /// never reaches that point reports zero: a validation reject, which produces no receipt and
    /// has no envelope to split, and the pre-execution intrinsic overrun, which is a validation
    /// reject on every spec that has this lane — see `MegaHandler::before_execution`, whose
    /// short-circuit books the split for a future spec that could reach it but is not itself a
    /// settlement point.
    ///
    /// This is a reported number and nothing else. Destroyed gas is not work the network did, so
    /// no resource limit is evaluated against it at any level — a consumer that accumulates this
    /// outcome into a further limit reads
    /// [`compute_gas_enforced`](Self::compute_gas_enforced), which comes off the enforcement lane
    /// itself rather than out of this subtraction.
    ///
    /// Same inspector caveat as [`compute_gas_used`](Self::compute_gas_used): the field is the
    /// uninspected split unless [`inspector_ledger`](Self::inspector_ledger) says otherwise.
    pub compute_gas_destroyed: u64,
    /// The part of [`compute_gas_used`](Self::compute_gas_used) every compute-gas limit is
    /// evaluated against: the work the transaction performed, with Rex7+ destroyed remainders left
    /// out (equal to `compute_gas_used` before Rex7, which destroys nothing).
    ///
    /// Read straight off the lane the transaction enforced its own compute limit on, built from
    /// the per-opcode and checkpoint recordings — deliberately *not* reconstructed as
    /// `compute_gas_used - compute_gas_destroyed`. The two are equal, and a cross-check in debug
    /// builds fails loudly if they ever stop being, but they are equal by agreement of two
    /// independent measurements rather than by construction. Every further limit this outcome
    /// feeds — today the block compute-gas counter — is enforcement, and enforcement stays on the
    /// measurement it was always on.
    pub compute_gas_enforced: u64,
    /// The state growth used.
    pub state_growth_used: u64,
    /// What an inspector did to this transaction, measured rather than inferred.
    ///
    /// `MegaETH` wraps every inspector it is handed in a measurement shim. The EVM does not
    /// execute inside an inspector callback, so anything that changes across one is the
    /// inspector's doing by construction, and this is what the shim booked: gas written into a
    /// live interpreter's counter ([`gas`](crate::InspectorLedger::gas)), into the envelope a
    /// frame is about to be built with ([`env`](crate::InspectorLedger::env)), into a
    /// returning frame's result ([`result`](crate::InspectorLedger::result)), how many
    /// rewrites the shim refused
    /// outright ([`rejected_rewrites`](crate::InspectorLedger::rejected_rewrites)), and how many
    /// rewrote what the execution *did* without moving any gas at all
    /// ([`interventions`](crate::InspectorLedger::interventions)).
    ///
    /// # Sign convention
    ///
    /// Every gas lane is signed and reads from the transaction's point of view: **positive is gas
    /// conjured** — gas that exists in the execution but that nothing debited from the
    /// transaction's envelope — and **negative is gas destroyed** — gas the envelope funded that
    /// the execution never got the benefit of. The lanes are net, so an injection and a matching
    /// removal cancel.
    ///
    /// # When it is zero
    ///
    /// [`InspectorLedger::is_zero`](crate::InspectorLedger::is_zero) holds for every transaction
    /// that ran without an inspector and for every observation-only inspector — which is every
    /// tracer. A non-zero ledger means this outcome's gas numbers describe an execution an
    /// inspector took part in, and the block-execution path refuses to admit one into a block for
    /// exactly that reason.
    ///
    /// The converse does not hold, and reading it that way is the mistake this field invites. What
    /// is measured is what the shim can see at a callback boundary: gas that moved, and arguments
    /// that came back changed. An inspector that reaches past those — editing the interpreter's
    /// stack or memory, writing the journal directly, or editing the pending action — leaves this
    /// empty while changing the state the transaction produces.
    ///
    /// # What it is for
    ///
    /// Reporting, and that refusal. No resource limit is ever evaluated against it, and no
    /// adjustment recorded here is ever counted as work: the interpreter-counter lane shifts the
    /// compute measurement's baseline as it books, so the edit settles outside the measured span
    /// and the gas clamp is re-derived on the spot, while the other two lanes move a gas budget
    /// rather than a recording and so never enter the measurement at all. It is also the `I` term
    /// of the conservation law — see [`ConservationTerms`](crate::ConservationTerms) — which is
    /// why an outcome carrying gas numbers is not fully described without it.
    pub inspector_ledger: crate::InspectorLedger,
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
