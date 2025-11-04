use core::ops::Range;

use alloy_primitives::{Address, Bytes, U256};
use revm::{
    handler::{EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallOutcome, CreateOutcome,
        FrameInput, Gas, InstructionResult, InterpreterResult, SStoreResult,
    },
};

use crate::{constants, MegaHaltReason, MegaTransaction};

mod compute_gas;
mod data_size;
mod kv_update;

pub use compute_gas::*;
pub use data_size::*;
pub use kv_update::*;

/// Additional limits for the `MegaETH` EVM beyond standard EVM limits.
///
/// This struct coordinates both data size and key-value update limits, providing
/// a unified interface for limit enforcement during transaction execution. When
/// limits are exceeded, transactions halt with `OutOfGas`, consuming all remaining gas.
///
/// # Tracking Details
///
/// - **Data Size**: Tracks transaction data (110 bytes base + calldata + access lists +
///   authorizations), caller/authority account updates (40 bytes each), log data, storage writes
///   (40 bytes when original ≠ new), account updates from calls/creates (40 bytes), and contract
///   code size
/// - **KV Updates**: Tracks transaction caller + authority updates, storage writes (when original ≠
///   new), and account updates from value transfers and creates
///
/// # Default Limits (`MINI_REX`)
///
/// - Data limit: 3.125 MB (25% of 12.5 MB block limit)
/// - KV update limit: 1,000 operations
#[derive(Debug)]
pub struct AdditionalLimit {
    /// A flag to indicate if the limit has been exceeded, set when the limit is exceeded. Once
    /// set, it will not be changed until `reset` is called. The current size and count in
    /// neither `kv_update_counter` nor `data_size_tracker` is reliable since when the limit is
    /// exceeded, the frames will be reverted and the data size and count will be discarded.
    pub has_exceeded_limit: AdditionalLimitResult,

    /// The data limit for the EVM. When the data limit is reached, the transaction will error and
    /// halt (consuming all remaining gas).
    ///
    /// This limit controls the maximum total data size that can be generated during
    /// transaction execution, including transaction data, logs, storage operations, etc.
    pub data_limit: u64,

    /// The key-value update limit for the EVM. When the key-value update limit is reached, the
    /// transaction will error and halt (consuming all remaining gas).
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
}

impl Default for AdditionalLimit {
    fn default() -> Self {
        Self {
            has_exceeded_limit: AdditionalLimitResult::WithinLimit,
            data_limit: constants::mini_rex::TX_DATA_LIMIT,
            kv_update_limit: constants::mini_rex::TX_KV_UPDATE_LIMIT,
            compute_gas_limit: constants::mini_rex::TX_COMPUTE_GAS_LIMIT,
            compute_gas_tracker: compute_gas::ComputeGasTracker::new(),
            data_size_tracker: data_size::DataSizeTracker::new(),
            kv_update_counter: kv_update::KVUpdateCounter::new(),
        }
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
    pub fn reset(&mut self) {
        self.has_exceeded_limit = AdditionalLimitResult::WithinLimit;
        self.data_size_tracker.reset();
        self.kv_update_counter.reset();
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
    pub fn check_limit(&mut self) -> AdditionalLimitResult {
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
    pub fn is_exceeding_limit_result(&mut self, instruction_result: InstructionResult) -> bool {
        instruction_result == Self::EXCEEDING_LIMIT_INSTRUCTION_RESULT &&
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
    pub(crate) fn exceeded_limit(&self) -> bool {
        matches!(self, Self::ExceedsDataLimit { .. } | Self::ExceedsKVUpdateLimit { .. })
    }

    /// Checks if all limits are within their configured thresholds.
    ///
    /// # Returns
    ///
    /// Returns `true` if no limits have been exceeded.
    pub(crate) fn within_limit(&self) -> bool {
        matches!(self, Self::WithinLimit)
    }
}

/* Hooks for transaction execution lifecycle. */
impl AdditionalLimit {
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
    pub(crate) fn before_frame_init(&mut self, frame_init: &FrameInit) -> AdditionalLimitResult {
        match &frame_init.frame_input {
            FrameInput::Empty => unreachable!(),
            FrameInput::Call(call_inputs) => {
                // new frame in data size tracker
                self.data_size_tracker
                    .record_call(call_inputs.target_address, call_inputs.transfers_value());
                // new frame in kv update counter
                self.kv_update_counter
                    .record_call(call_inputs.target_address, call_inputs.transfers_value());
            }
            FrameInput::Create(_) => {
                // new frame in data size tracker
                self.data_size_tracker.record_create();
                // new frame in kv update counter
                self.kv_update_counter.record_create();
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
    pub(crate) fn before_frame_return_result(
        &mut self,
        result: &FrameResult,
        last_frame: bool,
    ) -> AdditionalLimitResult {
        // end the current frame and merge into the previous frame
        self.data_size_tracker.end_frame(result.instruction_result(), last_frame);
        self.kv_update_counter.end_frame(result.instruction_result(), last_frame);

        self.check_limit()
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
/// This utility function creates a frame result that signals limit exceeded,
/// consuming all remaining gas and discarding any output.
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
    gas_limit: u64,
    return_memory_offset: Option<Range<usize>>,
) -> FrameResult {
    match return_memory_offset {
        None => FrameResult::Create(CreateOutcome::new(
            InterpreterResult::new(
                AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
                Bytes::new(),
                Gas::new_spent(gas_limit),
            ),
            None,
        )),
        Some(return_memory_offset) => FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
                Bytes::new(),
                Gas::new_spent(gas_limit),
            ),
            return_memory_offset,
        )),
    }
}

/// Marks an existing interpreter result as exceeding the limit.
///
/// This utility function modifies an existing interpreter result to indicate that
/// limits have been exceeded, consuming all remaining gas and discarding output.
///
/// # Arguments
///
/// * `result` - The interpreter result to modify
pub(crate) fn mark_interpreter_result_as_exceeding_limit(result: &mut InterpreterResult) {
    // spend all the gas and discard all the refund
    result.gas.spend_all();
    result.gas.set_refund(0);

    // mark the instruction result as exceeding the limit and discard the output
    result.result = AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT;
    result.output = Bytes::new();
}
