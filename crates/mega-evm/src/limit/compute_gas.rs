use revm::{handler::FrameResult, interpreter::interpreter_action::FrameInit};

use super::{
    frame_limit::{FrameLimitTracker, TxRuntimeLimit},
    LimitCheck, LimitKind,
};
use crate::{JournalInspectTr, MegaSpecId};

/// A frame-limit-based compute gas tracker using `FrameLimitTracker`.
///
/// Unlike the other trackers (`DataSizeTracker`, `KVUpdateTracker`, `StateGrowthTracker`), compute
/// gas is **always persistent**: CPU cycles cannot be undone, so even if a child frame reverts,
/// its compute gas still counts toward the parent's total. All gas is recorded as
/// `persistent_usage`, never as `discardable_usage` or `refund`.
///
/// In Rex4+, compute gas is enforced at **both** per-frame and TX level:
/// - **Per-frame**: Each inner call frame receives `remaining * 98 / 100` of the parent's
///   remaining compute gas budget. When a frame exceeds its budget, it reverts (not halts).
///   However, since gas is always persistent, the parent's total gas still increases by the
///   child's actual gas used — per-frame limits act as "early termination guardrails", not
///   budget protection.
/// - **TX-level (detained)**: The effective TX limit may be dynamically lowered by gas detention
///   (volatile data access). This remains a TX-level halt for all specs including Rex4+.
///
/// In pre-Rex4, compute gas is enforced at the TX level only.
///
/// Compute gas is NOT recorded via `TxRuntimeLimit` lifecycle hooks. Instead it is
/// recorded externally via `record_gas_used()` called from:
/// - `compute_gas!` macro in `instructions.rs` (per-opcode)
/// - `execution.rs` frame completion (code deposit cost + initial gas)
/// - `precompiles.rs` (precompile gas)
/// - `sandbox/execution.rs` (sandbox gas)
#[derive(Debug, Clone)]
pub(crate) struct ComputeGasTracker {
    rex1_enabled: bool,
    rex4_enabled: bool,
    /// The effective compute gas limit, which may be dynamically lowered by gas detention
    /// (volatile data access). Always <= `frame_tracker.tx_limit()`.
    detained_limit: u64,
    frame_tracker: FrameLimitTracker<()>,
}

impl ComputeGasTracker {
    pub(crate) fn new(spec: MegaSpecId, tx_limit: u64) -> Self {
        Self {
            detained_limit: tx_limit,
            frame_tracker: FrameLimitTracker::new(tx_limit),
            rex1_enabled: spec.is_enabled(MegaSpecId::REX1),
            rex4_enabled: spec.is_enabled(MegaSpecId::REX4),
        }
    }

    /// Pushes a new frame onto the tracker.
    /// In Rex4+, uses the 98/100 budget-based limit derived from parent's remaining budget.
    /// In pre-Rex4, pushes with `u64::MAX` since TX-level enforcement only.
    fn push_frame(&mut self) {
        if self.rex4_enabled {
            self.frame_tracker.push_frame(());
        } else {
            self.frame_tracker.push_frame_with_limit(u64::MAX, ());
        }
    }

    /// Sets the detained compute gas limit to a new value (takes the minimum).
    /// This is used to dynamically lower the compute gas limit when volatile data is accessed.
    pub(crate) fn set_detained_limit(&mut self, new_limit: u64) {
        self.detained_limit = self.detained_limit.min(new_limit);
    }

    /// Records compute gas as persistent usage in the current frame.
    /// If no frame exists (before `frame_init` or after last frame pop),
    /// records to the `tx_entry`.
    ///
    /// Compute gas is always persistent because CPU cycles cannot be undone.
    pub(crate) fn record_gas_used(&mut self, gas: u64) {
        if let Some(entry) = self.frame_tracker.frame_mut() {
            entry.persistent_usage += gas;
        } else {
            self.frame_tracker.tx_mut().persistent_usage += gas;
        }
    }
}

impl TxRuntimeLimit for ComputeGasTracker {
    /// Returns the current effective compute gas limit for the entire transaction (may be
    /// detained/lowered by volatile data access).
    #[inline]
    fn tx_limit(&self) -> u64 {
        self.frame_tracker.tx_limit().min(self.detained_limit)
    }

    /// Returns the current total compute gas used across all frames.
    #[inline]
    fn tx_usage(&self) -> u64 {
        self.frame_tracker.net_usage()
    }

    #[inline]
    fn reset(&mut self) {
        self.frame_tracker.reset();
        // Rex1+: reset detained limit to original TX limit between transactions.
        // Pre-Rex1: the detained limit persists across transactions.
        if self.rex1_enabled {
            self.detained_limit = self.frame_tracker.tx_limit();
        }
    }

    /// Returns whether the compute gas limit has been exceeded.
    ///
    /// In Rex4+, checks the per-frame budget first (frame_local: true on exceed), then falls
    /// through to the TX-level detained check. Gas detention is always TX-level (frame_local:
    /// false) across all specs — accessing volatile data caps the whole transaction.
    #[inline]
    fn check_limit(&self) -> LimitCheck {
        if self.rex4_enabled {
            let frame_check =
                self.frame_tracker.exceeds_current_frame_limit(LimitKind::ComputeGas);
            if frame_check.exceeded_limit() {
                return frame_check;
            }
        }
        // TX-level detained check (all specs): total usage vs effective limit (min of tx/detained).
        let limit = self.tx_limit();
        let used = self.tx_usage();
        if used > limit {
            LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, frame_local: false, limit, used }
        } else {
            LimitCheck::WithinLimit
        }
    }

    #[inline]
    fn push_empty_frame(&mut self) {
        self.push_frame();
    }

    /// Push a new frame when a child call/create starts.
    /// Compute gas does not need any data from the `frame_init` input.
    #[inline]
    fn before_frame_init<JOURNAL: JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        _frame_init: &FrameInit,
        _journal: &mut JOURNAL,
    ) -> Result<(), JOURNAL::DBError> {
        self.push_frame();
        Ok(())
    }

    /// Pop frame when returning. Since all gas is recorded as `persistent_usage`,
    /// the SUCCESS flag has no effect (only `discardable_usage` and `refund` differ,
    /// both are always 0 for compute gas). We use the actual result for convention.
    #[inline]
    fn before_frame_return_result<const LAST_FRAME: bool>(&mut self, result: &FrameResult) {
        assert!(LAST_FRAME || self.frame_tracker.has_active_frame(), "frame stack is empty");
        self.frame_tracker.pop_frame(result.instruction_result().is_ok());
    }
}
