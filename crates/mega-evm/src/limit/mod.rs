use core::ops::Range;

use alloy_primitives::{Address, Bytes, U256};
use op_revm::OpHaltReason;
use revm::{
    context::result::{HaltReason, OutOfGasError},
    handler::{EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallOutcome, CreateOutcome,
        FrameInput, Gas, InstructionResult, InterpreterResult, SStoreResult,
    },
};

use crate::{EvmTxRuntimeLimits, JournalInspectTr, MegaHaltReason, MegaSpecId, MegaTransaction};

mod compute_gas;
mod data_size;
mod kv_update;
mod state_growth;

pub use compute_gas::*;
pub use data_size::*;
pub use kv_update::*;
pub use state_growth::*;

/// Additional limits for the `MegaETH` EVM beyond standard EVM limits.
///
/// This struct coordinates three independent resource limits: compute gas, data size, and
/// key-value updates. Each limit is tracked separately and enforced during transaction execution.
/// When a limit is exceeded, the transaction halts with `OutOfGas` and remaining gas is preserved
/// (not consumed):
/// - **Compute gas limit**: Transaction halts with `OutOfGas`, remaining gas is preserved
/// - **Data size limit**: Transaction halts with `OutOfGas`, remaining gas is preserved
/// - **KV update limit**: Transaction halts with `OutOfGas`, remaining gas is preserved
///
/// # Tracking Details
///
/// - **Compute Gas**: Tracks gas consumption from EVM instructions during execution, monitoring the
///   computational cost separate from the standard gas limit
/// - **Data Size**: Tracks transaction data (110 bytes base + calldata + access lists +
///   authorizations), caller/authority account updates (40 bytes each), log data, storage writes
///   (40 bytes when original ≠ new), account updates from calls/creates (40 bytes), and contract
///   code size
/// - **KV Updates**: Tracks transaction caller + authority updates, storage writes (when original ≠
///   new), and account updates from value transfers and creates
///
/// # Default Limits (`MINI_REX`)
///
/// - Compute gas limit: 30,000,000 gas
/// - Data size limit: 3.125 MB (25% of 12.5 MB block limit)
/// - KV update limit: 1,000 operations
#[derive(Debug)]
pub struct AdditionalLimit {
    /// A flag to indicate if the limit has been exceeded, set when the limit is exceeded. Once
    /// set, it will not be changed until `reset` is called. The current size and count in
    /// neither `kv_update_counter` nor `data_size_tracker` is reliable since when the limit is
    /// exceeded, the frames will be reverted and the data size and count will be discarded.
    pub has_exceeded_limit: AdditionalLimitResult,

    /// The total remaining gas after the limit exceeds.
    pub rescued_gas: u64,

    /// The original limits set by the EVM. Some of the limits may be overridden (such as the
    /// compute gas limit) during transaction execution. We keep the original limits to be able to
    /// reset the limits before each transaction.
    pub limits: EvmTxRuntimeLimits,

    /// The data limit for the EVM. When the data limit is reached, the transaction will error and
    /// halt (remaining gas is preserved).
    ///
    /// This limit controls the maximum total data size that can be generated during
    /// transaction execution, including transaction data, logs, storage operations, etc.
    pub data_limit: u64,

    /// The key-value update limit for the EVM. When the key-value update limit is reached, the
    /// transaction will error and halt (remaining gas is preserved).
    ///
    /// This limit controls the maximum number of key-value storage operations that can be
    /// performed during transaction execution, including storage reads, writes, and account
    /// updates.
    pub kv_update_limit: u64,

    /// The compute gas limit for the EVM. When the compute gas limit is reached, the transaction
    /// will error and halt (remaining gas will be refunded).
    ///
    /// This limit controls the maximum total compute gas that can be consumed during transaction
    /// execution.
    pub compute_gas_limit: u64,

    /// The state growth limit for the EVM. When the state growth limit is reached, the transaction
    /// will error and halt (remaining gas will be refunded).
    ///
    /// This limit controls the maximum total state growth that can be performed during transaction
    /// execution, including new accounts and storage slots.
    pub state_growth_limit: u64,

    /// A tracker for the total compute gas consumed during transaction execution.
    ///
    /// This tracker monitors all compute gas consumed during execution.
    pub compute_gas_tracker: compute_gas::ComputeGasTracker,

    /// A tracker for the total data size (in bytes) generated from a transaction execution.
    ///
    /// This tracker monitors all data generated during execution, including transaction data,
    /// logs, storage operations, and account updates.
    pub data_size_tracker: data_size::DataSizeTracker,
    /// A counter for the number of key-value updates performed during transaction execution.
    ///
    /// This counter tracks storage operations and account updates, providing frame-aware
    /// counting that properly handles nested calls and reverts.
    pub kv_update_counter: kv_update::KVUpdateCounter,
    /// A tracker for the total state growth during transaction execution.
    ///
    /// This tracker monitors all state growth during execution, including new accounts and storage
    /// slots.
    pub state_growth_tracker: state_growth::StateGrowthTracker,
}

/// The usage of the additional limits.
#[derive(Clone, Copy, Debug, Default)]
pub struct LimitUsage {
    /// The data size usage in bytes.
    pub data_size: u64,
    /// The number of KV updates.
    pub kv_updates: u64,
    /// The compute gas usage.
    pub compute_gas: u64,
    /// The state growth.
    pub state_growth: u64,
}

impl AdditionalLimit {
    /// Creates a new `AdditionalLimit` instance from the given `MegaSpecId`.
    pub fn new(limits: EvmTxRuntimeLimits) -> Self {
        Self {
            has_exceeded_limit: AdditionalLimitResult::WithinLimit,
            rescued_gas: 0,
            limits,
            data_limit: limits.tx_data_size_limit,
            kv_update_limit: limits.tx_kv_updates_limit,
            compute_gas_limit: limits.tx_compute_gas_limit,
            state_growth_limit: limits.tx_state_growth_limit,
            compute_gas_tracker: compute_gas::ComputeGasTracker::new(),
            data_size_tracker: data_size::DataSizeTracker::new(),
            kv_update_counter: kv_update::KVUpdateCounter::new(),
            state_growth_tracker: state_growth::StateGrowthTracker::new(),
        }
    }

    /// Resets to the original [`EvmTxRuntimeLimits`].
    pub fn reset_limits(&mut self) {
        self.data_limit = self.limits.tx_data_size_limit;
        self.compute_gas_limit = self.limits.tx_compute_gas_limit;
        self.kv_update_limit = self.limits.tx_kv_updates_limit;
        self.state_growth_limit = self.limits.tx_state_growth_limit;
    }
}

impl AdditionalLimit {
    /// The [`InstructionResult`] to indicate that the limit is exceeded.
    ///
    /// This constant is used to signal that either the data limit or KV update limit
    /// has been exceeded during transaction execution.
    pub const EXCEEDING_LIMIT_INSTRUCTION_RESULT: InstructionResult = InstructionResult::OutOfGas;

    /// Resets the internal state for a new transaction or block.
    ///
    /// This method clears both the data size tracker and KV update counter,
    /// preparing the limit system for a new execution context.
    ///
    /// The `spec` parameter is used to determine if the limits should be reset.
    /// Starting from [`MegaSpecId::REX1`], the limits are reset to their original values
    /// between transactions. This is to ensure that the limits are not carried over
    /// from the previous transaction (e.g., when volatile data is accessed and the
    /// compute gas limit is lowered).
    pub fn reset(&mut self, spec: MegaSpecId) {
        println!("resetting limits: {:?}", spec);
        self.has_exceeded_limit = AdditionalLimitResult::WithinLimit;
        self.rescued_gas = 0;
        if spec.is_enabled(MegaSpecId::REX1) {
            self.reset_limits();
        }
        self.data_size_tracker.reset();
        self.kv_update_counter.reset();
        self.compute_gas_tracker.reset();
        self.state_growth_tracker.reset();
    }

    /// Gets the usage of the additional limits.
    #[inline]
    pub const fn get_usage(&self) -> LimitUsage {
        LimitUsage {
            data_size: self.data_size_tracker.current_size(),
            kv_updates: self.kv_update_counter.current_count(),
            compute_gas: self.compute_gas_tracker.current_gas_used(),
            state_growth: self.state_growth_tracker.current_growth(),
        }
    }

    /// Sets the compute gas limit to a new value.
    /// This is used to dynamically lower the compute gas limit when volatile data is accessed.
    /// The new limit must be lower than the current limit.
    #[inline]
    pub fn set_compute_gas_limit(&mut self, new_limit: u64) {
        self.compute_gas_limit = self.compute_gas_limit.min(new_limit);
    }

    /// Checks if any of the configured limits have been exceeded.
    ///
    /// This method examines both the data size and KV update limits to determine
    /// if the current usage exceeds the configured thresholds.
    ///
    /// # Returns
    ///
    /// Returns an [`AdditionalLimitResult`] indicating whether limits have been exceeded
    /// and which specific limit was exceeded if any.
    #[inline]
    pub const fn check_limit(&mut self) -> AdditionalLimitResult {
        // short circuit if the limit has already been exceeded
        if self.has_exceeded_limit.exceeded_limit() {
            return self.has_exceeded_limit;
        }

        if self.data_size_tracker.exceeds_limit(self.data_limit) {
            self.has_exceeded_limit = AdditionalLimitResult::ExceedsDataLimit {
                limit: self.data_limit,
                used: self.data_size_tracker.current_size(),
            }
        } else if self.kv_update_counter.exceeds_limit(self.kv_update_limit) {
            self.has_exceeded_limit = AdditionalLimitResult::ExceedsKVUpdateLimit {
                limit: self.kv_update_limit,
                used: self.kv_update_counter.current_count(),
            }
        } else if self.compute_gas_tracker.exceeds_limit(self.compute_gas_limit) {
            self.has_exceeded_limit = AdditionalLimitResult::ExceedsComputeGasLimit {
                limit: self.compute_gas_limit,
                used: self.compute_gas_tracker.current_gas_used(),
            }
        } else if self.state_growth_tracker.exceeds_limit(self.state_growth_limit) {
            self.has_exceeded_limit = AdditionalLimitResult::ExceedsStateGrowthLimit {
                limit: self.state_growth_limit,
                used: self.state_growth_tracker.current_growth(),
            }
        }
        self.has_exceeded_limit
    }

    /// Determines if a frame result indicates that limits have been exceeded.
    ///
    /// This method checks both the instruction result and the current limit status
    /// to determine if the frame failed due to limit enforcement.
    ///
    /// # Arguments
    ///
    /// * `result` - The frame result to check
    ///
    /// # Returns
    ///
    /// Returns `true` if the result indicates limit exceeded, `false` otherwise.
    #[inline]
    pub fn is_exceeding_limit_result(&mut self, instruction_result: InstructionResult) -> bool {
        instruction_result == Self::EXCEEDING_LIMIT_INSTRUCTION_RESULT &&
            self.check_limit().exceeded_limit()
    }

    /// Checks if the halt reason indicates that the limit has been exceeded.
    ///
    /// # Arguments
    ///
    /// * `halt_reason` - The halt reason to check
    ///
    /// # Returns
    ///
    /// Returns `true` if the halt reason indicates that the limit has been exceeded, `false`
    /// otherwise.
    pub fn is_exceeding_limit_halt(&mut self, halt_reason: &OpHaltReason) -> bool {
        matches!(halt_reason, &OpHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Basic))) &&
            self.check_limit().exceeded_limit()
    }
}

/// Result type indicating whether additional limits have been exceeded.
///
/// This enum provides detailed information about limit enforcement results,
/// including which specific limit was exceeded and the current usage values.
#[allow(clippy::enum_variant_names)]
#[allow(unused)]
#[allow(missing_docs)]
#[derive(Debug, Default, Copy, Clone)]
pub enum AdditionalLimitResult {
    /// Indicates that all limits are within their configured thresholds.
    #[default]
    WithinLimit,

    /// Indicates that the data size limit has been exceeded.
    ///
    /// # Fields
    ///
    /// * `limit` - The configured data size limit
    /// * `used` - The current data size usage
    ExceedsDataLimit { limit: u64, used: u64 },

    /// Indicates that the KV update limit has been exceeded.
    ///
    /// # Fields
    ///
    /// * `limit` - The configured KV update limit
    /// * `used` - The current KV update count
    ExceedsKVUpdateLimit { limit: u64, used: u64 },

    /// Indicates that the compute gas limit has been exceeded.
    ///
    /// # Fields
    ///
    /// * `limit` - The configured compute gas limit
    /// * `used` - The current compute gas usage
    ExceedsComputeGasLimit { limit: u64, used: u64 },

    /// Indicates that the state growth limit has been exceeded.
    ///
    /// # Fields
    ///
    /// * `limit` - The configured state growth limit
    /// * `used` - The current state growth usage
    ExceedsStateGrowthLimit { limit: u64, used: u64 },
}

impl AdditionalLimitResult {
    /// Returns the [`MegaHaltReason`] if the limit has been exceeded, otherwise returns `None`.
    pub fn maybe_halt_reason(&self) -> Option<MegaHaltReason> {
        match self {
            Self::ExceedsDataLimit { limit, used } => {
                Some(MegaHaltReason::DataLimitExceeded { limit: *limit, actual: *used })
            }
            Self::ExceedsKVUpdateLimit { limit, used } => {
                Some(MegaHaltReason::KVUpdateLimitExceeded { limit: *limit, actual: *used })
            }
            Self::ExceedsComputeGasLimit { limit, used } => {
                Some(MegaHaltReason::ComputeGasLimitExceeded { limit: *limit, actual: *used })
            }
            Self::ExceedsStateGrowthLimit { limit, used } => {
                Some(MegaHaltReason::StateGrowthLimitExceeded { limit: *limit, actual: *used })
            }
            Self::WithinLimit => None,
        }
    }
}

/// Implementation of utility methods for `AdditionalLimitResult`.
#[allow(unused)]
impl AdditionalLimitResult {
    /// Checks if any limit has been exceeded.
    ///
    /// # Returns
    ///
    /// Returns `true` if either the data limit or KV update limit has been exceeded.
    #[inline]
    pub(crate) const fn exceeded_limit(&self) -> bool {
        !matches!(self, Self::WithinLimit)
    }

    /// Checks if all limits are within their configured thresholds.
    ///
    /// # Returns
    ///
    /// Returns `true` if no limits have been exceeded.
    #[inline]
    pub(crate) const fn within_limit(&self) -> bool {
        matches!(self, Self::WithinLimit)
    }
}

/* Hooks for transaction execution lifecycle. */
impl AdditionalLimit {
    /// Records the compute gas used and checks if the limit has been exceeded.
    pub(crate) fn record_compute_gas(&mut self, compute_gas_used: u64) -> AdditionalLimitResult {
        self.compute_gas_tracker.record_gas_used(compute_gas_used);

        self.check_limit()
    }

    /// Rescues gas from the limit exceeding. This method is used to record the remaining gas of a
    /// frame after the limit exceeds. Typically, the frame execution will halt consuming all the
    /// remaining gas, we need to record so that we can give it back to the transaction sender
    /// afterwards.
    pub(crate) fn rescue_gas(&mut self, gas: &Gas) {
        self.rescued_gas += gas.remaining();
    }

    /// Hook called when a new transaction starts.
    pub(crate) fn before_tx_start(&mut self, tx: &MegaTransaction) -> AdditionalLimitResult {
        // record the data size of the tx itself
        self.data_size_tracker.record_tx_data(tx);
        // record the data size of the eip7702 account info update
        self.data_size_tracker.record_eip7702_account_info_update(tx);
        // record the data size of the caller's account info update
        self.data_size_tracker.record_account_info_update(tx.base.caller);

        // record the data size of the eip7702 account info update
        self.kv_update_counter.record_eip7702_account_info_update(tx);
        // record the kv update of the caller's account info update
        self.kv_update_counter.record_account_info_update(tx.base.caller);

        self.check_limit()
    }

    /// Hook called before a new execution frame is initialized.
    pub(crate) fn before_frame_init<JOURNAL: JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        frame_init: &FrameInit,
        journal: &mut JOURNAL,
    ) -> AdditionalLimitResult {
        match &frame_init.frame_input {
            FrameInput::Empty => unreachable!(),
            FrameInput::Call(call_inputs) => {
                let has_transfer = call_inputs.transfers_value();
                // new frame in data size tracker
                self.data_size_tracker.record_call(call_inputs.target_address, has_transfer);
                // new frame in kv update counter
                self.kv_update_counter.record_call(call_inputs.target_address, has_transfer);
                // new frame in state growth tracker
                self.state_growth_tracker.record_call(
                    journal,
                    call_inputs.target_address,
                    has_transfer,
                );
            }
            FrameInput::Create(_) => {
                // new frame in data size tracker
                self.data_size_tracker.record_create();
                // new frame in kv update counter
                self.kv_update_counter.record_create();
                // new frame in state growth tracker
                self.state_growth_tracker.record_create();
            }
        }

        self.check_limit()
    }

    /// Hook called when a new execution frame is successfully initialized in `frame_init` and needs
    /// to be run (i.e., target address has code).
    pub(crate) fn after_frame_init_on_frame(
        &mut self,
        frame: &EthFrame<EthInterpreter>,
    ) -> AdditionalLimitResult {
        if frame.data.is_create() {
            // we need to record the created address
            let created_address =
                frame.data.created_address().expect("created address is none for create frame");
            self.data_size_tracker.record_created_account(created_address);
            self.kv_update_counter.record_created_account(created_address);
        }

        self.check_limit()
    }

    /// Hook called when a create frame finishes running in `frame_run`.
    pub(crate) fn after_create_frame_run(
        &mut self,
        result: &InterpreterResult,
    ) -> AdditionalLimitResult {
        // if the limit has already been exceeded, return early
        if self.has_exceeded_limit.exceeded_limit() {
            return self.has_exceeded_limit;
        }

        // record the created contract code
        self.data_size_tracker.record_created_contract_code(result.output.len() as u64);

        self.check_limit()
    }

    /// Hook called when returning a frame result to parent frame in `frame_return_result`.
    pub(crate) fn before_frame_return_result<const LAST_FRAME: bool>(
        &mut self,
        result: &FrameResult,
    ) -> AdditionalLimitResult {
        // TRUE if the current function is called twice for the top-level frame. If the top-level
        // frame has child frames, the top-level frame's result will be handled twice (one via
        // `EvmTr::frame_return_result`, the other via `Handler::last_frame_result`). This flag is
        // used to distinguish these two cases.
        let duplicate_return_frame_result =
            LAST_FRAME && self.data_size_tracker.current_frame().is_none();

        // end the current frame and merge into the previous frame
        self.data_size_tracker.end_frame(result.instruction_result(), LAST_FRAME);
        self.kv_update_counter.end_frame(result.instruction_result(), LAST_FRAME);
        self.state_growth_tracker.end_frame(result.instruction_result(), LAST_FRAME);

        let limit_check = self.check_limit();
        if limit_check.exceeded_limit() && !duplicate_return_frame_result {
            // We rescue the remaining gas of the frame after the limit exceeds.
            // This gas will be refunded to the transaction sender in `last_frame_result`.
            self.rescue_gas(result.gas());
        }
        limit_check
    }

    /// Hook called when an orginally zero storage slot is written non-zero value for the first time
    /// in the transaction.
    pub(crate) fn on_sstore(
        &mut self,
        target_address: Address,
        slot: U256,
        store_reuslt: &SStoreResult,
    ) -> AdditionalLimitResult {
        self.data_size_tracker.record_sstore(target_address, slot, store_reuslt);
        self.kv_update_counter.record_sstore(target_address, slot, store_reuslt);

        self.check_limit()
    }

    /// Hook called when a log is written.
    pub(crate) fn on_log(&mut self, num_topics: u64, data_size: u64) -> AdditionalLimitResult {
        self.data_size_tracker.record_log(num_topics, data_size);

        self.check_limit()
    }
}

/// Creates a `FrameResult` indicating that the limit is exceeded.
///
/// This utility function creates a frame result that signals limit exceeded.
///
/// # Arguments
///
/// * `gas_limit` - The gas limit of the transaction
/// * `return_memory_offset` - The memory offset of the return value if the frame is a call frame.
///   `None` if the frame is a create frame
///
/// # Returns
///
/// A `FrameResult` indicating that the limit is exceeded with
/// [`AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT`] instruction result.
pub(crate) fn create_exceeding_limit_frame_result(
    gas: Gas,
    return_memory_offset: Option<Range<usize>>,
) -> FrameResult {
    match return_memory_offset {
        None => {
            FrameResult::Create(CreateOutcome::new(create_exceeding_interpreter_result(gas), None))
        }
        Some(return_memory_offset) => FrameResult::Call(CallOutcome::new(
            create_exceeding_interpreter_result(gas),
            return_memory_offset,
        )),
    }
}

/// Creates an interpreter result indicating that the limit is exceeded.
///
/// This utility function creates an interpreter result that signals limit exceeded.
///
/// # Arguments
///
/// * `gas` - The gas of the interpreter result
///
/// # Returns
///
/// An interpreter result indicating that the limit is exceeded with
/// [`AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT`] instruction result.
pub(crate) fn create_exceeding_interpreter_result(gas: Gas) -> InterpreterResult {
    InterpreterResult::new(AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT, Bytes::new(), gas)
}

/// Marks an existing interpreter result as exceeding the limit.
///
/// This utility function modifies an existing interpreter result to indicate that
/// limits have been exceeded. Remaining gas is preserved.
///
/// # Arguments
///
/// * `result` - The interpreter result to modify
pub(crate) fn mark_interpreter_result_as_exceeding_limit(result: &mut InterpreterResult) {
    // mark the instruction result as exceeding the limit and discard the output
    result.result = AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT;
}

/// Marks a frame result as exceeding the limit.
///
/// This utility function modifies a frame result to indicate that limits have been exceeded.
/// Remaining gas is preserved.
///
/// # Arguments
///
/// * `result` - The frame result to modify
pub(crate) fn mark_frame_result_as_exceeding_limit(result: &mut FrameResult) {
    match result {
        FrameResult::Call(call_outcome) => {
            mark_interpreter_result_as_exceeding_limit(&mut call_outcome.result);
        }
        FrameResult::Create(create_outcome) => {
            mark_interpreter_result_as_exceeding_limit(&mut create_outcome.result);
        }
    }
}
