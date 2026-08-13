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
        }
    }

    pub(crate) fn reset(&mut self) {
        self.baseline = 0;
        self.clamp = None;
        self.latched_detained = false;
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

    /// Returns the unsettled segment usage and re-opens the window at `remaining`.
    #[inline]
    pub(crate) fn take_segment(&mut self, remaining: u64) -> u64 {
        let gas_used = self.baseline.saturating_sub(remaining);
        self.baseline = remaining;
        gas_used
    }
}
