use core::ops::Range;

use alloy_primitives::{Address, Bytes, U256};
use op_revm::OpHaltReason;
use revm::{
    context::result::{HaltReason, OutOfGasError},
    handler::{EthFrame, FrameResult, ItemOrResult},
    interpreter::{
        gas::calculate_initial_tx_gas_for_tx, interpreter::EthInterpreter,
        interpreter_action::FrameInit, CallOutcome, CreateOutcome, FrameInput, Gas,
        InstructionResult, InterpreterAction, InterpreterResult, SStoreResult,
    },
};

use super::{
    compute_gas, data_size, frame_limit::TxRuntimeLimit, kv_update, state_growth,
    storage_call_stipend,
};
use crate::{
    EvmTxRuntimeLimits, JournalInspectTr, MegaHaltReason, MegaSpecId, MegaTransaction,
    VolatileDataAccess,
};

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
/// - **Data size / KV update**: TX-level fallthrough is active in all specs. In Rex4+ it catches
///   intrinsic overflow (when the frame stack is empty) and serves as a safety net behind the
///   per-frame check.
/// - **State growth**: TX-level fallthrough catches Rex5 pre-frame authority usage and serves as a
///   safety net behind the per-frame check.
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
///
/// Additionally, this struct manages the `STORAGE_CALL_STIPEND` (Rex4+): extra gas granted to
/// value-transferring `CALL`/`CALLCODE` for storage operations. REX5+ tracks the stipend as a
/// separated internal allowance drained at the `storage_gas_ext` charging sites; REX4 retains
/// the legacy `gas.limit()` inflation with a per-frame compute gas cap and burn-on-return.
#[derive(Debug)]
pub struct AdditionalLimit {
    /// Carries the tx's current limit-check verdict.
    ///
    /// Once stamped to a non-[`LimitCheck::WithinLimit`] value, the sub-tracker pass in
    /// [`check_limit`](Self::check_limit) is bypassed and individual tracker usage values may
    /// no longer be reliable (subsequent frames revert and their discardable usage is dropped).
    /// Legitimate writers: [`check_limit`](Self::check_limit) (latches `ExceedsLimit`),
    /// [`mark_exempt`](Self::mark_exempt) (stamps `Exempt`),
    /// [`reset`](Self::reset) (clears to `WithinLimit`), and
    /// [`before_frame_return_result`](Self::before_frame_return_result) (absorbs a frame-local
    /// `ExceedsLimit` back to `WithinLimit`). Everything else reads via
    /// [`check_limit`](Self::check_limit) or
    /// [`exceeded_limit`](LimitCheck::exceeded_limit) / [`is_exempt`](LimitCheck::is_exempt).
    pub(crate) has_exceeded_limit: LimitCheck,

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

    /// A tracker for the `STORAGE_CALL_STIPEND` granted to value-transferring calls (REX4+).
    pub(crate) storage_call_stipend: storage_call_stipend::StorageCallStipendTracker,
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
            storage_call_stipend: storage_call_stipend::StorageCallStipendTracker::new(spec),
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
        self.storage_call_stipend.reset();
    }

    /// Test-only setter for [`has_exceeded_limit`](Self::has_exceeded_limit). Bypasses every
    /// invariant maintained by the normal write paths (sticky `Exempt`, sub-tracker latching,
    /// frame-local absorb). Integration tests use this to construct specific pre-latched states
    /// — production code must not.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn set_has_exceeded_limit_for_test(&mut self, state: LimitCheck) {
        self.has_exceeded_limit = state;
    }

    /// Marks the current transaction as exempt from `MegaETH` per-tx resource metering by stamping
    /// `has_exceeded_limit = LimitCheck::Exempt`. REX6+ uses this for system-originated
    /// transactions (see [`crate::is_system_originated`]); cleared by [`reset`](Self::reset).
    ///
    /// `Exempt` is sticky: [`check_limit`](Self::check_limit) short-circuits on it, so no later
    /// sub-tracker overflow can latch over it, and every direct read of `has_exceeded_limit`
    /// observes the exemption as "not exceeded" via
    /// [`exceeded_limit`](LimitCheck::exceeded_limit). The host storage-gas charging sites
    /// additionally consult [`is_exempt`](LimitCheck::is_exempt) to charge the SALT-unscaled
    /// cost, since SALT-scaled storage gas is charged to interpreter gas and is not tracked here.
    ///
    /// `current_call_remaining_*` queries (compute gas, data size, KV updates, state growth) still
    /// report `limit − usage` against the configured limit while exempt, but the limit is not
    /// enforced — so a caller that uses these values to make admission or sizing decisions for an
    /// exempt tx will get a number that is not load-bearing. Today's consumers
    /// (`MegaLimitControl.remainingComputeGas`, the `KeylessDeploy` sandbox sub-limits, oracle
    /// hint precompile) are unreachable from a system-originated tx; revisit this if a future
    /// system contract joins the mega whitelist and would consult them.
    #[inline]
    pub(crate) fn mark_exempt(&mut self) {
        self.has_exceeded_limit = LimitCheck::Exempt;
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

    /// Checks whether the Rex5 sandbox's TX-level pre-frame intrinsic usage fits inside
    /// `limits`.
    ///
    /// Runs a trial `AdditionalLimit` through the same entry points production uses —
    /// `before_tx_start` (data size / KV updates) and `record_compute_gas(initial_gas)`
    /// (intrinsic compute gas, via `MegaHandler::validate`) — then returns its `check_limit()`
    /// result. Reusing production logic keeps tracker changes and dimension-priority ordering
    /// in sync automatically. Consumed by the `KeylessDeploy` preflight.
    ///
    /// Any future TX-level persistent usage recorded before the first frame through a different
    /// path MUST be added here too when it can be computed from the transaction alone. DB-dependent
    /// contributions, such as REX5 EIP-7702 net-new authority state growth, are recorded during
    /// pre-execution once the journal is available. Missing additions do not fail open — the
    /// `KeylessDeploy` post-merge overflow check still catches residual overflow — but the failure
    /// mode degrades from the preflight fast-path (pre-sandbox revert with `ParentBudgetExceeded`)
    /// to an outer `OutOfGas` halt after sandbox setup has already run.
    pub(crate) fn intrinsic_check_for_tx(
        spec: MegaSpecId,
        tx: &MegaTransaction,
        limits: EvmTxRuntimeLimits,
    ) -> LimitCheck {
        debug_assert!(spec.is_enabled(MegaSpecId::REX5));
        let mut trial = Self::new(spec, limits);
        trial.before_tx_start(tx);

        let initial_and_floor_gas = calculate_initial_tx_gas_for_tx(tx, spec.into_eth_spec());
        trial.record_compute_gas(initial_and_floor_gas.initial_gas);

        trial.check_limit()
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
        self.storage_call_stipend.push_empty_frame();
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

    /// Returns the remaining data size budget for the current call frame.
    #[inline]
    pub fn current_call_remaining_data_size(&self) -> u64 {
        self.data_size.current_call_remaining()
    }

    /// Returns the remaining KV update budget for the current call frame.
    #[inline]
    pub fn current_call_remaining_kv_updates(&self) -> u64 {
        self.kv_update.current_call_remaining()
    }

    /// Returns the remaining state growth budget for the current call frame.
    #[inline]
    pub fn current_call_remaining_state_growth(&self) -> u64 {
        self.state_growth.current_call_remaining()
    }

    /// Returns the detained compute gas limit (independent of the natural TX limit).
    /// This is the limit set by volatile data access gas detention.
    #[inline]
    pub fn detained_compute_gas_limit(&self) -> u64 {
        self.compute_gas.detained_limit()
    }

    /// Returns the halt reason when gas detention is the binding compute gas constraint.
    /// Otherwise (detention was not more restrictive than the base TX limit), returns `None`.
    #[inline]
    pub(crate) fn detained_compute_gas_halt_reason(
        &self,
        access_type: VolatileDataAccess,
    ) -> Option<MegaHaltReason> {
        self.compute_gas.is_detained_exceed().then(|| MegaHaltReason::VolatileDataAccessOutOfGas {
            access_type,
            limit: self.compute_gas.detained_limit(),
            actual: self.compute_gas.tx_usage(),
        })
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
        // Sticky short-circuit: `Exempt` (REX6+ system-originated tx; usage is still accumulated
        // by individual trackers for `get_usage` and block-level accounting, only the halt
        // decision is suppressed) and already-latched `ExceedsLimit` both bypass the sub-tracker
        // pass. For `Exempt` this also neutralizes gas detention (which runs through
        // `compute_gas.check_limit()` below), so protocol-mandated execution can never halt on
        // metering — e.g. when SALT buckets grow. The standard EVM `gas_limit` remains the
        // runaway guard.
        if !self.has_exceeded_limit.within_limit() {
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

    /// `true` when a per-tx resource limit has already been latched as exceeded — the exact
    /// condition [`frame_result_if_exceeding_limit`](Self::frame_result_if_exceeding_limit) halts
    /// the transaction on. `WithinLimit` and `Exempt` both return `false`. Reads the latched
    /// aggregate; call [`check_limit`](Self::check_limit) first if a fresh evaluation is needed.
    #[inline]
    pub(crate) fn limit_exceeded(&self) -> bool {
        self.has_exceeded_limit.exceeded_limit()
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

    /// Records the current frame's remaining gas on a TX-level limit exceed so it can be
    /// refunded to the sender. The storage-stipend tracker decides how `gas.remaining()`
    /// maps to the refundable balance — see
    /// `StorageCallStipendTracker::effective_remaining_for_rescue`.
    pub(crate) fn rescue_gas(&mut self, gas: &Gas) {
        self.rescued_gas += self.storage_call_stipend.effective_remaining_for_rescue(gas);
    }

    /// Drains up to `amount` from the current frame's storage stipend allowance and
    /// returns the portion drained. Caller charges the residual via the original site's
    /// gas-charging macro. Returns 0 pre-REX5 (the legacy path covers storage via
    /// `gas.limit()` inflation).
    pub(crate) fn try_consume_storage_stipend(&mut self, amount: u64) -> u64 {
        self.storage_call_stipend.try_consume(amount)
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
    ///
    /// Records transaction-only intrinsic resource usage that can be computed from the
    /// transaction itself (calldata size, access lists, EIP-7702 authority account update
    /// footprint, caller account update, etc.) and checks TX-level limits.
    ///
    /// DB-dependent pre-frame usage is recorded later once the journal is available.
    /// In particular, REX5 EIP-7702 net-new authority state growth is accounted during
    /// pre-execution rather than here because `before_tx_start()` cannot tell whether an
    /// authority account already exists.
    ///
    /// If the recorded usage already exceeds a configured limit, sets `has_exceeded_limit`
    /// so that the subsequent `frame_result_if_exceeding_limit()` or `before_frame_init()`
    /// call produces a normal execution failure (Halt), keeping the failure on the standard
    /// additional-limit path.
    ///
    /// Intrinsic overflow detection works through each tracker's own `check_limit()`, which
    /// includes a TX-level fallthrough that catches `tx_usage > tx_limit` even when the frame
    /// stack is empty (before the first frame is pushed).
    pub(crate) fn before_tx_start(&mut self, tx: &MegaTransaction) {
        self.state_growth.before_tx_start(tx);
        self.data_size.before_tx_start(tx);
        self.kv_update.before_tx_start(tx);
        self.check_limit();
    }

    /// Records REX5 EIP-7702 authority accounts that are net-new state entries — the state-growth
    /// dimension only. Data size and KV updates for REX5 are charged upfront in `before_tx_start`
    /// for every authorization with a recoverable authority, independent of application.
    ///
    /// Runs in pre-execution after the authorization scan identifies net-new authorities and
    /// before revm writes the delegation bytecode; the net-new check needs DB / journal state,
    /// so this cannot live in `before_tx_start`.
    ///
    /// REX6+ replaces this with the per-applied-authority hook
    /// [`AdditionalLimit::on_rex6_eip7702_authority_applied`], which records all three resource
    /// dimensions in a single call.
    ///
    /// Latches any TX-level overflow into `has_exceeded_limit` via `check_limit`; the next frame
    /// boundary surfaces it as the normal execution failure.
    pub(crate) fn on_rex5_eip7702_authority_creations(&mut self, amount: u64) {
        self.state_growth.record_authority_creations(amount);
        self.check_limit();
    }

    /// Records the resource footprint of a single *applied* EIP-7702 authorization — one that
    /// passed the chain-id / `u64::MAX`-nonce / recoverable-authority / code gates and therefore
    /// writes the authority account — as TX-level persistent usage across all three dimensions.
    ///
    /// Every applied authorization writes the authority account (delegation code + nonce bump),
    /// so it always costs data size (+40) and a KV update (+1). A net-new authority account
    /// additionally counts as state growth (+1) — the caller passes `creates_authority` for that.
    /// The matching dynamic SALT account-creation gas is folded into `initial_gas` by the caller.
    ///
    /// REX5 splits the same accounting into two paths: data size / KV charged unconditionally in
    /// `before_tx_start` (covers skipped authorizations too), and state growth via
    /// [`AdditionalLimit::on_rex5_eip7702_authority_creations`]. REX6 consolidates them so only
    /// applied authorizations pay.
    pub(crate) fn on_rex6_eip7702_authority_applied(&mut self, creates_authority: bool) {
        self.data_size.record_persistent_account_write();
        self.kv_update.record_persistent_account_update();
        if creates_authority {
            self.state_growth.record_authority_creations(1);
        }
        self.check_limit();
    }

    /// Hook called before a new execution frame is initialized. Returns `Some(FrameResult)` if the
    /// limit is exceeded and the frame should terminate early with the returned `FrameResult`.
    ///
    /// For REX4+ value-transferring internal `CALL`/`CALLCODE`, this method also applies the
    /// `STORAGE_CALL_STIPEND`: it inflates `gas_limit`, caps the per-frame compute gas budget
    /// at the original gas limit, and pushes the stipend amount to the burn stack.
    pub(crate) fn before_frame_init<JOURNAL: JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        frame_init: &mut FrameInit,
        journal: &mut JOURNAL,
    ) -> Result<Option<FrameResult>, JOURNAL::DBError> {
        // Push new frame in frame limit trackers.
        self.state_growth.before_frame_init(frame_init, journal)?;
        self.data_size.before_frame_init(frame_init, journal)?;
        self.kv_update.before_frame_init(frame_init, journal)?;
        self.compute_gas.before_frame_init(frame_init, journal)?;

        // REX4+: detect value-transferring CALL/CALLCODE, inflate gas_limit, push stipend
        // to stack, and cap per-frame compute gas budget.
        self.storage_call_stipend.before_frame_init(frame_init, &mut self.compute_gas);

        if self.check_limit().exceeded_limit() {
            return Ok(self.create_exceeded_limit_result(&frame_init.frame_input));
        }

        Ok(None)
    }

    /// Checks whether a TX-level limit was already exceeded before the first frame starts
    /// (e.g., intrinsic `DataSize` or `KVUpdate` overflow from `before_tx_start()`).
    ///
    /// Called from two sites that would otherwise skip `before_frame_init()`:
    /// - `frame_init()` before system contract interceptor dispatch (REX4+).
    /// - `inspect_frame_init()` before inspector early-return (REX4+).
    ///
    /// Without this check, an intrinsic overflow would never be converted into a real
    /// failure and gas rescue would be missed.
    ///
    /// Returns `Some(FrameResult)` if a TX-level limit is already exceeded.
    pub(crate) fn frame_result_if_exceeding_limit(
        &mut self,
        frame_input: &FrameInput,
    ) -> Option<FrameResult> {
        if !self.limit_exceeded() {
            return None;
        }
        self.create_exceeded_limit_result(frame_input)
    }

    /// Creates a `FrameResult` for an exceeded limit and rescues remaining gas.
    ///
    /// Shared by `before_frame_init` (limit exceeded after pushing sub-tracker frames)
    /// and `frame_result_if_exceeding_limit` (intrinsic overflow before frame push).
    fn create_exceeded_limit_result(&mut self, frame_input: &FrameInput) -> Option<FrameResult> {
        let (gas_limit, return_memory_offset) = match frame_input {
            FrameInput::Call(inputs) => {
                (inputs.gas_limit, Some(inputs.return_memory_offset.clone()))
            }
            FrameInput::Create(inputs) => (inputs.gas_limit, None),
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
        Some(result)
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
                // Fast-path: a TX-level limit was latched earlier; pick it up without re-running
                // sub-tracker checks. Under `Exempt`, the predicate is false, so the exemption
                // passes through unchanged.
                if self.limit_exceeded() {
                    let output = self.has_exceeded_limit.revert_data();
                    mark_interpreter_result_as_exceeding_limit(
                        interpreter_result,
                        self.exceeding_instruction_result(),
                        output,
                    );
                    return;
                }

                // The sub-tracker `after_frame_run` calls above may have recorded new usage; run
                // a fresh check to catch overflow first detected at this frame end.
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

        // Pop stipend from stack and burn unused stipend (Rex4+).
        self.storage_call_stipend.before_frame_return_result::<LAST_FRAME>(result);

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
                // Gas should already have been rescued at the point where the limit was
                // exceeded (frame_result_if_exceeding_limit, before_frame_init,
                // after_frame_init, or after_frame_run).
                // Just mark the result as exceeding the limit.
                mark_frame_result_as_exceeding_limit(
                    result,
                    Self::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
                    Default::default(),
                );
            }
        }
    }

    /// Merges resource usage from a sandbox execution into this tracker.
    ///
    /// Used by `KeylessDeploy` (REX5+) to propagate sandbox resource consumption
    /// back to the parent transaction.
    pub(crate) fn merge_usage(&mut self, usage: LimitUsage) {
        self.compute_gas.merge_persistent_usage(usage.compute_gas);
        self.data_size.merge_persistent_usage(usage.data_size);
        self.kv_update.merge_persistent_usage(usage.kv_updates);
        self.state_growth.merge_persistent_usage(usage.state_growth);
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

    /// REX5+: record that the current transaction's caller account is being materialised
    /// by deposit pre-execution (mint balance increment, nonce bump, or both).
    ///
    /// Routes a `+1` to `state_growth`'s TX intrinsic lane only. Does **not** touch
    /// `data_size` or `kv_update`: their `before_tx_start` hooks already record the
    /// caller's account-info write unconditionally for every transaction (protocol-level
    /// definition: one caller account-info write per tx). Adding a second record here
    /// would double-count.
    ///
    /// Must be called exactly once per deposit-like transaction, only when the caller
    /// account is empty at validation time (before `OpHandler::pre_execution` runs).
    pub(crate) fn record_deposit_caller_creation(&mut self) {
        self.state_growth.record_deposit_caller_creation();
        let _ = self.check_limit();
    }

    /// REX5+: meter an oracle-hint payload against the TX data-size budget.
    ///
    /// Records `len` bytes into the TX intrinsic data-size lane (same lane as calldata) and
    /// runs `check_limit()` so `has_exceeded_limit` is flipped to a TX-level exceed if the
    /// recording overflows.
    ///
    /// Returns `true` if the recording stayed within the limit, `false` otherwise.
    ///
    /// **Caller contract**: on `false`, do NOT synthesize a result. Return `None` from the
    /// interceptor and let the next `frame_init` step (`before_frame_init` →
    /// `create_exceeded_limit_result`) produce the canonical TX-level `OutOfGas` halt with
    /// rescued gas. This keeps the failure shape identical to any other data-size overflow.
    pub(crate) fn record_oracle_hint_bytes(&mut self, len: u64) -> bool {
        self.data_size.record_oracle_hint_bytes(len);
        !self.check_limit().exceeded_limit()
    }

    /// Hook called when a log is written. Returns `false` if the limit has been exceeded.
    pub(crate) fn on_log(&mut self, num_topics: u64, data_size: u64) -> bool {
        self.state_growth.after_log(num_topics, data_size);
        self.data_size.after_log(num_topics, data_size);

        !self.check_limit().exceeded_limit()
    }

    /// Hook called after a SELFDESTRUCT on a same-TX-created account (REX4+).
    ///
    /// Records state growth refund for the destroyed account and its new storage slots.
    /// The caller is responsible for computing the total refund before calling this.
    pub(crate) fn on_selfdestruct(&mut self, refund: u64) {
        self.state_growth.after_selfdestruct(refund);
    }

    /// Records resource usage when SELFDESTRUCT creates a new beneficiary account (REX5+).
    ///
    /// Charges data size (+40 for account info write), KV update (+1), and state growth (+1).
    pub(crate) fn on_selfdestruct_new_account(&mut self) {
        // Account info write: same as DataSizeTracker's ACCOUNT_INFO_WRITE_SIZE (40 bytes)
        self.data_size.record_account_write();
        self.kv_update.record_account_update();
        self.state_growth.record_growth(1);
    }

    /// Records resource usage when SELFDESTRUCT transfers balance to an existing
    /// beneficiary account (REX6+).
    ///
    /// Charges data size (+40 for account info write) and KV update (+1). The target
    /// already exists, so no `StateGrowth` is recorded. SELFDESTRUCT does not push a call
    /// frame, so the `target_updated` dedup path in `FrameLimitTracker` never sees the
    /// balance write to an existing target — hence this dedicated hook.
    pub(crate) fn on_selfdestruct_existing_account(&mut self) {
        self.data_size.record_account_write();
        self.kv_update.record_account_update();
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

#[cfg(test)]
mod metering_exemption_tests {
    use super::*;

    /// Tiny per-dimension limits so a single recording trivially exceeds them.
    fn tiny_limits() -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits {
            tx_data_size_limit: 1,
            tx_kv_updates_limit: 1,
            tx_compute_gas_limit: 1,
            tx_state_growth_limit: 1,
            block_env_access_compute_gas_limit: u64::MAX,
            oracle_access_compute_gas_limit: u64::MAX,
        }
    }

    #[test]
    fn test_metering_enforced_when_not_exempt() {
        // Default (non-exempt): halted once compute gas exceeds the (tiny) limit. The REX6 ×
        // system-origin gate that would stamp `Exempt` lives in `MegaContext::on_new_tx`; here we
        // exercise the tracker directly.
        let mut al = AdditionalLimit::new(MegaSpecId::REX6, tiny_limits());
        assert!(!al.record_compute_gas(1_000_000), "non-exempt tx must report exceeded limit");
        assert!(al.check_limit().exceeded_limit());
    }

    #[test]
    fn test_metering_bypassed_when_exempt() {
        // When the system-tx exemption is stamped, `check_limit` short-circuits on the sticky
        // `Exempt` state, covering the four dimensions *and* gas detention, so the same
        // over-limit usage never halts. Usage is still recorded (only the halt decision is
        // suppressed).
        let mut al = AdditionalLimit::new(MegaSpecId::REX6, tiny_limits());
        al.mark_exempt();
        assert!(al.record_compute_gas(1_000_000), "exempt tx must not report exceeded limit");
        assert!(!al.check_limit().exceeded_limit());
        assert!(al.check_limit().is_exempt(), "check_limit must surface the sticky Exempt state");
        assert!(al.get_usage().compute_gas >= 1_000_000, "usage is still accumulated while exempt");
    }

    #[test]
    fn test_detained_compute_gas_does_not_halt_when_exempt() {
        // Gas detention lowers the detained compute-gas limit; enforcement runs through the same
        // `check_limit` chokepoint, so the exemption neutralizes detention too.
        let mut al =
            AdditionalLimit::new(MegaSpecId::REX6, EvmTxRuntimeLimits::from_spec(MegaSpecId::REX6));
        al.mark_exempt();
        al.set_compute_gas_limit(1); // detain hard
        assert!(al.record_compute_gas(10_000_000), "exempt tx must ignore gas detention");
        assert!(!al.check_limit().exceeded_limit());
    }

    #[test]
    fn test_reset_clears_exempt() {
        // The sticky `Exempt` state must not leak to the next transaction reusing the same tracker.
        let mut al = AdditionalLimit::new(MegaSpecId::REX6, tiny_limits());
        al.mark_exempt();
        assert!(al.has_exceeded_limit.is_exempt());
        al.reset();
        assert!(!al.has_exceeded_limit.is_exempt(), "reset must clear the sticky Exempt state");
        assert!(
            al.has_exceeded_limit.within_limit(),
            "reset must restore the WithinLimit baseline"
        );
        assert!(!al.record_compute_gas(1_000_000), "after reset, metering is enforced again");
    }
}
