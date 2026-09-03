//! REX7+ checkpoint settlement and gas-clamp state.
//!
//! Holds the spec latch, the open-segment interpreter-gas baseline, the clamp
//! in force for the current plain-opcode segment, and the detention-attribution
//! flag for a clamp-induced out-of-gas. Cross-tracker orchestration (reading
//! compute-gas headroom, latching `has_exceeded_limit`) stays on
//! [`AdditionalLimit`](super::AdditionalLimit).

use super::compute_gas::ClampBinding;
use crate::MegaSpecId;

/// Tracks REX7+ checkpoint-accounting and gas-clamp state for one transaction.
#[derive(Debug, Clone)]
pub(crate) struct CheckpointTracker {
    /// REX7+: whether compute gas settles at checkpoints rather than per opcode.
    ///
    /// When set, plain opcodes run unwrapped and record nothing; the interpreter's own gas
    /// counter is read at each checkpoint and the whole segment since the previous one is
    /// recorded in a single call.
    rex7_enabled: bool,

    /// Interpreter gas remaining at the start of the current unsettled segment — the previous
    /// checkpoint, or the frame entry / resume that opened the window. Only meaningful while a
    /// frame is running and only when [`rex7_enabled`](Self::rex7_enabled) is active. Re-synced
    /// at every [`before_frame_run`](super::AdditionalLimit::before_frame_run) (which covers both
    /// frame entry and every resume after a child frame's outcome is merged back) and at every
    /// checkpoint prologue and body recording.
    baseline: u64,

    /// Gas-clamp enforcement (REX7+): the clamp in force for the plain-opcode segment the
    /// current frame is inside, so that revm's own per-opcode gas checks enforce the compute
    /// headroom at no per-opcode cost.
    ///
    /// Present only while the current frame is inside a plain segment: every checkpoint takes it
    /// before running its body — so CALL forwarding, `GAS` and storage charges observe the true
    /// counter — and re-applies it on the way out, and the frame's final result takes it via
    /// [`settle_frame_final_result`](super::AdditionalLimit::settle_frame_final_result).
    clamp: Option<ClampState>,

    /// Whether a clamp-induced out-of-gas was latched while gas detention was the binding TX-level
    /// constraint.
    ///
    /// [`ComputeGasTracker::is_detained_exceed`] requires `used > detained_limit`, which a
    /// clamp-stopped transaction never reaches — the crossing opcode is stopped before it
    /// executes, so usage stays at or below the limit. The halt-reason attribution consults
    /// this flag instead, keeping the reported reason `VolatileDataAccessOutOfGas` exactly as
    /// per-opcode enforcement reports it.
    latched_detained: bool,

    /// EVM gas the transaction spends that is neither compute work nor a destroyed remainder.
    ///
    /// Every gas unit the transaction burns is exactly one of three things: work the trackers
    /// record as compute gas, `MegaETH` storage gas, or budget an exceptional halt threw away.
    /// This field is the running total of the second kind, which makes the third derivable at the
    /// transaction's settlement point instead of having to be booked at each site that destroys
    /// one — see [`AdditionalLimit::conservation_terms`](
    /// super::AdditionalLimit::conservation_terms).
    ///
    /// Signed because one contributor is a difference rather than a charge: the `KeylessDeploy`
    /// sandbox boundary hands the parent one number for what the sandbox cost and another for what
    /// it performed, and an inner EIP-3529 refund can make the second exceed the first.
    non_compute_gas: i128,

    /// `CALL_STIPEND` gas revm mints into child frames, which the transaction's envelope never
    /// funded.
    ///
    /// A value-transferring `CALL` / `CALLCODE` debits the caller only the gas it forwards, and
    /// then hands the child a frame budget of `forwarded + CALL_STIPEND`. The extra stipend is
    /// conjured at the frame boundary — it is the protocol's EIP-150 subsidy, notionally paid for
    /// by the `CALLVALUE` fee, but mechanically it is never taken out of anyone's gas counter.
    /// No frame records the same gas twice: from REX5 the caller's compute window subtracts
    /// exactly what the caller contributed, `gas_limit - CALL_STIPEND`.
    ///
    /// The minted gas leaves the transaction one stipend richer per such call, however it is used.
    /// Spent by the callee, it is recorded as work no envelope paid for; returned when the child
    /// exits, it shrinks the envelope by the same amount. A child that never runs at all — a frame
    /// init that fails on balance or call depth — refunds the whole budget, mint included, and so
    /// shrinks the envelope exactly as a child that returned it would. Every outcome leaves the
    /// frames' recorded work exceeding what the transaction spent by one `CALL_STIPEND` per call,
    /// which is why the mint, not the child frame, is what this field counts.
    ///
    /// So the recorded compute total is *not* a partition of the gas the transaction spent, and
    /// the destroyed-remainder derivation has to account for the minted gas before the two sides
    /// can agree. The behaviour is REX5 semantics and is frozen; this field only measures it.
    minted_call_stipend: u64,

    /// The transaction's destroyed compute gas, as derived from the conservation law once its
    /// envelope was final — the number the transaction reports.
    ///
    /// Zero until the settlement point writes it, so a transaction that never reaches settlement
    /// reports nothing destroyed: a validation reject, which produces no receipt to report into,
    /// and the pre-execution intrinsic overrun, which is itself a validation reject on every spec
    /// that has this lane. See [`AdditionalLimit::settle_destroyed_compute_gas`](
    /// super::AdditionalLimit::settle_destroyed_compute_gas).
    settled_destroyed: u64,
}

/// A gas clamp in force for one plain-opcode segment (REX7+).
///
/// The clamp is a lifecycle, not an amount. It is recorded exactly while it **binds** — while the
/// interpreter's true remaining gas was at or above the compute headroom when the segment opened —
/// and a `hidden` of zero is a binding clamp whose two budgets happened to coincide, not the
/// absence of one. When the frame's own gas would run out ahead of the compute headroom no clamp
/// is recorded at all, and an out-of-gas inside that segment stays the EVM's own.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClampState {
    /// Interpreter gas hidden from the interpreter for this segment.
    pub(crate) hidden: u64,
    /// The constraint the clamp was bound to, captured at the moment it was applied.
    pub(crate) binding: ClampBinding,
}

impl CheckpointTracker {
    pub(crate) fn new(spec: MegaSpecId) -> Self {
        Self {
            rex7_enabled: spec.is_enabled(MegaSpecId::REX7),
            baseline: 0,
            clamp: None,
            latched_detained: false,
            non_compute_gas: 0,
            minted_call_stipend: 0,
            settled_destroyed: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.baseline = 0;
        self.clamp = None;
        self.latched_detained = false;
        self.non_compute_gas = 0;
        self.minted_call_stipend = 0;
        self.settled_destroyed = 0;
    }

    /// Whether compute gas settles at checkpoints (REX7+) rather than per opcode.
    #[inline]
    pub(crate) fn rex7_enabled(&self) -> bool {
        self.rex7_enabled
    }

    /// Interpreter gas remaining at the start of the current unsettled segment.
    #[inline]
    pub(crate) fn baseline(&self) -> u64 {
        self.baseline
    }

    /// Re-opens the settlement window at `remaining`, without recording anything.
    #[inline]
    pub(crate) fn sync_baseline(&mut self, remaining: u64) {
        self.baseline = remaining;
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
        if self.rex7_enabled {
            self.baseline = self.baseline.saturating_sub(amount);
        }
    }

    /// Takes the outstanding clamp so the caller can hand its hidden gas back to the interpreter,
    /// returning that amount.
    #[inline]
    pub(crate) fn restore_hidden(&mut self) -> u64 {
        self.clamp.take().map_or(0, |clamp| clamp.hidden)
    }

    /// Whether a clamp is currently outstanding.
    #[inline]
    pub(crate) fn has_clamp(&self) -> bool {
        self.clamp.is_some()
    }

    /// Records the clamp in force for the segment that starts now.
    #[inline]
    pub(crate) fn set_clamp(&mut self, hidden: u64, binding: ClampBinding) {
        self.clamp = Some(ClampState { hidden, binding });
    }

    /// Takes the outstanding clamp, if any.
    #[inline]
    pub(crate) fn take_clamp(&mut self) -> Option<ClampState> {
        self.clamp.take()
    }

    /// Whether a clamp-induced out-of-gas was latched under a detained TX-level constraint.
    #[inline]
    pub(crate) fn latched_detained(&self) -> bool {
        self.latched_detained
    }

    /// Records whether the just-latched clamp exceed was under a detained TX-level constraint.
    #[inline]
    pub(crate) fn set_latched_detained(&mut self, latched: bool) {
        self.latched_detained = latched;
    }

    /// Adds `amount` to the transaction's non-compute EVM gas — see
    /// [`non_compute_gas`](Self::non_compute_gas). No-op before REX7, where nothing derives the
    /// destroyed remainder.
    #[inline]
    pub(crate) fn record_non_compute_gas(&mut self, amount: i128) {
        if self.rex7_enabled {
            self.non_compute_gas += amount;
        }
    }

    /// The transaction's non-compute EVM gas so far — see
    /// [`non_compute_gas`](Self::non_compute_gas).
    #[inline]
    pub(crate) fn non_compute_gas(&self) -> i128 {
        self.non_compute_gas
    }

    /// Books one `CALL_STIPEND` minted into a child frame — see
    /// [`minted_call_stipend`](Self::minted_call_stipend). No-op before REX7.
    #[inline]
    pub(crate) fn record_minted_call_stipend(&mut self, amount: u64) {
        if self.rex7_enabled {
            self.minted_call_stipend += amount;
        }
    }

    /// The transaction's minted `CALL_STIPEND` total — see
    /// [`minted_call_stipend`](Self::minted_call_stipend).
    #[inline]
    pub(crate) fn minted_call_stipend(&self) -> u64 {
        self.minted_call_stipend
    }

    /// Stores the destroyed compute gas the settlement point derived — see
    /// [`settled_destroyed`](Self::settled_destroyed).
    #[inline]
    pub(crate) fn set_settled_destroyed(&mut self, amount: u64) {
        self.settled_destroyed = amount;
    }

    /// The destroyed compute gas the settlement point derived — see
    /// [`settled_destroyed`](Self::settled_destroyed).
    #[inline]
    pub(crate) fn settled_destroyed(&self) -> u64 {
        self.settled_destroyed
    }

    /// Returns the unsettled segment usage and re-opens the window at `remaining`.
    #[inline]
    pub(crate) fn take_segment(&mut self, remaining: u64) -> u64 {
        let gas_used = self.baseline.saturating_sub(remaining);
        self.baseline = remaining;
        gas_used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirty_tracker() -> CheckpointTracker {
        let mut tracker = CheckpointTracker::new(MegaSpecId::REX7);
        tracker.sync_baseline(99_999);
        tracker.set_clamp(7, ClampBinding { headroom: 1, frame_local: false, limit: 1 });
        tracker.set_latched_detained(true);
        tracker.record_non_compute_gas(4_242);
        tracker.record_minted_call_stipend(2_300);
        tracker
    }

    /// `AdditionalLimit::reset` (called from `MegaContext::on_new_tx`) must wipe leftover
    /// checkpoint state so a reused context cannot leak the previous transaction's baseline,
    /// outstanding clamp, or detention-attribution flag into the next one.
    #[test]
    fn test_reset_clears_baseline_clamp_and_latched_detained() {
        let mut tracker = dirty_tracker();

        tracker.reset();

        assert_eq!(tracker.baseline(), 0, "reset must drop the previous transaction's baseline");
        assert!(
            tracker.take_clamp().is_none(),
            "reset must drop an outstanding clamp left by the previous transaction"
        );
        assert!(
            !tracker.latched_detained(),
            "reset must drop a leftover detention-attribution flag"
        );
        assert_eq!(
            tracker.non_compute_gas(),
            0,
            "reset must drop the previous transaction's non-compute gas"
        );
        assert_eq!(
            tracker.minted_call_stipend(),
            0,
            "reset must drop the previous transaction's minted call stipend"
        );
    }

    /// The non-compute lane is REX7-only state: a frozen spec must keep it at zero so the
    /// derivation it feeds can never be read as meaningful there.
    #[test]
    fn test_non_compute_gas_is_inert_before_rex7() {
        let mut tracker = CheckpointTracker::new(MegaSpecId::REX6);
        tracker.record_non_compute_gas(1_000);
        assert_eq!(tracker.non_compute_gas(), 0, "pre-REX7 must not accumulate non-compute gas");
    }

    /// The sandbox boundary can hand the lane a negative contribution, so the accumulator must be
    /// signed rather than saturating at zero.
    #[test]
    fn test_non_compute_gas_accumulates_signed() {
        let mut tracker = CheckpointTracker::new(MegaSpecId::REX7);
        tracker.record_non_compute_gas(100);
        tracker.record_non_compute_gas(-250);
        assert_eq!(tracker.non_compute_gas(), -150);
    }

    /// Like the non-compute lane, the minted stipend is REX7-only state.
    #[test]
    fn test_minted_call_stipend_is_inert_before_rex7() {
        let mut tracker = CheckpointTracker::new(MegaSpecId::REX6);
        tracker.record_minted_call_stipend(2_300);
        assert_eq!(
            tracker.minted_call_stipend(),
            0,
            "pre-REX7 must not accumulate the minted call stipend"
        );
    }

    /// One transaction can make several value-transferring calls, and each mints its own stipend
    /// into its child — so the term accumulates rather than latching a single one.
    #[test]
    fn test_minted_call_stipend_accumulates_per_call() {
        let mut tracker = CheckpointTracker::new(MegaSpecId::REX7);
        tracker.record_minted_call_stipend(2_300);
        tracker.record_minted_call_stipend(2_300);
        // A call that mints no stipend must leave the running total alone.
        tracker.record_minted_call_stipend(0);
        assert_eq!(tracker.minted_call_stipend(), 4_600);
    }
}
