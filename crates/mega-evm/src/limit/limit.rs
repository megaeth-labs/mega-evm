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
    checkpoint, compute_gas, data_size, frame_limit::TxRuntimeLimit, kv_update, state_growth,
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

    /// A tracker for REX7+ checkpoint settlement and gas-clamp state.
    pub(crate) checkpoint: checkpoint::CheckpointTracker,
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
            checkpoint: checkpoint::CheckpointTracker::new(spec),
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
        self.checkpoint.reset();
    }

    /// Whether compute gas settles at checkpoints (REX7+) rather than per opcode.
    #[inline]
    pub(crate) fn rex7_enabled(&self) -> bool {
        self.checkpoint.rex7_enabled()
    }

    /// Interpreter gas remaining at the start of the current unsettled segment.
    ///
    /// Settlement sites that need to subtract their own storage gas or forwarded child gas read
    /// this instead of a per-opcode `gas_before` capture, so the measured delta covers every
    /// unwrapped plain opcode executed since the previous checkpoint.
    #[inline]
    pub(crate) fn checkpoint_baseline(&self) -> u64 {
        self.checkpoint.baseline()
    }

    /// Re-opens the settlement window at `remaining`, without recording anything.
    ///
    /// Used by settlement sites that compute their own segment amount; every such site must
    /// call this once it has recorded, so a later settlement cannot bill the segment twice.
    #[inline]
    pub(crate) fn sync_checkpoint_baseline(&mut self, remaining: u64) {
        self.checkpoint.sync_baseline(remaining);
    }

    /// Moves the open segment's baseline down by `amount` of `MegaETH` storage gas just charged to
    /// the interpreter, so the charge sits outside the segment rather than inside it.
    ///
    /// A checkpoint body normally subtracts its own storage charge when it closes its measurement
    /// window. A body that aborts — a static-context `LOG`, a `SELFDESTRUCT` whose inner
    /// instruction runs out of gas — never reaches that subtraction, and the frame-exit settlement
    /// that follows would then bill the charge as compute. Excluding it from the baseline as it is
    /// charged makes the exclusion hold on both paths; on the normal path the body's own window
    /// re-syncs the baseline afterwards, so this is invisible there.
    ///
    /// No-op before REX7, where nothing measures against a baseline.
    #[inline]
    pub(crate) fn exclude_storage_gas_from_segment(&mut self, amount: u64) {
        self.checkpoint.exclude_storage_gas_from_segment(amount);
    }

    /// Takes the outstanding clamp so the caller can hand its hidden gas back to the interpreter,
    /// returning that amount.
    ///
    /// Every checkpoint prologue calls this before running its body, and the frame's final result
    /// calls it before the result propagates, so the clamp is never observable outside a plain
    /// segment.
    #[inline]
    pub(crate) fn checkpoint_restore_hidden(&mut self) -> u64 {
        self.checkpoint.restore_hidden()
    }

    /// Applies the gas clamp for the segment that starts at `remaining`, and returns the amount
    /// the caller must debit from the interpreter's counter.
    ///
    /// The clamp is recorded — and the segment therefore enforces the compute limit — whenever the
    /// true remaining reaches the compute headroom, including when the two are exactly equal and
    /// nothing needs to be hidden. When the frame's own gas would run out first, no clamp is
    /// recorded: an out-of-gas in that segment is the EVM's own, and reclassifying it as a compute
    /// exceed would rescue gas the transaction never had a claim to.
    ///
    /// Records nothing and returns 0 when clamping does not apply: the transaction is exempt from
    /// per-tx metering, or a limit has already been latched (the enclosing site halts on it
    /// instead).
    #[inline]
    pub(crate) fn checkpoint_clamp_amount(&mut self, remaining: u64) -> u64 {
        debug_assert!(!self.checkpoint.has_clamp(), "clamp applied while a clamp is outstanding");
        if !self.has_exceeded_limit.within_limit() {
            return 0;
        }
        let binding = self.compute_gas.clamp_binding();
        let Some(hidden) = remaining.checked_sub(binding.headroom) else {
            return 0;
        };
        self.checkpoint.set_clamp(hidden, binding);
        hidden
    }

    /// Latches a clamp-induced out-of-gas as a compute gas limit exceed.
    ///
    /// The crossing opcode never executed — revm's own gas check stopped it at the clamp boundary —
    /// so its cost is not in the recorded usage and an ordinary [`check_limit`](Self::check_limit)
    /// pass sees usage at or below the limit. The latch is therefore stamped directly, from the
    /// constraint that bound the clamp: `frame_local` decides the shape the existing frame-result
    /// machinery produces (frame-local absorb to revert; TX-level mark plus gas rescue), and the
    /// constraint's own `limit` is what that shape reports — the sub-frame budget for a frame-local
    /// binding, the effective TX limit otherwise, matching what the non-clamp check path writes.
    #[inline]
    fn latch_clamp_exceed(&mut self, binding: &compute_gas::ClampBinding) {
        if !self.has_exceeded_limit.within_limit() {
            return;
        }
        self.has_exceeded_limit = LimitCheck::ExceedsLimit {
            kind: super::LimitKind::ComputeGas,
            frame_local: binding.frame_local,
            limit: binding.limit,
            used: self.compute_gas.tx_usage(),
        };
        // Preserve the volatile-detention attribution: when the binding TX-level constraint at
        // clamp time was the detained limit, the halt must classify as `VolatileDataAccessOutOfGas`
        // exactly as per-opcode enforcement classifies it.
        self.checkpoint.set_latched_detained(
            !binding.frame_local &&
                self.compute_gas.detained_limit() < self.compute_gas.base_tx_limit(),
        );
    }

    /// Finalises what the frame's own result decides about the clamp: restores any outstanding
    /// clamp into the result's gas and latches a clamp-induced out-of-gas as the compute exceed it
    /// stands for.
    ///
    /// Must run before anything reads or charges the result's gas — in particular before the
    /// execution-layer code-deposit storage charge, which would otherwise observe the clamped copy
    /// and mis-fire an out-of-gas on a CREATE frame that is nowhere near its limits. It is also
    /// what puts the result's gas into the true domain, which is where the destroyed remainder of
    /// an exceptionally halted frame is later read from.
    ///
    /// A clamp can only be outstanding when the frame ended inside a plain-opcode segment, because
    /// every checkpoint prologue takes it before its body. An out-of-gas exit from such a segment
    /// is a clamp artifact: the true counter held `hidden` more gas than the interpreter could see,
    /// and the crossing opcode was stopped at the clamp boundary *before executing* — exactly the
    /// gas-clamp enforcement point. When the crossing opcode would have exceeded the true
    /// remaining as well, the compute classification still wins: the two are indistinguishable
    /// here, and attributing the halt to the resource limit keeps the sender's remaining gas
    /// refundable.
    pub(crate) fn settle_frame_final_result(&mut self, result: &mut InterpreterResult) {
        if !self.checkpoint.rex7_enabled() {
            return;
        }
        if let Some(clamp) = self.checkpoint.take_clamp() {
            result.gas.erase_cost(clamp.hidden);
            // `MemoryOOG` is the same gas shortage reported from the memory-expansion path; every
            // other result either is unrelated to gas or cannot arise from a plain opcode.
            if matches!(result.result, InstructionResult::OutOfGas | InstructionResult::MemoryOOG) {
                self.latch_clamp_exceed(&clamp.binding);
            }
        }
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

    /// The part of [`get_usage`](Self::get_usage)'s `compute_gas` that exceptionally halted frames
    /// destroyed rather than performed (REX7+, always 0 before).
    ///
    /// Reported and accounted like the rest of the total, never enforced. A caller that merges
    /// this transaction's usage into another tracker — today the `KeylessDeploy` sandbox boundary —
    /// has to carry it alongside the total, or the receiving tracker re-enforces gas the EVM
    /// already destroyed.
    #[inline]
    pub(crate) fn burned_compute_gas(&self) -> u64 {
        self.compute_gas.burned_usage()
    }

    /// Records a destroyed remainder into the non-enforcing compute-gas lane (REX7+).
    ///
    /// Raises the reported total and leaves every limit comparison unchanged — the same
    /// [`ComputeGasTracker::record_burned_gas`](compute_gas::ComputeGasTracker::record_burned_gas)
    /// the interpreter-frame halt path uses. Precompile halt accounting calls this at the
    /// recording site rather than through frame-exit settlement: a halt `Gas` is reset to
    /// `Gas::new(limit)`, so the frame formula `remaining()` would double-count or miss the
    /// forwarded-cap gap.
    #[inline]
    pub(crate) fn record_burned_gas(&mut self, amount: u64) {
        self.compute_gas.record_burned_gas(amount);
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
        trial.record_compute_gas(initial_and_floor_gas.initial_regular_gas);

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
        // `is_detained_exceed` covers per-opcode enforcement, where usage crossed the detained
        // limit. `latched_detained` covers gas-clamp enforcement, where the crossing opcode
        // was stopped before executing and usage therefore stays at or below the limit.
        (self.compute_gas.is_detained_exceed() || self.checkpoint.latched_detained()).then(|| {
            MegaHaltReason::VolatileDataAccessOutOfGas {
                access_type,
                limit: self.compute_gas.detained_limit(),
                actual: self.compute_gas.tx_usage(),
            }
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
    ///
    /// This runs on every metered opcode, so it is the hottest hook in the whole tracker.
    /// `#[inline]` lets the record + within-limit check fold directly into the per-opcode
    /// wrapper, removing a call across the `RefMut<AdditionalLimit>` boundary.
    ///
    /// Surfacing an exceed here is what turns a latched non-compute overflow into a halt, so
    /// the call sites are also the positions a halt can land on: every metered opcode under
    /// per-opcode accounting, and every checkpoint under checkpoint accounting.
    #[inline]
    pub(crate) fn record_compute_gas(&mut self, compute_gas_used: u64) -> bool {
        self.record_compute_gas_impl::<true>(compute_gas_used)
    }

    /// Records the compute gas used without the latch-protocol guard.
    ///
    /// The guard in [`record_compute_gas_impl`](Self::record_compute_gas_impl) asserts that no
    /// non-compute dimension is over limit without having latched, which holds at every position
    /// an opcode can record from. It does not hold at a frame's final settlement: a pre-inner
    /// recorder whose opcode then failed (SELFDESTRUCT's beneficiary accounting) deliberately
    /// leaves its usage unlatched, and the frame is about to pop and discard it. Recording it
    /// through the guarded entry point would trip the assert on that path.
    #[inline]
    pub(crate) fn record_compute_gas_unguarded(&mut self, compute_gas_used: u64) -> bool {
        self.record_compute_gas_impl::<false>(compute_gas_used)
    }

    #[inline]
    fn record_compute_gas_impl<const GUARD_LATCH_PROTOCOL: bool>(
        &mut self,
        compute_gas_used: u64,
    ) -> bool {
        // Record unconditionally, even when another dimension has already latched an exceed:
        // the compute work was performed, and the recorded total feeds the transaction outcome
        // and block-level compute accounting. Skipping the record would under-report compute
        // usage for transactions halted on a non-compute dimension (e.g. intrinsic data size
        // latched in `before_tx_start` before `validate` records the initial gas).
        self.compute_gas.record_gas_used(compute_gas_used);
        // Sticky short-circuit, mirroring `check_limit`: an already-latched `ExceedsLimit` is
        // surfaced immediately, and `Exempt` (REX6+ system-originated tx) suppresses the halt
        // decision — including the compute-gas / detained check below — while the recording
        // above still feeds `get_usage` and block-level accounting.
        if !self.has_exceeded_limit.within_limit() {
            return !self.has_exceeded_limit.exceeded_limit();
        }
        // Debug-only guard for the latch protocol: the compute-only fast path below is sound
        // only if every non-compute mutation site already latched its own exceed. If a
        // non-compute dimension is over limit but not yet latched, some mutation site is missing
        // its `check_limit()` — catch it here in tests, not in production. The sub-tracker
        // `check_limit()` calls are non-mutating, so this compiles out of release builds. The one
        // pre-inner recorder, SELFDESTRUCT, routes through `record_compute_gas_all_dims`, not this
        // method, so it never trips this; the frame-final settlement, which can observe that same
        // recorder's usage after its opcode failed, opts out via `GUARD_LATCH_PROTOCOL`.
        debug_assert!(
            !GUARD_LATCH_PROTOCOL ||
                (!self.data_size.check_limit().exceeded_limit() &&
                    !self.kv_update.check_limit().exceeded_limit() &&
                    !self.state_growth.check_limit().exceeded_limit()),
            "non-compute limit exceeded without latching: a mutation site is missing check_limit()",
        );
        // Recording compute gas can only change the compute-gas dimension, so check just that one
        // (`compute_gas.check_limit()` covers both the Rex4+ per-frame budget and the TX-level
        // detained limit) instead of fanning out to all four sub-trackers. The other three
        // dimensions only change at their own mutation sites (`on_sstore`, `on_log`,
        // `record_oracle_hint_bytes`, the frame-lifecycle hooks), each of which runs
        // `check_limit()` itself and latches any exceed into `has_exceeded_limit` — which the
        // short-circuit above then surfaces here. The one exception is SELFDESTRUCT's pre-inner
        // `on_selfdestruct_new_account` / `on_selfdestruct_existing_account`, which deliberately
        // do not latch; their dimensions latch in the trailing `record_compute_gas_all_dims`.
        let check = self.compute_gas.check_limit();
        if check.exceeded_limit() {
            self.has_exceeded_limit = check;
            return false;
        }
        true
    }

    /// Records the compute gas used and checks ALL four limit dimensions (the
    /// pre-optimization fan-out), returning `false` if any has been exceeded.
    ///
    /// Retained for SELFDESTRUCT: its REX5 storage wrapper records beneficiary
    /// data/KV/state usage *before* the inner instruction runs, without latching
    /// (the inner instruction may still fail, in which case the frame pops that
    /// discardable usage). The fan-out here latches those dimensions only after the
    /// inner instruction has succeeded, with `check_limit`'s dimension priority.
    /// Hot-path opcodes use [`Self::record_compute_gas`] instead.
    #[inline]
    pub(crate) fn record_compute_gas_all_dims(&mut self, compute_gas_used: u64) -> bool {
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
            FrameInput::Create(inputs) => (inputs.gas_limit(), None),
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
        frame: &mut EthFrame<EthInterpreter>,
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

        // Checkpoint accounting: apply the gas clamp and open the settlement window at the
        // frame's clamped gas. This hook runs both at frame entry and at every resume after a child
        // frame's outcome — including the gas it returned — has been merged back into this frame's
        // interpreter, so the window always starts at an instruction boundary with the
        // interpreter's counter in its real, post-merge state. No clamp can be outstanding
        // here: every suspension point (the CALL / CREATE checkpoint prologue) and every
        // frame end restores it first.
        if self.checkpoint.rex7_enabled() {
            debug_assert!(!self.checkpoint.has_clamp(), "frame resumed with a clamp outstanding");
            let hide = self.checkpoint_clamp_amount(frame.interpreter.gas.remaining());
            if hide > 0 {
                let clamped = frame.interpreter.gas.record_regular_cost(hide);
                debug_assert!(clamped, "clamp amount exceeds remaining gas");
            }
            self.checkpoint.sync_baseline(frame.interpreter.gas.remaining());
        }
        None
    }

    /// Hook called after frame action processing in `frame_run`.
    ///
    /// Records compute gas cost induced in frame action processing (e.g., code deposit cost),
    /// marks the frame result as exceeding limit if needed, settles an exceptionally halted
    /// frame's destroyed remainder (REX7+), and rescues gas if a TX-level limit was exceeded
    /// (before any inspector callback that might modify gas).
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
        self.settle_exceptional_halt_burn(result);
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
        // Checkpoint accounting: the frame has produced its final action, so settle the tail
        // segment — everything since the last checkpoint — against the interpreter's gas counter.
        // `frame.interpreter.gas` still holds the loop-exit value here (the clamp restore and the
        // code-deposit storage charge both mutate only the action's gas copy), and both it and the
        // baseline live in the same clamped domain, so the delta telescopes over exactly the
        // unwrapped plain opcodes that ran since. A checkpoint that already settled and halted
        // leaves `baseline == remaining` (delta 0), and a CALL abort path's forwarded-gas
        // `erase_cost` can only raise `remaining` above the baseline, which the saturation turns
        // into 0. Any exceed recorded here is latched, and the frame result marking below / in
        // `before_frame_return_result` surfaces it. The clamp restore itself already happened, in
        // `settle_frame_final_result`, before the execution-layer hook charged code-deposit storage
        // gas against the action's gas.
        //
        // This delta is the work the frame *performed*, so it settles the same way — through the
        // enforcing path — however the frame ended. A frame that halts exceptionally still ran the
        // opcodes ahead of its failure, and a parent frame keeps executing after absorbing that
        // failure; leaving the executed tail out of enforcement would let the code after the failed
        // frame spend the same headroom a second time. What such a frame additionally destroys —
        // the budget it never gets to spend — is settled after action processing, outside
        // enforcement, by `settle_exceptional_halt_burn`.
        if self.checkpoint.rex7_enabled() {
            if let InterpreterAction::Return(_) = action {
                let remaining = frame.interpreter.gas.remaining();
                let gas_used = self.checkpoint.take_segment(remaining);
                let _ = self.record_compute_gas_unguarded(gas_used);
                self.refresh_latched_compute_usage();
            }
        }

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
        // remaining gas returns to the caller. This works at any depth including the
        // top-level frame.
        //
        // The rewrite changes the reported result, not the journal. revm decides
        // commit-or-revert from the frame's original instruction result, before the
        // `FrameResult` ever reaches this hook, so a frame that ran to a successful exit is
        // already committed and stays committed under the rewritten Revert.
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

    /// Re-reads a latched TX-level compute exceed's usage from the tracker (REX7+).
    ///
    /// A clamp-induced exceed is latched at the frame's final result, before the frame-exit
    /// settlement closes the plain segment the crossing opcode stopped inside. The latch is sticky,
    /// so the halt reason built later from [`check_limit`](Self::check_limit) would otherwise
    /// report the usage as it stood one settlement short of final — which is not the number the
    /// transaction's compute total ends on. The detention path never had this problem: it rebuilds
    /// its halt reason from live tracker usage.
    ///
    /// Only TX-level exceeds are refreshed. A frame-local exceed's `used` is the frame's own
    /// figure, which the frame-local revert payload does not carry, so rewriting it with a
    /// transaction-level total would only blur what it means.
    #[inline]
    fn refresh_latched_compute_usage(&mut self) {
        let usage = self.compute_gas.tx_usage();
        if let LimitCheck::ExceedsLimit {
            kind: super::LimitKind::ComputeGas,
            frame_local: false,
            used,
            ..
        } = &mut self.has_exceeded_limit
        {
            *used = usage;
        }
    }

    /// Settles the remainder an exceptionally halted frame destroys, as non-enforcing compute gas
    /// (REX7+).
    ///
    /// An exceptional halt returns no gas: the top-level frame's whole envelope is spent by the
    /// transaction's final gas accounting, and an inner frame's remainder is simply never handed
    /// back to its caller. The interpreter zeroes its own counter only for a plain `OutOfGas`, so
    /// the frame-exit delta cannot see that destroyed budget on any other classification. The
    /// result's own gas can: by the time this runs,
    /// [`settle_frame_final_result`](Self::settle_frame_final_result) has handed back whatever the
    /// clamp was hiding and the code-deposit storage charge has been taken, so
    /// `result.gas().remaining()` is exactly what the frame still held and will not get to keep.
    ///
    /// Runs **after** action processing, which is the first point the classification is final:
    /// revm's create-return can still turn a successful constructor into a canonical code-deposit
    /// out-of-gas, an EIP-3541 reject or a runtime code-size reject, and each of those destroys the
    /// frame's remainder just like a halt from the interpreter loop.
    ///
    /// Only the destroyed part goes to the tracker's non-enforcing lane — the work performed ahead
    /// of the failure already settled through the enforcing path in
    /// [`after_frame_run_instructions`](Self::after_frame_run_instructions). Enforcing the
    /// destroyed part would turn an ordinary EVM halt into a resource-limit failure with the
    /// remaining gas rescued for the sender, which is exactly the receipt change the
    /// exceptional-halt carve-out forbids.
    ///
    /// Not reached when a resource limit is already latched: that path destroys nothing, because
    /// the frame either reverts to its parent (frame-local) or halts the transaction with its gas
    /// rescued (TX-level) — including a clamp-induced out-of-gas, which
    /// [`settle_frame_final_result`](Self::settle_frame_final_result) latches earlier in this
    /// frame exit.
    fn settle_exceptional_halt_burn(&mut self, result: &FrameResult) {
        if !self.checkpoint.rex7_enabled() ||
            self.limit_exceeded() ||
            result.instruction_result().is_ok_or_revert()
        {
            return;
        }
        self.compute_gas.record_burned_gas(result.gas().remaining());
    }

    /// Merges resource usage from a sandbox execution into this tracker.
    ///
    /// Used by `KeylessDeploy` (REX5+) to propagate sandbox resource consumption
    /// back to the parent transaction.
    ///
    /// `burned_compute_gas` is the part of `usage.compute_gas` the sandbox destroyed rather than
    /// performed (REX7+, always 0 before). It is already inside the merged total, so it is only
    /// reclassified here — the parent reports it and never enforces it, exactly as the sandbox
    /// did. Merging it as ordinary usage instead would let a sandbox frame's ordinary EVM halt
    /// fail the outer transaction on a resource limit.
    pub(crate) fn merge_usage(&mut self, usage: LimitUsage, burned_compute_gas: u64) {
        self.compute_gas.merge_persistent_usage(usage.compute_gas);
        self.compute_gas.merge_burned_usage(burned_compute_gas);
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

#[cfg(test)]
mod tests {
    use revm::context::tx::TxEnvBuilder;

    use super::{super::LimitKind, *};
    use crate::VolatileDataAccess;

    fn test_limits() -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits {
            tx_data_size_limit: 100,
            tx_kv_updates_limit: 1_000,
            tx_compute_gas_limit: 1_000_000,
            tx_state_growth_limit: 1_000,
            block_env_access_compute_gas_limit: 1_000_000,
            oracle_access_compute_gas_limit: 1_000_000,
        }
    }

    /// Returns the latched limit kind, or `None` when within limit.
    fn latched_kind(limit: &AdditionalLimit) -> Option<LimitKind> {
        match limit.has_exceeded_limit {
            LimitCheck::ExceedsLimit { kind, .. } => Some(kind),
            LimitCheck::WithinLimit | LimitCheck::Exempt => None,
        }
    }

    /// Compute gas must be recorded even when another dimension has already latched an
    /// exceed: the work was performed, and the recorded total feeds the transaction outcome
    /// and block-level compute accounting. A skipped record would under-report compute usage
    /// for transactions halted on a non-compute dimension.
    ///
    /// This pins the `validate()` path: `before_tx_start` latches intrinsic data-size
    /// overflow before any frame exists, then the initial gas is recorded.
    #[test]
    fn test_record_compute_gas_records_after_other_dimension_latched() {
        let mut limit = AdditionalLimit::new(MegaSpecId::REX5, test_limits());

        // Latch the data-size dimension at its mutation site: intrinsic transaction data
        // (110-byte base + 200 bytes of calldata) exceeds the 100-byte limit.
        let tx = crate::MegaTransaction(op_revm::OpTransaction::new(
            TxEnvBuilder::new()
                .caller(Address::ZERO)
                .call(Address::ZERO)
                .data(vec![0u8; 200].into())
                .build_fill(),
        ));
        limit.before_tx_start(&tx);
        assert_eq!(latched_kind(&limit), Some(LimitKind::DataSize));

        // The trailing compute-gas record of the same opcode must still surface the latched
        // exceed AND record the gas.
        assert!(!limit.record_compute_gas(5_000), "latched exceed must surface as false");
        assert_eq!(
            limit.get_usage().compute_gas,
            5_000,
            "compute gas must be recorded even after another dimension latched",
        );

        // The latched kind is preserved (not overwritten by the compute-gas check).
        assert_eq!(latched_kind(&limit), Some(LimitKind::DataSize));
    }

    #[test]
    fn test_rex5_authority_creation_latches_state_growth_exceed() {
        let mut limits = test_limits();
        limits.tx_state_growth_limit = 1;
        let mut limit = AdditionalLimit::new(MegaSpecId::REX5, limits);

        limit.on_rex5_eip7702_authority_creations(2);

        assert_eq!(
            latched_kind(&limit),
            Some(LimitKind::StateGrowth),
            "authority creation accounting must latch its TX-level state-growth exceed",
        );
    }

    /// SELFDESTRUCT's beneficiary usage is recorded *before* the inner instruction runs and
    /// must NOT latch at the recording site: the inner instruction can still fail (out of
    /// gas, DB error), in which case the frame pops the discardable usage and a latch taken
    /// at recording time would stick and misattribute the failure. The latch belongs to the
    /// trailing all-dimension check, which only runs after the inner instruction succeeds.
    #[test]
    fn test_selfdestruct_usage_latches_only_at_trailing_check() {
        let mut limits = test_limits();
        // Below the 40-byte account-info write, so the beneficiary creation exceeds it.
        limits.tx_data_size_limit = 10;
        let mut limit = AdditionalLimit::new(MegaSpecId::REX5, limits);
        // Simulate an active call frame (SELFDESTRUCT always runs inside one).
        limit.push_empty_frame();

        // Recording site: usage recorded, but no latch yet (inner instruction may still fail).
        limit.on_selfdestruct_new_account();
        assert_eq!(latched_kind(&limit), None, "recording site must not latch");

        // Trailing all-dimension check (runs only after inner success): latches and halts.
        assert!(!limit.record_compute_gas_all_dims(100), "trailing check must surface exceed");
        assert_eq!(latched_kind(&limit), Some(LimitKind::DataSize));
    }

    /// `intrinsic_check_for_tx` is a REX5-only preflight (EIP-7702 authority growth);
    /// its `debug_assert!(spec.is_enabled(REX5))` precondition must reject a pre-REX5
    /// spec. Calling it at REX4 must trip the assert.
    ///
    /// Kills the spec-gate mutant `is_enabled(MegaSpecId::REX5) -> is_enabled(REX4)` at
    /// limit.rs:203: under the mutant the precondition would silently accept REX4 and the
    /// call would return a `LimitCheck` instead of panicking.
    #[test]
    #[should_panic(expected = "REX5")]
    #[cfg(debug_assertions)]
    fn test_intrinsic_check_for_tx_requires_rex5_spec() {
        let tx = crate::MegaTransaction(op_revm::OpTransaction::new(
            TxEnvBuilder::new().caller(Address::ZERO).call(Address::ZERO).build_fill(),
        ));
        // REX4 < REX5: the precondition assert must fire.
        let _ = AdditionalLimit::intrinsic_check_for_tx(MegaSpecId::REX4, &tx, test_limits());
    }

    /// Builds a successful `FrameResult::Call` carrying `gas_limit` gas.
    fn stopped_call_result(gas_limit: u64) -> FrameResult {
        FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(InstructionResult::Stop, Bytes::new(), Gas::new(gas_limit)),
            0..0,
        ))
    }

    /// A frame that returns while a TX-level limit is latched must have its result rewritten to
    /// the exceeding instruction result, otherwise a transaction that blew its budget would be
    /// reported as a plain success.
    ///
    /// The `duplicate_return_frame_result` guard must suppress the rewrite only for the *second*
    /// top-level invocation, which is distinguished by an already-empty tracker frame stack.
    /// Here the frame is still on the stack, so the rewrite is required even with
    /// `LAST_FRAME == true`.
    #[test]
    fn test_before_frame_return_result_marks_latched_tx_level_exceed() {
        let mut limits = test_limits();
        limits.tx_data_size_limit = 100;
        // MINI_REX is pre-Rex4, so the exceed is TX-level (not frame-local) and must halt.
        let mut limit = AdditionalLimit::new(MegaSpecId::MINI_REX, limits);
        limit.push_empty_frame();
        assert!(!limit.on_log(4, 1_000), "an oversized log must latch a data-size exceed");

        let mut result = stopped_call_result(1_000);
        limit.before_frame_return_result::<true>(&mut result);
        assert_eq!(
            result.instruction_result(),
            AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
            "a returning frame with a latched TX-level exceed must be marked as exceeding",
        );
    }

    /// The duplicate top-level invocation (frame stack already emptied by the first one) must
    /// leave the result untouched — it is the same result object the first call already handled.
    #[test]
    fn test_before_frame_return_result_skips_duplicate_top_level_call() {
        let mut limits = test_limits();
        limits.tx_data_size_limit = 100;
        let mut limit = AdditionalLimit::new(MegaSpecId::MINI_REX, limits);
        limit.push_empty_frame();
        assert!(!limit.on_log(4, 1_000));

        let mut result = stopped_call_result(1_000);
        // First call pops the frame and marks the result.
        limit.before_frame_return_result::<true>(&mut result);
        // Reset the result to observe whether the duplicate call would mark it again.
        let mut duplicate = stopped_call_result(1_000);
        limit.before_frame_return_result::<true>(&mut duplicate);
        assert_eq!(
            duplicate.instruction_result(),
            InstructionResult::Stop,
            "the duplicate top-level invocation must not re-handle the result",
        );
    }

    fn oog_result() -> InterpreterResult {
        InterpreterResult::new(InstructionResult::OutOfGas, Bytes::new(), Gas::new(100_000))
    }

    fn rex7_limit() -> AdditionalLimit {
        AdditionalLimit::new(MegaSpecId::REX7, EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7))
    }

    /// A frame-local clamp exceed while detention is active must not be attributed to
    /// detention: the child reverts with `MegaLimitExceeded`, and `latched_detained` stays
    /// clear so a later halt cannot be rewritten as `VolatileDataAccessOutOfGas`.
    ///
    /// Kills `&&` → `||` in `latch_clamp_exceed`: the `||` would fire on the detained-limit
    /// arm alone and stamp the frame-local exceed as detained.
    #[test]
    fn test_latch_clamp_exceed_frame_local_with_detention_is_not_detained() {
        let mut limit = rex7_limit();
        limit.set_compute_gas_limit(1);
        assert!(
            limit.compute_gas.detained_limit() < limit.compute_gas.base_tx_limit(),
            "the fixture must actually tighten detention"
        );
        limit.checkpoint.set_clamp(
            50,
            compute_gas::ClampBinding { headroom: 100, frame_local: true, limit: 1_000 },
        );

        limit.settle_frame_final_result(&mut oog_result());

        assert!(
            limit.has_exceeded_limit.is_frame_local(),
            "the exceed must stay frame-local; got {:?}",
            limit.has_exceeded_limit
        );
        assert!(
            !limit.checkpoint.latched_detained(),
            "a frame-local clamp must not inherit detention attribution"
        );
        assert!(
            limit.detained_compute_gas_halt_reason(VolatileDataAccess::TIMESTAMP).is_none(),
            "frame-local + detention must not classify as VolatileDataAccessOutOfGas"
        );
    }

    /// A TX-level clamp exceed when detention did not tighten (`detained_limit == base`)
    /// must stay a compute-gas halt, not `VolatileDataAccessOutOfGas`.
    ///
    /// Kills `<` → `<=` in `latch_clamp_exceed`: at equality the `<=` mutant stamps
    /// `latched_detained` and the halt-reason remap would blame detention.
    #[test]
    fn test_latch_clamp_exceed_tx_level_without_tightened_detention_is_not_detained() {
        let mut limit = rex7_limit();
        assert_eq!(
            limit.compute_gas.detained_limit(),
            limit.compute_gas.base_tx_limit(),
            "the fixture is the detained == base knife edge"
        );
        let tx_limit = limit.compute_gas.base_tx_limit();
        limit.checkpoint.set_clamp(
            50,
            compute_gas::ClampBinding { headroom: 100, frame_local: false, limit: tx_limit },
        );

        limit.settle_frame_final_result(&mut oog_result());

        assert!(
            !limit.checkpoint.latched_detained(),
            "detained_limit == base_tx_limit must not count as a detained clamp"
        );
        assert!(
            limit.detained_compute_gas_halt_reason(VolatileDataAccess::TIMESTAMP).is_none(),
            "a TX-level clamp with no tightened detention must not classify as \
             VolatileDataAccessOutOfGas"
        );
    }

    /// The same `AdditionalLimit` is reused across transactions (`on_new_tx` calls `reset`).
    /// Leftover checkpoint state from a detained clamp halt must not pollute the next
    /// transaction's halt classification.
    #[test]
    fn test_reset_clears_checkpoint_so_the_next_tx_is_not_classified_as_detained() {
        let mut limit = rex7_limit();
        limit.set_compute_gas_limit(1);
        limit.checkpoint.sync_baseline(99_999);
        limit.checkpoint.set_clamp(
            50,
            compute_gas::ClampBinding { headroom: 100, frame_local: false, limit: 1 },
        );
        limit.settle_frame_final_result(&mut oog_result());
        assert!(
            limit.checkpoint.latched_detained(),
            "TX1's detained clamp must stamp the attribution flag"
        );
        assert!(
            limit.detained_compute_gas_halt_reason(VolatileDataAccess::TIMESTAMP).is_some(),
            "TX1 must classify as a detained halt"
        );

        limit.reset();

        assert_eq!(limit.checkpoint.baseline(), 0, "reset must drop TX1's baseline");
        assert!(
            limit.checkpoint.take_clamp().is_none(),
            "reset must drop any clamp TX1 left behind"
        );
        assert!(
            !limit.checkpoint.latched_detained(),
            "reset must drop TX1's detention-attribution flag"
        );
        assert!(
            limit.detained_compute_gas_halt_reason(VolatileDataAccess::TIMESTAMP).is_none(),
            "TX2 must not inherit TX1's VolatileDataAccessOutOfGas attribution"
        );
    }

    /// `mark_frame_result_as_exceeding_limit` rewrites both frame-result variants in place.
    #[test]
    fn test_mark_frame_result_as_exceeding_limit_rewrites_both_variants() {
        let output = Bytes::from_static(b"over");

        let mut call = stopped_call_result(50);
        mark_frame_result_as_exceeding_limit(
            &mut call,
            InstructionResult::OutOfGas,
            output.clone(),
        );
        let FrameResult::Call(call_outcome) = &call else { panic!("call frame result") };
        assert_eq!(call_outcome.result.result, InstructionResult::OutOfGas);
        assert_eq!(call_outcome.result.output, output);

        let mut create = FrameResult::Create(CreateOutcome::new(
            InterpreterResult::new(InstructionResult::Stop, Bytes::new(), Gas::new(50)),
            None,
        ));
        mark_frame_result_as_exceeding_limit(
            &mut create,
            InstructionResult::OutOfGas,
            output.clone(),
        );
        let FrameResult::Create(create_outcome) = &create else { panic!("create frame result") };
        assert_eq!(create_outcome.result.result, InstructionResult::OutOfGas);
        assert_eq!(create_outcome.result.output, output);
    }
}
