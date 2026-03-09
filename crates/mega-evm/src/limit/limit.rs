use core::ops::Range;

use alloy_primitives::{Address, Bytes, U256};
use op_revm::OpHaltReason;
use revm::{
    context::result::{HaltReason, OutOfGasError},
    handler::{EthFrame, FrameResult, ItemOrResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallOutcome, CreateOutcome,
        FrameInput, Gas, InstructionResult, InterpreterAction, InterpreterResult, SStoreResult,
    },
};

use super::{compute_gas, data_size, frame_limit::TxRuntimeLimit, kv_update, state_growth};
use crate::{EvmTxRuntimeLimits, JournalInspectTr, MegaSpecId, MegaTransaction};

use super::LimitCheck;

/// Additional limits for the `MegaETH` EVM beyond standard EVM limits.
///
/// This struct coordinates four independent resource limits: compute gas, data size,
/// key-value updates, and state growth. Each limit is tracked separately and enforced during
/// transaction execution.
///
/// ## TX-Level Halt Enforcement
///
/// TX-level exceed is represented as `InstructionResult::OutOfGas`.
/// Remaining gas is rescued and later refunded to the sender.
/// - **Compute gas**: TX-level check is always active (`min(tx_limit, detained_limit)`).
/// - **Data size / KV update / State growth**: TX-level check applies in pre-Rex4 specs.
///
/// ## Per-Frame Enforcement (Rex4+)
///
/// In Rex4+, all four limits use per-frame budgets.
/// Each inner call frame receives `remaining * 98 / 100` of the parent's remaining budget.
/// When a frame exceeds its per-frame budget, it **reverts** (not halts) and gas returns to
/// the parent frame, which can continue executing:
/// - **State growth**: Reverted child's growth is discarded (`discardable_usage` dropped).
/// - **Data size**: Reverted child's discardable data is dropped, protecting parent's budget.
/// - **KV updates**: Reverted child's discardable KV ops are dropped, protecting parent's budget.
/// - **Compute gas**: Reverted child's gas still counts toward parent (gas is always persistent).
///   Per-frame limits act as "early termination guardrails" only, not budget protection. Compute
///   gas still retains TX-level detained checking in all specs.
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
/// - **State Growth**: Tracks net new accounts + net new storage slots
#[derive(Debug)]
pub struct AdditionalLimit {
    /// A flag to indicate if the limit has been exceeded, set when the limit is exceeded. The
    /// current size and count in neither `kv_update_counter` nor `data_size_tracker` is
    /// reliable since when the limit is exceeded, the frames will be reverted and the data
    /// size and count will be discarded.
    pub has_exceeded_limit: LimitCheck,

    /// The total remaining gas after the limit exceeds.
    pub rescued_gas: u64,

    /// The original limits set by the EVM. Some of the limits may be overridden (such as the
    /// compute gas limit) during transaction execution. We keep the original limits to be able to
    /// reset the limits before each transaction.
    pub limits: EvmTxRuntimeLimits,

    /// A tracker for the state growth during transaction execution.
    pub(crate) state_growth: state_growth::StateGrowthTracker,

    /// A tracker for the total data size (in bytes) generated from a transaction execution.
    pub(crate) data_size: data_size::DataSizeTracker,

    /// A tracker for the total KV updates during transaction execution.
    pub(crate) kv_update: kv_update::KVUpdateTracker,

    /// A tracker for the total compute gas consumed during transaction execution.
    pub(crate) compute_gas: compute_gas::ComputeGasTracker,
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
    pub fn new(spec: MegaSpecId, limits: EvmTxRuntimeLimits) -> Self {
        Self {
            has_exceeded_limit: LimitCheck::WithinLimit,
            rescued_gas: 0,
            limits,
            state_growth: state_growth::StateGrowthTracker::new(spec, limits.tx_state_growth_limit),
            data_size: data_size::DataSizeTracker::new(spec, limits.tx_data_size_limit),
            kv_update: kv_update::KVUpdateTracker::new(spec, limits.tx_kv_updates_limit),
            compute_gas: compute_gas::ComputeGasTracker::new(spec, limits.tx_compute_gas_limit),
        }
    }
}

impl AdditionalLimit {
    /// The [`InstructionResult`] to indicate that the limit is exceeded (TX-level).
    ///
    /// This constant is used for TX-level additional-limit exceeds.
    /// For TX-level exceeds, this is `OutOfGas` (halt path, with rescued gas refund).
    /// For frame-local exceeds (Rex4+), use
    /// `exceeding_instruction_result()` which returns `Revert` instead.
    pub const EXCEEDING_LIMIT_INSTRUCTION_RESULT: InstructionResult = InstructionResult::OutOfGas;

    /// Returns the appropriate [`InstructionResult`] for the current limit exceed.
    ///
    /// - **Frame-local (Rex4+)**: `Revert` — gas returns to the parent frame naturally.
    /// - **TX-level**: `OutOfGas` — halt, gas consumed (rescued via `rescued_gas`).
    #[inline]
    pub(crate) fn exceeding_instruction_result(&self) -> InstructionResult {
        if self.has_exceeded_limit.is_frame_local() {
            InstructionResult::Revert
        } else {
            Self::EXCEEDING_LIMIT_INSTRUCTION_RESULT
        }
    }

    /// Resets the internal state for a new transaction or block.
    ///
    /// This method clears both the data size tracker and KV update counter,
    /// preparing the limit system for a new execution context.
    ///
    /// Each tracker internally handles spec-gated behavior (e.g., `ComputeGasTracker`
    /// resets the detained limit only for Rex1+).
    pub fn reset(&mut self) {
        self.has_exceeded_limit = LimitCheck::WithinLimit;
        self.rescued_gas = 0;
        self.compute_gas.reset();
        self.state_growth.reset();
        self.data_size.reset();
        self.kv_update.reset();
    }

    /// Gets the usage of the additional limits.
    #[inline]
    pub fn get_usage(&self) -> LimitUsage {
        LimitUsage {
            data_size: self.data_size.tx_usage(),
            kv_updates: self.kv_update.tx_usage(),
            compute_gas: self.compute_gas.tx_usage(),
            state_growth: self.state_growth.tx_usage(),
        }
    }

    /// Pushes an empty frame to all trackers so `before_frame_return_result` can pop
    /// them to keep stacks aligned with the EVM's call stack.
    ///
    /// Used when `frame_init` returns an early `Result` (e.g., inspector interception,
    /// access control interception) without going through `after_frame_init`.
    #[inline]
    pub(crate) fn push_empty_frame(&mut self) {
        self.state_growth.push_empty_frame();
        self.data_size.push_empty_frame();
        self.kv_update.push_empty_frame();
        self.compute_gas.push_empty_frame();
    }

    /// Returns the current effective compute gas limit (may be detained/lowered by volatile
    /// data access).
    #[inline]
    pub fn compute_gas_limit(&self) -> u64 {
        self.compute_gas.tx_limit()
    }

    /// Returns the remaining compute gas of the current call.
    ///
    /// In Rex4+, returns the minimum of the caller's per-frame remaining compute gas
    /// and the TX-level detained remaining, reflecting the actual gas available before
    /// execution halts (whether due to frame budget or gas detention).
    /// If no frame exists yet (direct TX → system contract), returns the TX-level
    /// remaining which accounts for intrinsic compute gas.
    /// In pre-Rex4, falls back to TX-level remaining compute gas.
    #[inline]
    pub fn current_call_remaining_compute_gas(&self) -> u64 {
        self.compute_gas.current_call_remaining()
    }

    /// Sets the compute gas limit to a new value.
    /// This is used to dynamically lower the compute gas limit when volatile data is accessed.
    /// The new limit must be lower than the current limit.
    #[inline]
    pub fn set_compute_gas_limit(&mut self, new_limit: u64) {
        self.compute_gas.set_detained_limit(new_limit);
    }

    /// Checks if any of the configured limits have been exceeded.
    ///
    /// This method examines data size, KV update, compute gas, and state growth in fixed order
    /// and returns the first exceeded limit.
    ///
    /// # Returns
    ///
    /// Returns a [`LimitCheck`] indicating whether limits have been exceeded
    /// and which specific limit was exceeded if any.
    #[inline]
    pub fn check_limit(&mut self) -> LimitCheck {
        // short circuit if the limit has already been exceeded
        if self.has_exceeded_limit.exceeded_limit() {
            return self.has_exceeded_limit;
        }

        let data_size_check = self.data_size.check_limit();
        if data_size_check.exceeded_limit() {
            self.has_exceeded_limit = data_size_check;
            return self.has_exceeded_limit;
        }

        let kv_update_check = self.kv_update.check_limit();
        if kv_update_check.exceeded_limit() {
            self.has_exceeded_limit = kv_update_check;
            return self.has_exceeded_limit;
        }

        // Per-frame compute gas check (Rex4+) and TX-level detained check (all specs).
        let compute_gas_check = self.compute_gas.check_limit();
        if compute_gas_check.exceeded_limit() {
            self.has_exceeded_limit = compute_gas_check;
            return self.has_exceeded_limit;
        }

        // State growth check:
        // - Rex4+: frame-local budget check.
        // - pre-Rex4: TX-level check inside `state_growth.check_limit()`.
        let state_growth_check = self.state_growth.check_limit();
        if state_growth_check.exceeded_limit() {
            self.has_exceeded_limit = state_growth_check;
            return self.has_exceeded_limit;
        }

        self.has_exceeded_limit
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

/* Hooks for transaction execution lifecycle. */
impl AdditionalLimit {
    /// Records the compute gas used and returns `false` if the limit has been exceeded.
    pub(crate) fn record_compute_gas(&mut self, compute_gas_used: u64) -> bool {
        self.compute_gas.record_gas_used(compute_gas_used);

        !self.check_limit().exceeded_limit()
    }

    /// Rescues gas from the limit exceeding. This method is used to record the remaining gas of a
    /// frame after the limit exceeds. Typically, the frame execution will halt consuming all the
    /// remaining gas, we need to record so that we can give it back to the transaction sender
    /// afterwards.
    pub(crate) fn rescue_gas(&mut self, gas: &Gas) {
        self.rescued_gas += gas.remaining();
    }

    /// Rescue remaining gas from a frame result if a TX-level additional limit has been
    /// exceeded.
    ///
    /// This must be called before any inspector callback (`frame_end`) that might modify the
    /// gas via `spend_all()`, so the correct `gas.remaining()` value is captured.
    /// The rescued gas is later refunded to the transaction sender in `last_frame_result`.
    pub(crate) fn try_rescue_gas(&mut self, gas: &Gas) {
        let limit_check = self.check_limit();
        if limit_check.exceeded_limit() && !limit_check.is_frame_local() {
            self.rescue_gas(gas);
        }
    }

    /// Hook called when a new transaction starts.
    /// Returns `false` if the limit has been exceeded.
    pub(crate) fn before_tx_start(&mut self, tx: &MegaTransaction) -> bool {
        self.state_growth.before_tx_start(tx);
        self.data_size.before_tx_start(tx);
        self.kv_update.before_tx_start(tx);

        !self.check_limit().exceeded_limit()
    }

    /// Hook called before a new execution frame is initialized. Returns `Some(FrameResult)` if the
    /// limit is exceeded and the frame should terminate early with the returned `FrameResult`.
    pub(crate) fn before_frame_init<JOURNAL: JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        frame_init: &FrameInit,
        journal: &mut JOURNAL,
    ) -> Result<Option<FrameResult>, JOURNAL::DBError> {
        // new frame in frame limit trackers
        self.state_growth.before_frame_init(frame_init, journal)?;
        self.data_size.before_frame_init(frame_init, journal)?;
        self.kv_update.before_frame_init(frame_init, journal)?;
        self.compute_gas.before_frame_init(frame_init, journal)?;

        if self.check_limit().exceeded_limit() {
            // if the limit is exceeded, create an error frame result and return it directly
            let (gas_limit, return_memory_offset) = match &frame_init.frame_input {
                FrameInput::Create(inputs) => (inputs.gas_limit, None),
                FrameInput::Call(inputs) => {
                    (inputs.gas_limit, Some(inputs.return_memory_offset.clone()))
                }
                FrameInput::Empty => unreachable!(),
            };
            let output = self.has_exceeded_limit.revert_data();
            let result = create_exceeding_limit_frame_result(
                self.exceeding_instruction_result(),
                Gas::new(gas_limit),
                return_memory_offset,
                output,
            );
            self.try_rescue_gas(result.gas());
            return Ok(Some(result));
        }

        Ok(None)
    }

    /// Hook called when a new execution frame is successfully initialized in `frame_init` and needs
    /// to be run (i.e., target address has code).
    pub(crate) fn after_frame_init(
        &mut self,
        init_result: &ItemOrResult<&mut EthFrame<EthInterpreter>, FrameResult>,
    ) {
        if let ItemOrResult::Item(frame) = &init_result {
            self.state_growth.after_frame_init_on_frame(frame);
            self.data_size.after_frame_init_on_frame(frame);
            self.kv_update.after_frame_init_on_frame(frame);
            self.compute_gas.after_frame_init_on_frame(frame);
        } else if let ItemOrResult::Result(result) = init_result {
            // Rescue gas if a TX-level limit was exceeded. This covers the
            // before_frame_init early-return path and any other Result from frame_init.
            self.try_rescue_gas(result.gas());
        }
    }

    /// Hook called before a frame run. If the limit is exceeded, return an interpreter result
    /// indicating that the limit is exceeded.
    pub(crate) fn before_frame_run(
        &mut self,
        frame: &EthFrame<EthInterpreter>,
    ) -> Option<InterpreterResult> {
        self.state_growth.before_frame_run(frame);
        self.data_size.before_frame_run(frame);
        self.kv_update.before_frame_run(frame);
        self.compute_gas.before_frame_run(frame);

        if self.check_limit().exceeded_limit() {
            let output = self.has_exceeded_limit.revert_data();
            return Some(create_exceeding_interpreter_result(
                self.exceeding_instruction_result(),
                frame.interpreter.gas,
                output,
            ));
        }
        None
    }

    /// Hook called after frame action processing in `frame_run`.
    ///
    /// Records compute gas cost induced in frame action processing (e.g., code deposit cost),
    /// marks the frame result as exceeding limit if needed, and rescues gas if a TX-level limit
    /// was exceeded (before any inspector callback that might modify gas).
    pub(crate) fn after_frame_run(
        &mut self,
        result: &mut FrameResult,
        gas_remaining_before_process_action: Option<u64>,
    ) {
        if let Some(gas_remaining_before) = gas_remaining_before_process_action {
            let compute_gas_cost = gas_remaining_before.saturating_sub(result.gas().remaining());
            if !self.record_compute_gas(compute_gas_cost) {
                mark_frame_result_as_exceeding_limit(
                    result,
                    self.exceeding_instruction_result(),
                    Default::default(),
                );
            }
        }
        // Rescue gas if a TX-level additional limit has been exceeded.
        // This must happen before any inspector callback (`frame_end`) that might modify
        // the gas via `spend_all()`, so the correct `gas.remaining()` value is captured.
        self.try_rescue_gas(result.gas());
    }

    /// Hook called when a frame finishes running in `frame_run`. If the limit is exceeded, mark
    /// in place the interpreter result as exceeding the limit.
    pub(crate) fn after_frame_run_instructions<'a>(
        &mut self,
        frame: &'a EthFrame<EthInterpreter>,
        action: &'a mut InterpreterAction,
    ) {
        self.state_growth.after_frame_run(frame, action);
        self.data_size.after_frame_run(frame, action);
        self.kv_update.after_frame_run(frame, action);
        self.compute_gas.after_frame_run(frame, action);

        if let InterpreterAction::Return(interpreter_result) = action {
            if frame.data.is_create() {
                // if the limit has already been exceeded, return early
                if self.has_exceeded_limit.exceeded_limit() {
                    let output = self.has_exceeded_limit.revert_data();
                    mark_interpreter_result_as_exceeding_limit(
                        interpreter_result,
                        self.exceeding_instruction_result(),
                        output,
                    );
                    return;
                }

                // if the limit has been exceeded, we mark the interpreter result as
                // exceeding the limit
                if self.check_limit().exceeded_limit() {
                    let output = self.has_exceeded_limit.revert_data();
                    mark_interpreter_result_as_exceeding_limit(
                        interpreter_result,
                        self.exceeding_instruction_result(),
                        output,
                    );
                }
            }
        }
    }

    /// Hook called when returning a frame result to parent frame in `frame_return_result` or
    /// `last_frame_result`. May modify the frame result in place if the limit is exceeded.
    pub(crate) fn before_frame_return_result<const LAST_FRAME: bool>(
        &mut self,
        result: &mut FrameResult,
    ) {
        // TRUE if the current function is called twice for the top-level frame. If the top-level
        // frame has child frames, the top-level frame's result will be handled twice (one via
        // `EvmTr::frame_return_result`, the other via `Handler::last_frame_result`). This flag is
        // used to distinguish these two cases.
        let duplicate_return_frame_result = LAST_FRAME && !self.data_size.has_active_frame();

        // Pop frame from the frame limit trackers.
        self.state_growth.before_frame_return_result::<LAST_FRAME>(result);
        self.data_size.before_frame_return_result::<LAST_FRAME>(result);
        self.kv_update.before_frame_return_result::<LAST_FRAME>(result);
        self.compute_gas.before_frame_return_result::<LAST_FRAME>(result);

        // Frame-level limit handling (Rex4+): check if the child frame exceeded its
        // frame-local budget. The detection may not have happened during execution, so
        // we call check_limit() here to ensure it's caught.
        // If frame-local, absorb it — clear the exceed flag and change to Revert so
        // remaining gas returns to the caller. State changes are reverted by revm's
        // Revert handling. This works at any depth including the top-level frame.
        let limit_check = self.check_limit();
        if limit_check.exceeded_limit() && !duplicate_return_frame_result {
            if limit_check.is_frame_local() {
                let output = limit_check.revert_data();
                self.has_exceeded_limit = LimitCheck::WithinLimit;
                match result {
                    FrameResult::Call(o) => {
                        o.result.result = InstructionResult::Revert;
                        o.result.output = output;
                    }
                    FrameResult::Create(o) => {
                        o.result.result = InstructionResult::Revert;
                        o.result.output = output;
                    }
                }
            } else {
                // Gas has already been rescued at the point where the limit was
                // exceeded (before_frame_init, after_frame_init, or after_frame_run).
                // Just mark the result as exceeding the limit.
                mark_frame_result_as_exceeding_limit(
                    result,
                    Self::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
                    Default::default(),
                );
            }
        }
    }

    /// Hook called when an orginally zero storage slot is written non-zero value for the first time
    /// in the transaction. Returns `false` if the limit has been exceeded.
    pub(crate) fn on_sstore(
        &mut self,
        target_address: Address,
        slot: U256,
        store_result: &SStoreResult,
    ) -> bool {
        self.state_growth.after_sstore(target_address, slot, store_result);
        self.data_size.after_sstore(target_address, slot, store_result);
        self.kv_update.after_sstore(target_address, slot, store_result);

        !self.check_limit().exceeded_limit()
    }

    /// Hook called when a log is written. Returns `false` if the limit has been exceeded.
    pub(crate) fn on_log(&mut self, num_topics: u64, data_size: u64) -> bool {
        self.state_growth.after_log(num_topics, data_size);
        self.data_size.after_log(num_topics, data_size);

        !self.check_limit().exceeded_limit()
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
/// A `FrameResult` indicating that the limit is exceeded with the given instruction result.
fn create_exceeding_limit_frame_result(
    instruction_result: InstructionResult,
    gas: Gas,
    return_memory_offset: Option<Range<usize>>,
    output: Bytes,
) -> FrameResult {
    match return_memory_offset {
        None => FrameResult::Create(CreateOutcome::new(
            create_exceeding_interpreter_result(instruction_result, gas, output),
            None,
        )),
        Some(return_memory_offset) => FrameResult::Call(CallOutcome::new(
            create_exceeding_interpreter_result(instruction_result, gas, output),
            return_memory_offset,
        )),
    }
}

/// Creates an interpreter result indicating that the limit is exceeded.
fn create_exceeding_interpreter_result(
    instruction_result: InstructionResult,
    gas: Gas,
    output: Bytes,
) -> InterpreterResult {
    InterpreterResult::new(instruction_result, output, gas)
}

/// Marks an existing interpreter result as exceeding the limit.
fn mark_interpreter_result_as_exceeding_limit(
    result: &mut InterpreterResult,
    instruction_result: InstructionResult,
    output: Bytes,
) {
    result.result = instruction_result;
    result.output = output;
}

/// Marks a frame result as exceeding the limit.
pub(crate) fn mark_frame_result_as_exceeding_limit(
    result: &mut FrameResult,
    instruction_result: InstructionResult,
    output: Bytes,
) {
    match result {
        FrameResult::Call(call_outcome) => {
            mark_interpreter_result_as_exceeding_limit(
                &mut call_outcome.result,
                instruction_result,
                output,
            );
        }
        FrameResult::Create(create_outcome) => {
            mark_interpreter_result_as_exceeding_limit(
                &mut create_outcome.result,
                instruction_result,
                output,
            );
        }
    }
}
