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
    checkpoint, compute_gas, conservation, data_size, destroyed, frame_limit::TxRuntimeLimit,
    inspector_ledger, kv_update, state_growth, storage_call_stipend,
};
use crate::{
    EvmTxRuntimeLimits, JournalInspectTr, MegaHaltReason, MegaSpecId, MegaTransaction,
    VolatileDataAccess,
};

use super::LimitCheck;

/// How a frame reached the outcome [`AdditionalLimit::finalize_frame`] is settling.
///
/// The three shapes differ in what the frame's remaining gas means and in which of them the
/// settlement is allowed to reach at all, so the caller names the shape rather than the settlement
/// guessing it from the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameExit {
    /// The frame ran and produced its own result.
    Ran,
    /// The frame was refused before it could run, by a path that went through the limit tracker's
    /// frame-init accounting: a resource limit already over its budget, or one of revm's own
    /// frame-init rejections.
    Refused,
    /// The frame was refused by a synthetic result that never reached that accounting — a system
    /// contract interceptor's, or an inspector's.
    ///
    /// Frozen specs leave such a refusal's envelope entirely alone, which is the gap REX4's
    /// pre-dispatch limit check was added to narrow and which REX7 closes: without a settlement
    /// here, an envelope that is neither handed back nor booked as destroyed leaves the
    /// conservation law short by exactly that amount.
    RefusedSynthetically,
}

/// What a precompile's recording site knows about its call, held until the frame's settlement
/// point can decide the split (REX7+).
///
/// A precompile is answered inside the frame init and never becomes a child frame, so its
/// recording site is the only place that knows both of these numbers: the envelope is the
/// caller-supplied forwarded amount rather than the REX5-capped effective limit, and the work is
/// `MegaETH`'s own price for what the call performed, which a halting precompile's gas object does
/// not carry. What that site cannot know is how the call ends — an inspector's `call_end` runs
/// afterwards and can rewrite the classification, and the classification is what decides whether
/// the caller reclaims the remainder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrecompileEnvelope {
    /// The gas the caller forwarded, uncapped.
    forwarded: u64,
    /// The work the precompile performed, already recorded on the enforcing lane.
    executed: u64,
}

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

    /// What the inspector — if any — did to this transaction's gas accounting.
    ///
    /// Written only by the measurement shim every inspector is wrapped in, and read only by the
    /// conservation law and by reporting. It stays at its default for every transaction that runs
    /// without an inspector and for every observation-only inspector.
    inspector: inspector_ledger::InspectorLedger,

    /// The precompile call whose split is waiting for its frame's settlement point (REX7+).
    ///
    /// At most one can ever be outstanding: a precompile is answered inside a frame init and the
    /// same frame init settles the result a few statements later, with no room for another frame
    /// to start in between. [`finalize_frame`](Self::finalize_frame) takes it unconditionally, so
    /// it cannot outlive the frame that staged it.
    staged_precompile: Option<PrecompileEnvelope>,

    /// Gas an inspector wrote into a *terminating* pending action, waiting for the frame that
    /// action ends to reach its settlement point.
    ///
    /// Staged rather than booked for the same reason a frame result's edit is: the action becomes
    /// the frame's result, and whether an edit to it moves anything depends on the classification
    /// the caller ends up seeing. Only one frame can have one outstanding — a frame that has set
    /// its terminating action starts no more children — and
    /// [`finalize_frame`](Self::finalize_frame) takes it.
    staged_action_result_gas: i128,

    /// Gas an inspector wrote into a *suspending* pending action, waiting for the frame-start
    /// callback of the child it is about to build.
    ///
    /// That callback is the first point at which the edit can be told apart from an interception,
    /// and it runs immediately after the action is handed on, with nothing in between that could
    /// stage another one.
    staged_action_env_gas: i128,

    /// The envelope a callback that answered a frame itself was handed, waiting for the
    /// settlement point of the frame it answered.
    ///
    /// An interception produces a whole frame result out of nothing, so the reading its
    /// settlement needs is not a difference the shim can take — it is this baseline against the
    /// gas the result turns out to carry. At most one can be outstanding: revm stops at the
    /// first callback that answers, and the frame init that asked settles a few statements later.
    staged_interception_envelope: Option<u64>,
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
            inspector: inspector_ledger::InspectorLedger::default(),
            staged_precompile: None,
            staged_action_result_gas: 0,
            staged_action_env_gas: 0,
            staged_interception_envelope: None,
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
        self.inspector = inspector_ledger::InspectorLedger::default();
        self.staged_precompile = None;
        self.staged_action_result_gas = 0;
        self.staged_action_env_gas = 0;
        self.staged_interception_envelope = None;
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
    ///
    /// The same charge is also the canonical funnel for the transaction's in-frame `MegaETH`
    /// storage gas, so it feeds the non-compute lane the destroyed-remainder derivation reads.
    #[inline]
    pub(crate) fn exclude_storage_gas_from_segment(&mut self, amount: u64) {
        self.checkpoint.exclude_storage_gas_from_segment(amount);
        self.checkpoint.record_non_compute_gas(i128::from(amount));
    }

    /// Records EVM gas the transaction spends that is neither compute work nor a destroyed
    /// remainder (REX7+).
    ///
    /// The in-frame storage-gas charges arrive through
    /// [`exclude_storage_gas_from_segment`](Self::exclude_storage_gas_from_segment); this is the
    /// entry point for the contributions that are charged outside an open settlement segment —
    /// the `MegaETH` share of intrinsic gas, the code-deposit storage charge, the `KeylessDeploy`
    /// interceptor's caller-materialisation charge, and the sandbox boundary's residue.
    #[inline]
    pub(crate) fn record_non_compute_gas(&mut self, amount: i128) {
        self.checkpoint.record_non_compute_gas(amount);
    }

    /// The terms of the transaction's gas conservation law, as they stand right now — see
    /// [`ConservationTerms`](conservation::ConservationTerms), which states the law and both of
    /// its rearrangements.
    ///
    /// Every site that derives, re-settles or checks a transaction's gas accounting reads the law
    /// from here, so the law exists once and the terms cannot drift apart between the places that
    /// use them.
    ///
    /// Meaningful once the transaction's envelope is final. Read earlier, the terms are simply
    /// the partial totals recorded so far — and the envelope a caller solves the law against must
    /// be read at the one moment it is final too, after the resource-limit rescue has been handed
    /// back and before `post_execution` applies the EIP-3529 refund and the EIP-7623 floor. Gas
    /// that is rescued for the sender and gas the clamp was hiding are both erased from the
    /// envelope before that point, so neither can reach the subtraction.
    #[inline]
    pub fn conservation_terms(&self) -> conservation::ConservationTerms {
        conservation::ConservationTerms {
            enforced_compute_gas: self.enforced_compute_gas(),
            non_compute_gas: self.non_compute_gas(),
            minted_call_stipend: self.minted_call_stipend(),
            inspector_conjured_gas: self.inspector_conjured_gas(),
            booked_destroyed_compute_gas: self.burned_compute_gas(),
        }
    }

    /// The `CALL_STIPEND` total this transaction's value-transferring calls minted into their
    /// child frames — the term that keeps recorded compute gas from being a partition of what the
    /// transaction spent. Always 0 before REX7.
    #[inline]
    pub(crate) fn minted_call_stipend(&self) -> u64 {
        self.checkpoint.minted_call_stipend()
    }

    /// Settles the transaction's destroyed compute gas from the conservation law and stores it as
    /// the number the transaction reports (REX7+; a no-op before, where nothing is destroyed).
    ///
    /// `tx_gas_spent` must be the envelope the transaction actually burnt, read at the one moment
    /// it is final: after op-revm has normalised the gas object and the resource-limit rescue has
    /// been handed back, and before `post_execution` applies the EIP-3529 refund and the EIP-7623
    /// floor. Those two move the number the receipt reports without anybody having burnt the
    /// difference, so reading after them would fold a refund into the destroyed total. Gas that is
    /// rescued for the sender and gas the clamp was hiding are both erased from the envelope
    /// before this point, so neither can reach the subtraction either.
    ///
    /// The per-site destroyed bookings do not feed this number. They stay as the independent
    /// second opinion the `debug_assert` below cross-checks the derivation against, so a site that
    /// destroys an envelope without booking it — or a spend the non-compute lane does not know
    /// about — still fails loudly in debug builds and in the test corpus.
    ///
    /// A negative derivation is defended against rather than expected: it would mean the recorded
    /// compute and non-compute lanes together claim more gas than the transaction spent, which no
    /// spec produces today. Debug builds trip on it; release builds clamp to zero so a reporting
    /// defect cannot wrap into an enormous destroyed total.
    #[inline]
    pub(crate) fn settle_destroyed_compute_gas(&mut self, tx_gas_spent: u64) {
        if !self.rex7_enabled() {
            return;
        }
        let terms = self.conservation_terms();
        let derived = terms.destroyed_for(tx_gas_spent);
        debug_assert!(
            derived >= 0,
            "derived destroyed compute gas is negative: {derived} (spent {tx_gas_spent}, {terms})",
        );
        debug_assert!(
            terms.unbooked_for(tx_gas_spent) == 0,
            "destroyed compute gas disagrees with the conservation law: derived {derived} \
             (spent {tx_gas_spent}, {terms})",
        );
        let settled = u64::try_from(derived.max(0)).unwrap_or(u64::MAX);
        self.checkpoint.set_settled_destroyed(settled);
    }

    /// Settles a transaction whose reported envelope is rewritten after every `MegaETH`
    /// settlement has already run (REX7+; a no-op before, where nothing is destroyed).
    ///
    /// One such rewrite exists. An OP deposit is not allowed to fail, so a deposit that does fail
    /// has its receipt rebuilt to report the whole `gas_limit`, with the journal rolled back to
    /// nothing but the nonce bump and the mint. That rebuild happens at the outermost error
    /// boundary, past every site that records or settles, so neither the per-site bookings nor
    /// [`settle_destroyed_compute_gas`](Self::settle_destroyed_compute_gas) can see it. Two
    /// shapes arrive here:
    ///
    /// - A validation reject, which never reached a settlement at all. Its lanes hold only what
    ///   `validate` recorded before returning the error, and the whole rest of the rewritten
    ///   envelope is unaccounted.
    /// - An execution halt, which settled correctly against the envelope it really burnt and is
    ///   then raised back to `gas_limit`. The gap is exactly what the resource-limit rescue had
    ///   handed back to the sender, which the rewrite takes away again.
    ///
    /// Both are the same accounting event: the receipt burns an envelope that nothing was
    /// executed for. So the difference between what the conservation law derives for the rewritten
    /// envelope and what the per-site bookings already hold is destroyed compute gas. Booking it
    /// makes the reported total cover the receipt; re-settling against the rewritten envelope
    /// keeps the derived total and the bookings agreeing, which is what the cross-check in
    /// `settle_destroyed_compute_gas` verifies.
    ///
    /// Enforcement is deliberately untouched. [`record_burned_gas`](Self::record_burned_gas)
    /// raises the reported total and the destroyed lane by the same amount, so
    /// [`enforced_compute_gas`](Self::enforced_compute_gas) — what every limit comparison and the
    /// block's admission counter read — does not move. A deposit rejected before it executed
    /// anything must not consume block compute capacity for work it never performed.
    ///
    /// The difference is non-negative on every shape that reaches here. The rewritten envelope is
    /// the transaction's `gas_limit`. A rejected deposit's lanes hold at most the intrinsic gas
    /// requirement it had already cleared against that limit when the reject fired, and nothing is
    /// booked as destroyed yet. A halted deposit settled against the same limit less whatever the
    /// resource-limit rescue returned, so raising the envelope back to the limit can only add.
    ///
    /// The one shape that would break that is a synthetic pre-frame halt which books a destroyed
    /// remainder without settling — it would leave the `MegaETH` share of intrinsic gas booked as
    /// non-compute *and* the whole envelope booked as destroyed, double-counting it. No spec with
    /// the destroyed lane reaches one: an intrinsic overrun has been a validation reject since
    /// REX5. A spec that re-opened that path would have to settle it at its own site. Debug builds
    /// trip on a negative difference; release builds book nothing for it.
    #[inline]
    pub(crate) fn settle_rewritten_envelope(&mut self, envelope_gas_spent: u64) {
        if !self.rex7_enabled() {
            return;
        }
        let terms = self.conservation_terms();
        let unbooked = terms.unbooked_for(envelope_gas_spent);
        debug_assert!(
            unbooked >= 0,
            "rewritten envelope destroys a negative amount: {unbooked} \
             (envelope {envelope_gas_spent}, {terms})",
        );
        self.record_burned_gas(u64::try_from(unbooked.max(0)).unwrap_or(u64::MAX));
        self.settle_destroyed_compute_gas(envelope_gas_spent);
    }

    /// The transaction's destroyed compute gas, as settled by
    /// [`settle_destroyed_compute_gas`](Self::settle_destroyed_compute_gas) — the part of
    /// [`get_usage`](Self::get_usage)'s `compute_gas` that is reported and accounted but never
    /// enforced.
    ///
    /// This is the reporting answer, and the only one a caller outside this module should use.
    /// [`burned_compute_gas`](Self::burned_compute_gas) is the per-site booking that backs the
    /// transaction's own enforcement and cross-checks this derivation; the two agree, and reading
    /// the wrong one would silently pick the wrong side of that check.
    #[inline]
    pub(crate) fn destroyed_compute_gas(&self) -> u64 {
        self.checkpoint.settled_destroyed()
    }

    /// Books one `CALL_STIPEND` minted into a child invocation that the caller never funded
    /// (REX7+).
    ///
    /// Called from the CALL-family settlement once the opcode has handed the invocation on, which
    /// is where the mint is created — not once a child frame runs. A frame init that then fails on
    /// balance or call depth refunds the whole child budget, mint included, to the caller, so the
    /// envelope shrinks against recorded work by exactly one stipend just as a child that ran and
    /// returned it would. The one path that mints nothing is the compute-limit abort, which
    /// discards the pending child and returns its forwarded gas before the EVM sees it.
    #[inline]
    pub(crate) fn record_minted_call_stipend(&mut self, amount: u64) {
        self.checkpoint.record_minted_call_stipend(amount);
    }

    /// What the inspector did to this transaction's gas accounting, as measured at the callback
    /// boundaries — see [`InspectorLedger`](inspector_ledger::InspectorLedger).
    ///
    /// Default (all-zero) for every transaction that ran without an inspector and for every
    /// observation-only inspector. Cumulative over the whole transaction: a caller that wants the
    /// aggregate over one frame, or over any other window, reads this at both ends of the window
    /// and takes the difference.
    #[inline]
    pub fn inspector_ledger(&self) -> inspector_ledger::InspectorLedger {
        self.inspector
    }

    /// The net gas the inspector conjured — the term
    /// [`ConservationTerms`](conservation::ConservationTerms) adds to the envelope so that gas
    /// nobody funded does not read as the transaction having spent less than it did.
    #[inline]
    pub(crate) fn inspector_conjured_gas(&self) -> i128 {
        self.inspector.conjured_gas()
    }

    /// Books an adjustment an inspector made to a live interpreter's gas counter, and restores
    /// correct accounting and enforcement around it.
    ///
    /// This is the single entry point for interpreter-counter adjustments. `remaining_before` is
    /// the counter the shim snapshotted before delegating to the user's callback, and
    /// `gas.remaining()` is what the callback left behind; the difference is the adjustment,
    /// because the EVM does not execute inside a callback.
    ///
    /// `reaches_envelope` is whether the counter the callback left behind is one the EVM will read
    /// again — false exactly when the interpreter is already holding a terminating action, whose
    /// own copy of the counter is what the caller reclaims from. It gates the ledger and
    /// nothing else: an edit nobody will read moves no gas and must not be booked, but
    /// `MegaETH`'s own tail settlement does read this counter after the action is set, so the
    /// baseline still has to shift or the edit would be measured as work the frame performed.
    ///
    /// Three things happen, in this order:
    ///
    /// 1. **The ledger** takes the adjustment, if it can reach the envelope at all, so the
    ///    conservation law can account for gas nobody funded (or gas that vanished) when it derives
    ///    the destroyed remainder.
    /// 2. **The open segment is settled against the pre-callback counter** (REX7+,
    ///    `IN_OPEN_SEGMENT`). This is what keeps the adjustment out of enforcement: compute gas is
    ///    measured as a drop in the interpreter's counter, so an injection made mid-segment would
    ///    otherwise show up as *less* work than the frame performed — the frame would have been
    ///    handed free compute headroom. Closing the segment at `remaining_before` and re-opening it
    ///    at the adjusted counter measures exactly the work, and nothing else.
    /// 3. **The gas clamp is re-derived** from the freshly settled usage, exactly as a checkpoint's
    ///    epilogue does. Without this, an injection would be spendable past the compute headroom:
    ///    the clamp hides gas beyond the headroom from the interpreter, and gas written in after
    ///    the clamp was applied is not hidden by it.
    ///
    /// `IN_OPEN_SEGMENT` is false at `initialize_interp`, the one callback that runs after a frame
    /// is built but before its settlement window is opened. There is no segment to settle and no
    /// clamp to re-derive there; the frame's own entry hook opens the window on the adjusted
    /// counter a moment later, which absorbs the adjustment for free.
    ///
    /// The settlement records through the unguarded entry point for the same reason the frame-exit
    /// tail settlement does: a callback can run immediately after an opcode whose pre-inner
    /// recorder deliberately left a non-compute dimension unlatched, and the latch-protocol guard
    /// would trip on it.
    ///
    /// A settlement that latches an exceed does not stop the interpreter here — a callback has no
    /// way to fail an instruction. The latch is sticky, so the next checkpoint or the frame's own
    /// exit surfaces it as it would have anyway; the adjustment only moves *when* the halt lands,
    /// never whether it does.
    pub(crate) fn record_inspector_gas_adjustment<const IN_OPEN_SEGMENT: bool>(
        &mut self,
        gas: &mut Gas,
        remaining_before: u64,
        reaches_envelope: bool,
    ) {
        let remaining_after = gas.remaining();
        if remaining_after == remaining_before {
            return;
        }
        if reaches_envelope {
            self.inspector.gas.book(i128::from(remaining_after) - i128::from(remaining_before));
        }

        if !IN_OPEN_SEGMENT || !self.rex7_enabled() {
            return;
        }

        // Close the open segment against the counter as the EVM left it, so the adjustment sits
        // outside the measured span. Both the baseline and `remaining_before` live in the clamped
        // domain, so the difference telescopes over exactly the opcodes that ran since the last
        // checkpoint.
        let segment = self.checkpoint_baseline().saturating_sub(remaining_before);
        let hidden = self.checkpoint_restore_hidden();
        gas.erase_cost(hidden);
        self.sync_checkpoint_baseline(gas.remaining());
        let _ = self.record_compute_gas_unguarded(segment);

        // Re-derive the clamp for the segment that starts now, from the usage just settled.
        let hide = self.checkpoint_clamp_amount(gas.remaining());
        if hide > 0 {
            let clamped = gas.record_regular_cost(hide);
            debug_assert!(clamped, "clamp amount exceeds remaining gas");
            self.sync_checkpoint_baseline(gas.remaining());
        }
    }

    /// Books an adjustment an inspector made to a frame's envelope — the `gas_limit` the frame is
    /// about to be built with.
    ///
    /// The caller's `CALL` / `CREATE` opcode debited the forwarded amount before any inspector
    /// callback ran, so raising the limit hands the child gas the transaction never paid for, and
    /// lowering it makes gas the caller paid for reach nobody. Either way the transaction's
    /// envelope no longer matches the work its frames recorded, and the conservation law needs the
    /// difference.
    ///
    /// Call this only when the adjusted inputs actually reach a frame. A callback that returns a
    /// synthetic outcome has intercepted the frame, and the inputs it edited are dropped without
    /// being read.
    #[inline]
    pub(crate) fn record_inspector_env_adjustment(&mut self, delta: i128) {
        self.inspector.env.book(delta);
    }

    /// Stages an adjustment an inspector made to the gas a *terminating* pending action carries.
    ///
    /// The action is the object that becomes the frame's result, so this is the same measurement
    /// as an edit made at the frame's last callback, taken one step earlier — and it is settled at
    /// the same place, [`finalize_frame`](Self::finalize_frame), for the same reason: whether the
    /// edit moves anything at all depends on the classification the caller ends up seeing.
    #[inline]
    pub(crate) fn stage_inspector_action_result_adjustment(&mut self, delta: i128) {
        self.staged_action_result_gas += delta;
    }

    /// Stages an adjustment an inspector made to the gas a *suspending* pending action carries —
    /// the envelope the child frame is about to be built with.
    ///
    /// Same lane as an edit made at the frame-start callback, taken one step earlier, and booked
    /// there: the shim takes it back out at that callback, which is the first point that can tell
    /// the edit apart from an interception.
    #[inline]
    pub(crate) fn stage_inspector_action_env_adjustment(&mut self, delta: i128) {
        self.staged_action_env_gas += delta;
    }

    /// Takes the staged suspending-action adjustment, for the frame-start callback to book.
    #[inline]
    pub(crate) fn take_inspector_action_env_adjustment(&mut self) -> i128 {
        core::mem::take(&mut self.staged_action_env_gas)
    }

    /// Stages the envelope a callback that answered a frame itself was handed.
    ///
    /// The number recorded is the gas limit as that callback *received* it, not as it left it.
    /// That is the envelope the transaction actually funded: the caller's `CALL` / `CREATE`
    /// opcode debited it, and any edit an earlier callback made to it on the way here was booked
    /// on the envelope lane as it was made. An edit the answering callback itself makes is
    /// deliberately not part of the baseline — see
    /// [`record_inspector_env_adjustment`](Self::record_inspector_env_adjustment) for why it
    /// reaches no frame, and note that whatever of it survives into the result the caller is
    /// handed is measured here instead, as part of that result.
    #[inline]
    pub(crate) fn stage_inspector_interception_envelope(&mut self, envelope: u64) {
        self.staged_interception_envelope = Some(envelope);
    }

    /// Takes the staged interception envelope, for the frame init that asked to settle against.
    #[inline]
    pub(crate) fn take_inspector_interception_envelope(&mut self) -> Option<u64> {
        self.staged_interception_envelope.take()
    }

    /// Books an adjustment an inspector made to a pending action the same callback then removed,
    /// leaving the frame to carry on from its own counter.
    ///
    /// With no action left there is nothing for the edit to travel in, so it lands where the
    /// frame's remaining budget already lives — the same lane a counter edit takes, and for the
    /// same reason: the frame will spend what it now holds.
    #[inline]
    pub(crate) fn record_inspector_action_counter_adjustment(&mut self, delta: i128) {
        self.inspector.gas.book(delta);
    }

    /// Books an adjustment an inspector made to a refund counter — see
    /// [`InspectorLedger::refund`](inspector_ledger::InspectorLedger::refund).
    ///
    /// Booked and nothing else: no limit reads it, the conservation law has no term for it, and
    /// the transaction's gas accounting is unmoved by it. Its one consumer is
    /// [`InspectorLedger::is_zero`](inspector_ledger::InspectorLedger::is_zero), which is what the
    /// canonical block path asks before admitting a transaction — and a refund is what the sender
    /// pays, so a receipt an inspector moved this way has to be refused like any other.
    #[inline]
    pub(crate) fn record_inspector_refund_adjustment(&mut self, delta: i128) {
        self.inspector.refund.book(delta);
    }

    /// Books the EIP-8037 state-gas dimension a transaction ends holding, at the one point it is
    /// final — see [`InspectorLedger::reservoir`](inspector_ledger::InspectorLedger::reservoir).
    ///
    /// Both numbers are structurally zero on every `MegaETH` path and every spec: EIP-8037 is off,
    /// so no instruction charges state gas, no site fills a reservoir, and nothing here fires for
    /// a transaction that ran without a rewriting inspector. What is non-zero is therefore the
    /// inspector's in whole, which is why this reads the final figures rather than differencing
    /// two readings the way every other lane does.
    ///
    /// Call this after op-revm has normalised the top-level gas object and before the destroyed
    /// remainder is settled: the reservoir is what the settlement's envelope has to be reduced by,
    /// and the conservation law reads the lane back out of that envelope.
    #[inline]
    pub(crate) fn record_inspector_state_gas_dimension(
        &mut self,
        reservoir: u64,
        state_gas_spent: i64,
    ) {
        self.inspector.reservoir.book(i128::from(reservoir));
        self.inspector.state_gas.book(i128::from(state_gas_spent));
    }

    /// Counts one rewrite the shim refused because its shape is forbidden — see
    /// [`InspectorLedger::rejected_rewrites`](inspector_ledger::InspectorLedger::rejected_rewrites).
    #[inline]
    pub(crate) fn record_inspector_rejected_rewrite(&mut self) {
        self.inspector.rejected_rewrites = self.inspector.rejected_rewrites.saturating_add(1);
    }

    /// Counts one rewrite that changes what the execution did rather than what it cost — see
    /// [`InspectorLedger::interventions`](inspector_ledger::InspectorLedger::interventions).
    #[inline]
    pub(crate) fn record_inspector_intervention(&mut self) {
        self.inspector.interventions = self.inspector.interventions.saturating_add(1);
    }

    /// The EVM gas the transaction has spent that is neither compute work nor destroyed (REX7+,
    /// always 0 before) — the second term of
    /// [`ConservationTerms`](conservation::ConservationTerms).
    #[inline]
    pub(crate) fn non_compute_gas(&self) -> i128 {
        self.checkpoint.non_compute_gas()
    }

    /// The compute gas the transaction claims to have performed: the reported total less the
    /// destroyed remainders — the third term of
    /// [`ConservationTerms`](conservation::ConservationTerms), and the number every compute-gas
    /// limit comparison runs against.
    #[inline]
    pub(crate) fn enforced_compute_gas(&self) -> u64 {
        self.compute_gas.enforced_tx_usage()
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

    /// The destroyed remainders the per-site bookings recorded, summed as they happened (REX7+,
    /// always 0 before) — **not** the number the transaction reports.
    ///
    /// This is the sum that separates the recorded compute total into the work every limit is
    /// evaluated against and the remainder none of them sees, so it is what the transaction's own
    /// enforcement runs on, and what a tracker merging this transaction's usage — today the
    /// `KeylessDeploy` sandbox boundary — must carry alongside the total, or the receiving tracker
    /// re-enforces gas the EVM already destroyed.
    ///
    /// It is also the second opinion the settlement point's `debug_assert` holds the derivation
    /// to. For the reported destroyed total use
    /// [`destroyed_compute_gas`](Self::destroyed_compute_gas).
    #[inline]
    pub(crate) fn burned_compute_gas(&self) -> u64 {
        self.compute_gas.burned_usage()
    }

    /// Records a destroyed remainder into the non-enforcing compute-gas lane (REX7+).
    ///
    /// Raises the reported total and leaves every limit comparison unchanged — the same
    /// [`ComputeGasTracker::record_burned_gas`](compute_gas::ComputeGasTracker::record_burned_gas)
    /// the interpreter-frame halt path uses.
    #[inline]
    pub(crate) fn record_burned_gas(&mut self, amount: u64) {
        self.compute_gas.record_burned_gas(amount);
    }

    /// Hands the precompile recording site's two numbers to the frame's settlement point (REX7+;
    /// a no-op before, where nothing is destroyed).
    ///
    /// `executed` must already have been recorded on the enforcing lane — it is the work the call
    /// performed, which does not depend on how the call is classified afterwards. Only the
    /// destroyed half does, and that is what the settlement point derives from these two numbers
    /// and the final classification. See [`PrecompileEnvelope`].
    #[inline]
    pub(crate) fn stage_precompile_envelope(&mut self, forwarded: u64, executed: u64) {
        if !self.rex7_enabled() {
            return;
        }
        debug_assert!(
            self.staged_precompile.is_none(),
            "a precompile's envelope outlived the frame init that staged it",
        );
        self.staged_precompile = Some(PrecompileEnvelope { forwarded, executed });
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

    /// [`check_limit`](Self::check_limit) as it will read once the returning frame has been
    /// popped and merged into its caller — asked before the merge, and latching nothing.
    ///
    /// A per-frame budget is defined by the frame's usage weighed against its *caller's* budget
    /// after the merge, so that is the only question worth asking at a frame return. Asked where
    /// its numbers naturally appear — after the pop — it comes too late for the answer to change
    /// what the merge did with the frame's usage or what the journal did with its state. This asks
    /// the same question one step earlier, which is the whole of why it exists.
    ///
    /// "The same question" is meant literally: every dimension runs its own `check_limit` body,
    /// in `check_limit`'s order, over a reading of its tracker taken as if the pop had happened.
    /// The only thing that differs is where the numbers come from, and
    /// [`FrameLimitTracker::view_after_pop`](super::FrameLimitTracker::view_after_pop) is the
    /// single place that computes them. `before_frame_return_result` cross-checks the two readings
    /// against each other on every frame return in debug builds.
    ///
    /// `success` is the merge the pop would perform — the returning frame's classification as it
    /// stands when this is asked, before anything this answer causes rewrites it.
    pub(crate) fn peek_check_limit_after_pop(&self, success: bool) -> LimitCheck {
        // Sticky short-circuit, mirroring `check_limit`: a latched exceed or an exemption is what
        // that pass would return, whatever the sub-trackers hold.
        if !self.has_exceeded_limit.within_limit() {
            return self.has_exceeded_limit;
        }

        let data_size_check = self.data_size.check_limit_after_pop(success);
        if data_size_check.exceeded_limit() {
            return data_size_check;
        }

        let kv_update_check = self.kv_update.check_limit_after_pop(success);
        if kv_update_check.exceeded_limit() {
            return kv_update_check;
        }

        let compute_gas_check = self.compute_gas.check_limit_after_pop(success);
        if compute_gas_check.exceeded_limit() {
            return compute_gas_check;
        }

        let state_growth_check = self.state_growth.check_limit_after_pop(success);
        if state_growth_check.exceeded_limit() {
            return state_growth_check;
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
    pub(crate) fn rescue_gas(&mut self, gas: &Gas, remaining: u64) {
        self.rescued_gas +=
            self.storage_call_stipend.effective_remaining_for_rescue(gas, remaining);
    }

    /// Drains up to `amount` from the current frame's storage stipend allowance and
    /// returns the portion drained. Caller charges the residual via the original site's
    /// gas-charging macro. Returns 0 pre-REX5 (the legacy path covers storage via
    /// `gas.limit()` inflation).
    pub(crate) fn try_consume_storage_stipend(&mut self, amount: u64) -> u64 {
        self.storage_call_stipend.try_consume(amount)
    }

    /// Rescues a frame's remaining gas for the sender if a TX-level additional limit has been
    /// exceeded, and refunds it in `last_frame_result`.
    ///
    /// `remaining` is the gas the EVM left in the result, which is not always the number the
    /// result now carries: an inspector callback runs between the two, and a callback that spends
    /// the result down — the shape `GasInspector` takes on an error — must not be able to take
    /// the sender's refund with it. Every frame the transaction unwinds through rescues the part
    /// of the envelope it was still holding, and those parts are disjoint, so the sum is the whole
    /// of what the halted transaction never spent.
    pub(crate) fn try_rescue_gas(&mut self, gas: &Gas, remaining: u64) {
        let limit_check = self.check_limit();
        if limit_check.exceeded_limit() && !limit_check.is_frame_local() {
            self.rescue_gas(gas, remaining);
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
        &self,
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
    fn create_exceeded_limit_result(&self, frame_input: &FrameInput) -> Option<FrameResult> {
        let (gas_limit, return_memory_offset) = match frame_input {
            FrameInput::Call(inputs) => {
                (inputs.gas_limit, Some(inputs.return_memory_offset.clone()))
            }
            FrameInput::Create(inputs) => (inputs.gas_limit(), None),
            FrameInput::Empty => unreachable!(),
        };
        let output = self.has_exceeded_limit.revert_data();
        // The gas this result carries is rescued in `finalize_frame`, along with every other
        // refused frame's, once the last callback that can rewrite it has run.
        Some(create_exceeding_limit_frame_result(
            self.exceeding_instruction_result(),
            Gas::new(gas_limit),
            return_memory_offset,
            output,
        ))
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
        }
        // A `Result` needs no work here. A frame init that refuses to build a frame settles in
        // `finalize_frame`, like every other frame outcome, so that whatever an inspector's
        // callback does to the refusal is already in it.
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

    /// Records the compute gas a frame's own classification spent — the code deposit of a
    /// contract creation, on the specs that read the charge back off the result instead of
    /// weighing it beforehand.
    ///
    /// Frozen: REX5 onwards weigh the same charge at the frame's exit and pass `None` here, so
    /// the live callers are the specs through REX4. Their reading is a difference between the
    /// result's gas before and after classification, which is why this stays ahead of the last
    /// mutating callback rather than joining [`finalize_frame`](Self::finalize_frame): a callback
    /// editing that gas would otherwise land inside the difference and be recorded as work.
    pub(crate) fn settle_post_action_charge(
        &mut self,
        result: &mut FrameResult,
        gas_remaining_before_classification: Option<u64>,
    ) {
        let Some(gas_remaining_before) = gas_remaining_before_classification else {
            return;
        };
        let compute_gas_cost = gas_remaining_before.saturating_sub(result.gas().remaining());
        if !self.record_compute_gas(compute_gas_cost) {
            mark_frame_result_as_exceeding_limit(
                result,
                self.exceeding_instruction_result(),
                Default::default(),
            );
        }
    }

    /// Settles a frame's outcome, once and for all.
    ///
    /// # Where this sits
    ///
    /// After the last callback that can rewrite the frame's classification, and before the journal
    /// is told what to do with the frame. Everything here reads the classification the caller will
    /// actually see, and everything the journal does follows from what this leaves behind. That
    /// ordering is the whole point: a settlement taken earlier books a result that may still
    /// change, and a journal decision taken earlier leaves state behind that the reported result
    /// denies.
    ///
    /// # What it does not cover
    ///
    /// The gas clamp's restore and its out-of-gas latch stay ahead of this point, in
    /// [`settle_frame_final_result`](Self::settle_frame_final_result). The latch's input is the
    /// interpreter's own exit classification, which the create-return classification overwrites —
    /// a creation that cannot afford its code deposit ends `OutOfGas` for a reason that has
    /// nothing to do with the clamp — and the code-deposit settlement that runs between the two
    /// reads the latch. Both halves therefore stay where their inputs are still intact.
    ///
    /// The frame's tracker entries are popped later still, when the frame is handed back to its
    /// caller. Popping here would double-pop the paths that reach the caller without running a
    /// frame at all.
    pub(crate) fn finalize_frame(
        &mut self,
        result: &mut FrameResult,
        exit: FrameExit,
        inspector_gas_delta: i128,
    ) {
        // First, because everything below reads the classification: a frame-local exceed rewrites
        // it to a revert.
        self.absorb_frame_local_exceed(result);

        // Taken unconditionally, so a staged envelope can never outlive the frame that staged it.
        let staged_precompile = self.staged_precompile.take();
        // Everything an inspector wrote into this result, whether it wrote it into the frame's
        // terminating action or into the result the action became. The two are the same number
        // measured on either side of the classification, so they settle as one.
        let inspector_gas_delta =
            inspector_gas_delta + core::mem::take(&mut self.staged_action_result_gas);
        // The gas the EVM itself left in this result. Every settlement below is defined against
        // it: the last callback's edit to the number is the inspector's, and the two are only the
        // same object on a frame no callback touched.
        let evm_remaining = evm_own_remaining(result.gas().remaining(), inspector_gas_delta);
        let rescuable =
            self.settle_inspector_result_gas(result, inspector_gas_delta, evm_remaining);

        match exit {
            FrameExit::Ran => {
                debug_assert!(
                    staged_precompile.is_none(),
                    "a precompile never becomes a frame, so it cannot reach a Ran settlement",
                );
                // The burn before the rescue: the rescue's `check_limit` is what latches a
                // TX-level exceed, and a latched exceed is exactly the case whose remainder is
                // handed back rather than destroyed.
                self.settle_exceptional_halt_burn(result, evm_remaining);
                self.try_rescue_gas(result.gas(), rescuable);
            }
            FrameExit::Refused | FrameExit::RefusedSynthetically => {
                // The rescue before the burn, for the mirror-image reason: here the latch the
                // rescue produces is what tells the burn the envelope is being handed back.
                if exit == FrameExit::Refused || self.rex7_enabled() {
                    self.try_rescue_gas(result.gas(), rescuable);
                }
                self.settle_frame_init_reject_burn(result, evm_remaining, staged_precompile);
            }
        }
    }

    /// Books what an inspector did to a frame result's gas, and reports the gas the EVM itself
    /// left in that result.
    ///
    /// `delta` covers both places such an edit can be made: the frame's last mutating callback,
    /// and — one step earlier, through `LoopControl` — the terminating action that *becomes* this
    /// result. They are one number measured on either side of the classification, so they settle
    /// as one.
    ///
    /// Whether such an edit moves anything depends on the frame's final classification, which is
    /// why this can only run here:
    ///
    /// - a returning or reverting frame hands its remaining gas back to its caller, so an edit to
    ///   that number really does change what the transaction spends. It goes to the ledger, and the
    ///   conservation law reads it back out of the envelope;
    /// - a swallowed (halting) frame hands nothing back, so the edit changes nothing the
    ///   transaction spends. The rescue is then taken on `evm_remaining`, the EVM's own number — an
    ///   inspector does not perform work, and gas it removed from a doomed result was never the
    ///   inspector's to destroy.
    ///
    /// Which of the two a result is comes from
    /// [`destroyed_disposition`](super::destroyed_disposition), not from revm's `is_ok_or_revert`
    /// catch-all: a new [`InstructionResult`] variant is a compile error until it is classified.
    ///
    /// Returns the number the resource-limit rescue may hand back to the sender, which is the
    /// result as it now stands on the first case and the EVM's own on the second. The destroyed
    /// settlements always take `evm_remaining`, because a booked edit is already accounted for on
    /// the ledger and booking it a second time as a destroyed remainder would double it.
    fn settle_inspector_result_gas(
        &mut self,
        result: &FrameResult,
        delta: i128,
        evm_remaining: u64,
    ) -> u64 {
        if delta == 0 {
            return evm_remaining;
        }
        if destroyed::remaining_is_destroyed(result.instruction_result()) {
            evm_remaining
        } else {
            self.inspector.result.book(delta);
            result.gas().remaining()
        }
    }

    /// Absorbs a frame-local resource exceed the frame itself latched, into the frame's own
    /// result (REX7+).
    ///
    /// A frame that overran a per-frame budget reverts: the exceed is the frame's, its caller is
    /// free to carry on, and the gas the frame still held goes back to that caller. Running the
    /// rewrite here rather than on the way out to the caller is what makes the frame's state
    /// follow it — the journal decision is still ahead, and it reads this same result — so a
    /// frame that reports a revert has reverted, rather than reporting one over state that stayed
    /// committed. That split is what a contract creation needs closed most: a constructor that
    /// ran to a successful exit and is then rewritten leaves deployed code and emitted logs
    /// behind an otherwise-failed frame.
    ///
    /// Only an exceed that is *already latched* is absorbed here — one the frame recorded against
    /// its own budget while it ran, or one its exit settlement stamped. This deliberately does not
    /// run a fresh [`check_limit`](Self::check_limit): a fresh pass at this point would weigh the
    /// frame's usage against its own budget, whereas the pass that runs on the way out to the
    /// caller weighs it after the frame's usage has been merged into the caller's. Those are
    /// different questions with different answers, and the second one is the one the per-frame
    /// budgets are defined by. So a late first detection stays where it is, and this settles the
    /// one that produces the split.
    ///
    /// Frozen specs absorb everything later, on the way out to the caller, and leave a
    /// successfully-exited frame's state committed under the revert they report.
    fn absorb_frame_local_exceed(&mut self, result: &mut FrameResult) {
        if !self.checkpoint.rex7_enabled() {
            return;
        }
        let limit_check = self.has_exceeded_limit;
        if limit_check.exceeded_limit() && limit_check.is_frame_local() {
            self.has_exceeded_limit = LimitCheck::WithinLimit;
            mark_frame_result_as_exceeding_limit(
                result,
                InstructionResult::Revert,
                limit_check.revert_data(),
            );
        }
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

    /// The verdict [`record_compute_gas`](Self::record_compute_gas) would reach for `charge`,
    /// without recording it and without latching anything.
    ///
    /// Recording and then reacting is the right shape for work that has already happened: the gas
    /// was spent whatever the verdict says. It is the wrong shape for a charge that is still
    /// conditional — one the EVM only takes if the frame survives — because a charge skipped after
    /// being recorded leaves compute gas in the tracker that nothing ever spent. Such a caller asks
    /// here first and records only on the answer that lets the charge happen.
    ///
    /// The verdict is produced by the same predicate enforcement uses, evaluated at `charge` more
    /// usage, so there is no gap between what this reports and what recording would produce.
    #[inline]
    pub(crate) fn would_exceed_compute_gas(&self, charge: u64) -> LimitCheck {
        // Sticky short-circuit, mirroring `record_compute_gas`: an already-latched `ExceedsLimit`
        // is what the caller would observe, and `Exempt` suppresses the decision entirely.
        if !self.has_exceeded_limit.within_limit() {
            return self.has_exceeded_limit;
        }
        self.compute_gas.check_limit_with_extra(charge)
    }

    /// Records the canonical code-deposit compute gas of a CREATE frame that is about to deposit
    /// its code, or reports the result rewrite that stops the deposit from happening (REX7+).
    ///
    /// `charge` is the gas revm charges the frame for the deposit, and it is charged only if the
    /// frame's result is still successful when the action is processed. So the decision has to be
    /// made here, ahead of that: recording it and marking the result afterwards would leave the
    /// tracker holding compute gas for a deposit that then never happened.
    ///
    /// Returns `Some((result, output))` when the charge cannot be afforded, for the caller to write
    /// onto the frame's result:
    ///
    /// - a frame-local exceed reverts the frame and is settled here — nothing is recorded and
    ///   nothing is latched, because with the charge not made the transaction is within its limits
    ///   and the frames above are free to continue;
    /// - a TX-level exceed is latched, which is what the transaction halts and rescues its gas on,
    ///   exactly as it would have had the charge been recorded.
    ///
    /// Returns `None` when the charge fits, having recorded it.
    pub(crate) fn settle_create_code_deposit_compute_gas(
        &mut self,
        charge: u64,
    ) -> Option<(InstructionResult, Bytes)> {
        let check = self.would_exceed_compute_gas(charge);
        if !check.exceeded_limit() {
            let recorded = self.record_compute_gas(charge);
            debug_assert!(recorded, "the peek and the record must reach the same verdict");
            return None;
        }

        let output = check.revert_data();
        if check.is_frame_local() {
            return Some((InstructionResult::Revert, output));
        }

        self.has_exceeded_limit = check;
        // Preserve the volatile-detention attribution the recorded path would have produced: with
        // nothing recorded, usage stays at or below the detained limit, so `is_detained_exceed`
        // cannot see that detention is what the charge ran into.
        self.checkpoint.set_latched_detained(
            self.compute_gas.detained_limit() < self.compute_gas.base_tx_limit(),
        );
        Some((Self::EXCEEDING_LIMIT_INSTRUCTION_RESULT, output))
    }

    /// Hook called when returning a frame result to parent frame in `frame_return_result` or
    /// `last_frame_result`. May modify the frame result in place if the limit is exceeded.
    ///
    /// # The late frame-local exceed
    ///
    /// A per-frame budget is defined by the frame's usage weighed against its *caller's* budget
    /// after the merge, so a frame can overrun one without anything having noticed while it ran.
    /// This hook is where that is first detectable.
    ///
    /// REX7 asks the question ahead of the pop, through
    /// [`peek_check_limit_after_pop`](Self::peek_check_limit_after_pop), and writes the answer onto
    /// the frame's result before anything acts on it. One classification then drives all three
    /// things that follow from it: the caller is told the frame reverted, the pop discards the
    /// frame's usage the way it discards any reverting frame's, and the journal — whose decision
    /// waits until this hook has run — rolls the frame's state back. Weighing the usage before the
    /// merge would be a different question with a different answer, which is why the *reading* is
    /// taken as of after the merge even though the *decision* is taken before it.
    ///
    /// Frozen specs keep the split: the check runs after the pop, the merge has already happened
    /// on the frame's original classification, and revm decided commit-or-revert from that same
    /// classification before the result ever reached this hook — so a frame that ran to a
    /// successful exit is already committed and stays committed under the rewritten `Revert`.
    pub(crate) fn before_frame_return_result<const LAST_FRAME: bool>(
        &mut self,
        result: &mut FrameResult,
    ) {
        // TRUE if the current function is called twice for the top-level frame. If the top-level
        // frame has child frames, the top-level frame's result will be handled twice (one via
        // `EvmTr::frame_return_result`, the other via `Handler::last_frame_result`). This flag is
        // used to distinguish these two cases.
        let duplicate_return_frame_result = LAST_FRAME && !self.data_size.has_active_frame();

        // The merge the pop below is about to perform, read before anything can rewrite it.
        let merges_usage = result.instruction_result().is_ok();
        // Frozen specs need this only for the debug cross-check under the pop, and the `cfg!` is a
        // constant, so their release builds skip it entirely.
        let peeked = (!duplicate_return_frame_result &&
            (self.rex7_enabled() || cfg!(debug_assertions)))
        .then(|| self.peek_check_limit_after_pop(merges_usage));

        if self.rex7_enabled() {
            if let Some(check) = peeked {
                if check.is_frame_local() {
                    // Nothing is latched and nothing needs clearing: the peek only read.
                    mark_frame_result_as_exceeding_limit(
                        result,
                        InstructionResult::Revert,
                        check.revert_data(),
                    );
                }
            }
        }

        // Pop frame from the frame limit trackers.
        self.state_growth.before_frame_return_result::<LAST_FRAME>(result);
        self.data_size.before_frame_return_result::<LAST_FRAME>(result);
        self.kv_update.before_frame_return_result::<LAST_FRAME>(result);
        self.compute_gas.before_frame_return_result::<LAST_FRAME>(result);

        // Pop stipend from stack and burn unused stipend (Rex4+).
        self.storage_call_stipend.before_frame_return_result::<LAST_FRAME>(result);

        let limit_check = self.check_limit();

        // The peek and this check are one question asked on either side of the merge. Whenever the
        // merge the peek was asked about is the merge that happened — which is every frame return
        // on a frozen spec, and every REX7 one the peek did not itself rewrite — the two readings
        // must be identical, down to the reported `limit` and `used`. This is what stands between
        // the pre-pop decision and a drift in what counts as a frame-local exceed.
        debug_assert!(
            peeked.is_none_or(|peeked| result.instruction_result().is_ok() != merges_usage ||
                peeked == limit_check),
            "the pre-pop peek and the post-pop check disagreed: {peeked:?} vs {limit_check:?}"
        );

        // Frame-level limit handling (Rex4+): if frame-local, absorb it — clear the exceed flag
        // and change to Revert so remaining gas returns to the caller. This works at any depth
        // including the top-level frame. Under REX7 the settlement above has already taken the
        // frame-local case, and what reaches here is a second reading of a caller that the discard
        // could not bring back within its budget.
        if limit_check.exceeded_limit() && !duplicate_return_frame_result {
            if limit_check.is_frame_local() {
                let output = limit_check.revert_data();
                self.has_exceeded_limit = LimitCheck::WithinLimit;
                mark_frame_result_as_exceeding_limit(result, InstructionResult::Revert, output);
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
    fn settle_exceptional_halt_burn(&mut self, result: &FrameResult, evm_remaining: u64) {
        if !self.checkpoint.rex7_enabled() ||
            self.limit_exceeded() ||
            !destroyed::remaining_is_destroyed(result.instruction_result())
        {
            return;
        }
        self.compute_gas.record_burned_gas(evm_remaining);
    }

    /// Settles the envelope a frame that never started destroys, as non-enforcing compute gas
    /// (REX7+).
    ///
    /// A frame init can refuse to build a frame and hand back a result instead. Such a result
    /// carries the whole child budget as `remaining`, and what the caller does with it is decided
    /// by the classification alone: a success or a revert is erased back into the caller's
    /// counter, while an exceptional halt is not — the caller simply never sees that gas again.
    /// The child never runs, so the frame-exit settlement that splits an ordinary exceptional halt
    /// cannot see it either, and without this the destroyed budget would be missing from the
    /// transaction's reported total while the conservation law still derives it from the envelope.
    ///
    /// Only a swallowed classification books anything. The success and revert shapes reaching this
    /// site — an empty-code call, a nonce overflow, a depth or balance rejection — destroy nothing
    /// precisely because their gas is erased back into the caller. The split is
    /// [`destroyed_disposition`](super::destroyed_disposition): a new [`InstructionResult`]
    /// variant is a compile error until it is classified.
    ///
    /// A precompile result takes [`settle_precompile_envelope`](Self::settle_precompile_envelope)
    /// instead. It reaches this point the same way and is settled against the same classification,
    /// but neither of the two numbers the formula above uses is the right one for it: the envelope
    /// it destroys is the caller's forwarded amount rather than the REX5-capped budget its result
    /// carries, and a precompile that failed after doing work has that work priced by `MegaETH`
    /// rather than spent down in its gas object.
    ///
    /// Which of the two a result takes is decided by whether its dispatch staged an envelope, not
    /// by `CallOutcome::was_precompile_called`. The flag is on a result an inspector's `call_end`
    /// is handed by mutable reference, so it is not something the accounting may key on; the
    /// staged slot is written before any callback runs and read once.
    ///
    /// Nothing is booked once a limit is latched, which is also why this runs after the rescue in
    /// [`after_frame_init`](Self::after_frame_init) rather than before it. A TX-level exceed
    /// rescues this same remaining gas for the sender and erases it from the envelope, and a
    /// frame-local exceed is absorbed in
    /// [`before_frame_return_result`](Self::before_frame_return_result), which rewrites the result
    /// to a revert and so returns the gas to the caller. Either way the envelope is not destroyed,
    /// and booking it would report gas that was handed back.
    fn settle_frame_init_reject_burn(
        &mut self,
        result: &FrameResult,
        evm_remaining: u64,
        staged_precompile: Option<PrecompileEnvelope>,
    ) {
        if !self.checkpoint.rex7_enabled() || self.limit_exceeded() {
            return;
        }
        if let Some(staged) = staged_precompile {
            self.settle_precompile_envelope(staged, result, evm_remaining);
            return;
        }
        if !destroyed::remaining_is_destroyed(result.instruction_result()) {
            return;
        }
        self.compute_gas.record_burned_gas(evm_remaining);
    }

    /// Settles what a precompile call destroyed, against the classification its caller will
    /// actually see (REX7+).
    ///
    /// The recording site staged the two numbers only it knows — the forwarded envelope and the
    /// work performed — and the classification decides the rest, exactly as it does for an
    /// ordinary frame: a returned classification hands the remainder back to the caller, a
    /// swallowed one does not. Which of the two a result is comes from
    /// [`destroyed_disposition`](super::destroyed_disposition), the same closed table the two
    /// frame burns route through. So the envelope this call consumed is
    ///
    /// ```text
    /// consumed = forwarded − (returned to the caller)
    /// ```
    ///
    /// and everything in it that was not the work performed is destroyed.
    ///
    /// # Why the difference can go the other way
    ///
    /// A halting precompile's `Gas` is reset rather than spent down, so it reports the whole
    /// budget as remaining even when `MegaETH` priced the call as having done work — the KZG fixed
    /// fee for a failure raised inside verification is the one case that exists today. Told that
    /// such a call succeeded, the caller reclaims all of it, fee included. The work stays on the
    /// enforcing lane, because it really was performed; the fee nobody paid for is gas the rewrite
    /// conjured, and goes to the ledger so the conservation law still closes. That direction is
    /// unreachable without an inspector: no classification the EVM itself produces both prices
    /// work and hands the budget back.
    fn settle_precompile_envelope(
        &mut self,
        staged: PrecompileEnvelope,
        result: &FrameResult,
        evm_remaining: u64,
    ) {
        let returned = if destroyed::remaining_is_destroyed(result.instruction_result()) {
            0
        } else {
            evm_remaining
        };
        let consumed = staged.forwarded.saturating_sub(returned);
        self.compute_gas.record_burned_gas(consumed.saturating_sub(staged.executed));
        self.inspector.result.book(i128::from(staged.executed.saturating_sub(consumed)));
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
    ///
    /// `sandbox_gas_used` is the EVM gas the sandbox cost the parent's own counter: the parent
    /// pre-debits a reservation and gets the unused part back, so this is what the parent's
    /// envelope is short by. Whatever of it the sandbox did not spend on compute work is
    /// non-compute gas from the parent's point of view — the sandbox's own storage gas, less any
    /// refund the sandbox's receipt handed back — and joins the lane the destroyed-remainder
    /// derivation reads. The difference is taken against the merged total rather than the enforced
    /// part so the sandbox's destroyed remainder stays destroyed on the parent's books instead of
    /// being reclassified as storage gas.
    pub(crate) fn merge_usage(
        &mut self,
        usage: LimitUsage,
        burned_compute_gas: u64,
        sandbox_gas_used: u64,
    ) {
        self.record_non_compute_gas(i128::from(sandbox_gas_used) - i128::from(usage.compute_gas));
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
/// Undoes the last mutating callback's edit to a frame result's gas, reporting the number the EVM
/// itself left there.
///
/// The EVM does not execute inside an inspector callback, so the difference across one is the
/// inspector's by construction, and every settlement that asks what the *transaction* spent has to
/// ask it of the EVM's number rather than of the rewritten one.
#[inline]
fn evm_own_remaining(remaining: u64, inspector_gas_delta: i128) -> u64 {
    (i128::from(remaining) - inspector_gas_delta).clamp(0, i128::from(u64::MAX)) as u64
}

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

    /// A frame-local exceed the frame latched while it ran is absorbed at the frame's settlement
    /// point under REX7, which is ahead of the journal decision — so the state the frame leaves
    /// behind is rolled back with the revert it reports, instead of staying committed under it.
    ///
    /// Frozen specs must not absorb here: they take the journal decision first, and absorbing
    /// ahead of it would revert state their replay keeps.
    #[test]
    fn test_rex7_absorbs_a_latched_frame_local_exceed_before_the_journal_decision() {
        for spec in [MegaSpecId::REX6, MegaSpecId::REX7] {
            let mut limit = AdditionalLimit::new(spec, EvmTxRuntimeLimits::from_spec(spec));
            limit.set_has_exceeded_limit_for_test(LimitCheck::ExceedsLimit {
                kind: LimitKind::ComputeGas,
                limit: 10,
                used: 11,
                frame_local: true,
            });

            let mut result = stopped_call_result(50_000);
            limit.finalize_frame(&mut result, FrameExit::Ran, 0);

            if spec.is_enabled(MegaSpecId::REX7) {
                assert_eq!(
                    result.instruction_result(),
                    InstructionResult::Revert,
                    "REX7 must absorb here, so the journal decision that follows reverts too",
                );
                assert!(
                    !limit.limit_exceeded(),
                    "an absorbed frame-local exceed must not stop the frames above it",
                );
            } else {
                assert_eq!(
                    result.instruction_result(),
                    InstructionResult::Stop,
                    "a frozen spec absorbs on the way out to the caller, after the journal",
                );
                assert!(limit.limit_exceeded(), "and so it still has the exceed to absorb");
            }
        }
    }

    /// The settlement must not go looking for an exceed of its own. A fresh check here would
    /// weigh a frame's usage against its own budget, while the check that decides a per-frame
    /// exceed weighs it against the caller's, after the frame's usage has been merged in — so a
    /// fresh check here fails frames the per-frame budgets do not.
    #[test]
    fn test_the_settlement_does_not_detect_a_frame_local_exceed_of_its_own() {
        let mut limit = AdditionalLimit::new(
            MegaSpecId::REX7,
            EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(10),
        );
        limit.push_empty_frame();
        // An order of magnitude past the frame's budget, and unlatched: only a fresh check
        // inside the settlement could find it.
        let _ = limit.record_compute_gas_unguarded(100);
        assert!(
            limit.check_limit().is_frame_local(),
            "the fixture must be a frame-local exceed a fresh check would find",
        );
        limit.set_has_exceeded_limit_for_test(LimitCheck::WithinLimit);

        let mut result = stopped_call_result(50_000);
        limit.finalize_frame(&mut result, FrameExit::Ran, 0);

        assert_eq!(
            result.instruction_result(),
            InstructionResult::Stop,
            "the settlement absorbs what the frame latched, and nothing else",
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

    /// The peek must reach exactly the verdict recording would have produced. Recording the
    /// charge and then checking, versus asking first, are compared on the same tracker state
    /// across the whole knife edge — the one place a second copy of `used > limit` could drift
    /// from the copy enforcement runs.
    #[test]
    fn test_peek_matches_record_then_check_across_the_edge() {
        const FRAME_BUDGET: u64 = 1_000;
        for charge in 0..=(FRAME_BUDGET + 2) {
            let mut peeked = rex7_limit();
            peeked.compute_gas.push_frame_with_limit_for_test(FRAME_BUDGET);
            let mut recorded = rex7_limit();
            recorded.compute_gas.push_frame_with_limit_for_test(FRAME_BUDGET);

            let peek = peeked.would_exceed_compute_gas(charge);
            let within = recorded.record_compute_gas(charge);

            assert_eq!(
                peek.exceeded_limit(),
                !within,
                "charge {charge}: the peek and the record must agree on the verdict",
            );
            assert_eq!(
                peek, recorded.has_exceeded_limit,
                "charge {charge}: the peek must report what the record latched",
            );
        }
    }

    /// The frame-local arm of the code-deposit settlement reverts the frame without touching the
    /// tracker: nothing recorded, nothing latched. Both matter — a recorded charge would be
    /// compute gas the deposit never spent, and a latched exceed would outlive the frame that is
    /// already being reverted for it.
    #[test]
    fn test_create_code_deposit_frame_local_arm_records_and_latches_nothing() {
        let mut limit = rex7_limit();
        limit.compute_gas.push_frame_with_limit_for_test(100);
        let before = limit.get_usage().compute_gas;

        let rewrite = limit.settle_create_code_deposit_compute_gas(101);

        let (result, output) = rewrite.expect("an unaffordable charge must rewrite the result");
        assert_eq!(result, InstructionResult::Revert, "a frame-local exceed reverts the frame");
        assert!(!output.is_empty(), "the revert must carry the MegaLimitExceeded payload");
        assert_eq!(limit.get_usage().compute_gas, before, "the charge must not be recorded");
        assert_eq!(latched_kind(&limit), None, "the frame-local arm must not latch");
    }

    /// The TX-level arm latches instead, which is what the transaction halts and rescues its gas
    /// on — but still records nothing. With nothing recorded, usage stays under the detained
    /// limit, so the halt can only keep blaming detention through the latched flag.
    #[test]
    fn test_create_code_deposit_tx_level_arm_latches_detention_without_recording() {
        let mut limit = rex7_limit();
        // A frame budget far above the charge, so the transaction limit is what binds.
        limit.compute_gas.push_frame_with_limit_for_test(u64::MAX);
        limit.set_compute_gas_limit(10);
        let before = limit.get_usage().compute_gas;

        let rewrite = limit.settle_create_code_deposit_compute_gas(11);

        let (result, _) = rewrite.expect("an unaffordable charge must rewrite the result");
        assert_eq!(
            result,
            AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT,
            "a TX-level exceed halts the transaction",
        );
        assert_eq!(limit.get_usage().compute_gas, before, "the charge must not be recorded");
        assert_eq!(latched_kind(&limit), Some(LimitKind::ComputeGas), "the TX-level arm latches");
        assert!(
            limit.detained_compute_gas_halt_reason(VolatileDataAccess::empty()).is_some(),
            "the halt must still be attributable to detention",
        );
    }

    /// An affordable charge is recorded like any other work and leaves the result alone.
    #[test]
    fn test_create_code_deposit_affordable_charge_is_recorded() {
        let mut limit = rex7_limit();
        limit.compute_gas.push_frame_with_limit_for_test(100);
        let before = limit.get_usage().compute_gas;

        assert!(
            limit.settle_create_code_deposit_compute_gas(100).is_none(),
            "an affordable charge must not rewrite the result",
        );
        assert_eq!(limit.get_usage().compute_gas, before + 100, "an affordable charge is recorded",);
        assert_eq!(latched_kind(&limit), None, "an affordable charge must not latch");
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

    /// The settlement point turns the signed derivation into the number the transaction reports,
    /// clamping the one direction that must never reach a consumer.
    ///
    /// A negative derivation means the recorded compute and non-compute lanes together claim more
    /// gas than the transaction spent, which no spec produces today — the guard is defence, not an
    /// expected shape. Driving it at the seam is the only way to reach it: every end-to-end fixture
    /// that could produce it would have to break the conservation law first.
    ///
    /// Debug builds trip the assert instead of clamping, so the test asserts the panic there and
    /// the clamp in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "derived destroyed compute gas is negative")
    )]
    fn test_negative_derivation_is_clamped_to_zero() {
        let mut limit = AdditionalLimit::new(MegaSpecId::REX7, test_limits());
        // Claim more non-compute gas than the envelope the settlement is handed.
        limit.record_non_compute_gas(1_000);

        limit.settle_destroyed_compute_gas(100);

        assert_eq!(
            limit.destroyed_compute_gas(),
            0,
            "a negative derivation must clamp to zero rather than wrap into an enormous total",
        );
    }

    /// The sandbox boundary hands the lane a difference, not a charge, so a sandbox whose own
    /// EIP-3529 refund outgrew its storage gas drives the non-compute lane negative. The
    /// derivation has to stay correct across that sign change — a lane that saturated at zero
    /// would silently under-report the destroyed remainder by the whole overshoot.
    ///
    /// The end-to-end shape that produces a negative lane is in the REX7 suite; this pins the
    /// arithmetic at the seam, where the sign can be set directly.
    #[test]
    fn test_derivation_survives_a_negative_non_compute_lane() {
        let mut limit = AdditionalLimit::new(MegaSpecId::REX7, test_limits());
        // A sandbox that cost the parent 1,000 gas while recording 3,000 of compute work: the
        // 2,000 difference is refund the sandbox's own receipt handed back.
        limit.merge_usage(LimitUsage { compute_gas: 3_000, ..Default::default() }, 0, 1_000);
        assert_eq!(limit.non_compute_gas(), -2_000, "the lane must carry the difference signed");
        // On top of that, a frame destroyed 4,000 of its budget.
        limit.record_burned_gas(4_000);

        // 5,000 spent = 3,000 enforced + (−2,000) non-compute + 4,000 destroyed.
        limit.settle_destroyed_compute_gas(5_000);

        assert_eq!(
            limit.destroyed_compute_gas(),
            4_000,
            "the negative lane must add to the destroyed remainder, not saturate away",
        );
    }
}
