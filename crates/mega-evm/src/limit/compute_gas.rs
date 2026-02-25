use revm::{handler::FrameResult, interpreter::interpreter_action::FrameInit};

use super::frame_limit::{FrameLimitTracker, LimitCheck, LimitKind, TxRuntimeLimit};
use crate::{JournalInspectTr, MegaSpecId};

/// A frame-limit-based compute gas tracker using `FrameLimitTracker`.
///
/// Unlike other trackers (DataSizeLimit2, KVUpdateLimit2, StateGrowthLimit2), compute gas
/// is **always persistent**: CPU cycles cannot be undone, so even if a child frame reverts,
/// its compute gas still counts toward the parent's total. All gas is recorded as
/// `persistent_usage`, never as `discardable_usage` or `refund`.
///
/// Compute gas is enforced at the TX level only (no per-frame budgets).
/// The effective limit may be dynamically lowered by gas detention (volatile data access).
///
/// Compute gas is NOT recorded via `TxRuntimeLimit` lifecycle hooks. Instead it is
/// recorded externally via `record_gas_used()` called from:
/// - `compute_gas!` macro in `instructions.rs` (per-opcode)
/// - `execution.rs` frame completion (code deposit cost + initial gas)
/// - `precompiles.rs` (precompile gas)
/// - `sandbox/execution.rs` (sandbox gas)
#[derive(Debug, Clone)]
pub(crate) struct ComputeGasLimit2 {
    rex1_enabled: bool,
    /// The effective compute gas limit, which may be dynamically lowered by gas detention
    /// (volatile data access). Always <= frame_tracker.tx_limit().
    detained_limit: u64,
    frame_tracker: FrameLimitTracker<()>,
}

impl ComputeGasLimit2 {
    pub(crate) fn new(spec: MegaSpecId, tx_limit: u64) -> Self {
        Self {
            detained_limit: tx_limit,
            frame_tracker: FrameLimitTracker::new(tx_limit),
            rex1_enabled: spec.is_enabled(MegaSpecId::REX1),
        }
    }

    /// Pushes a new frame onto the tracker with `u64::MAX` limit.
    /// Compute gas uses TX-level enforcement only (no per-frame budgets).
    fn push_frame(&mut self) {
        self.frame_tracker.push_frame_with_limit(u64::MAX, ());
    }

    /// Sets the detained compute gas limit to a new value (takes the minimum).
    /// This is used to dynamically lower the compute gas limit when volatile data is accessed.
    pub(crate) fn set_detained_limit(&mut self, new_limit: u64) {
        self.detained_limit = self.detained_limit.min(new_limit);
    }

    /// Records compute gas as persistent usage in the current frame.
    /// If no frame exists (before frame_init or after last frame pop),
    /// records to the tx_entry.
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

impl TxRuntimeLimit for ComputeGasLimit2 {
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
    /// Checks total usage against the detained limit, which may be dynamically lowered
    /// by volatile data access.
    #[inline]
    fn check_limit(&self) -> LimitCheck {
        let limit = self.tx_limit();
        let used = self.tx_usage();
        if used > limit {
            LimitCheck::ExceedsLimit {
                kind: LimitKind::ComputeGas,
                frame_local: false,
                limit,
                used,
            }
        } else {
            LimitCheck::WithinLimit
        }
    }

    #[inline]
    fn after_inspector_intercept_frame_init(&mut self) {
        self.push_frame();
    }

    /// Push a new frame when a child call/create starts.
    /// Compute gas does not need any data from the frame_init input.
    #[inline]
    fn before_frame_init<JOURNAL: JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        _frame_init: &FrameInit,
        _journal: &mut JOURNAL,
    ) {
        self.push_frame();
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
