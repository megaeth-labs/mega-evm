use revm::{handler::FrameResult, interpreter::interpreter_action::FrameInit};

use super::{
    frame_limit::{FrameLimitTracker, TxRuntimeLimit},
    LimitCheck, LimitKind,
};
use crate::{JournalInspectTr, MegaSpecId};

/// The constraint that bounds the gas clamp for one plain-opcode segment.
///
/// Captured when the clamp is applied, so a clamp-induced out-of-gas can be classified against the
/// constraint that was in force at the time rather than against whatever the tracker looks like
/// once the frame has already failed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClampBinding {
    /// The compute headroom the interpreter is allowed to keep seeing.
    pub(crate) headroom: u64,
    /// `true` when the current frame's compute budget is what binds, `false` when the TX-level
    /// (possibly detained) limit is.
    pub(crate) frame_local: bool,
    /// The binding constraint's own limit. A clamp-induced exceed reports this — as
    /// `MegaLimitExceeded.limit` in the frame-local revert payload, or as
    /// `ComputeGasLimitExceeded.limit` in the transaction halt — so it has to be the budget that
    /// actually stopped execution, exactly as the non-clamp check path reports it.
    pub(crate) limit: u64,
}

/// A frame-limit-based compute gas tracker using `FrameLimitTracker`.
///
/// Unlike the other trackers (`DataSizeTracker`, `KVUpdateTracker`, `StateGrowthTracker`), compute
/// gas is **always persistent**: CPU cycles cannot be undone, so even if a child frame reverts,
/// its compute gas still counts toward the parent's total. All gas is recorded as
/// `persistent_usage`, never as `discardable_usage` or `refund`.
///
/// In Rex4+, compute gas is enforced at **both** per-frame and TX level:
/// - **Per-frame**: Each inner call frame receives `remaining * 98 / 100` of the parent's remaining
///   compute gas budget. When a frame exceeds its budget, it reverts (not halts). However, since
///   gas is always persistent, the parent's total gas still increases by the child's actual gas
///   used — per-frame limits act as "early termination guardrails", not budget protection.
/// - **TX-level (detained)**: The effective TX limit may be dynamically lowered by gas detention
///   (volatile data access). In Rex4+ the cap is **relative** to current usage at the access point
///   (`current_usage + cap`), while pre-Rex4 it is absolute. This remains a TX-level halt for all
///   specs including Rex4+.
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
    /// Compute gas settled from the **destroyed** remainders of exceptionally halted frames
    /// (REX7+).
    ///
    /// Recorded into the TX-level lane of `frame_tracker`, so it shows up in the transaction's
    /// reported compute total and in block-level accounting, and subtracted back out of every
    /// limit comparison. A destroyed remainder is gas the EVM threw away without executing
    /// anything for it; letting it trip a limit would turn an ordinary EVM halt into a
    /// resource-limit failure with the remaining gas rescued for the sender, changing a receipt
    /// that must stay identical to per-opcode accounting.
    ///
    /// Only the remainder lands here. The work an exceptionally halted frame actually performed
    /// before it failed is recorded through [`record_gas_used`](Self::record_gas_used) like any
    /// other work, so it shrinks the parent frame's and the transaction's budgets — otherwise the
    /// code that runs after the failed frame returns could spend the same headroom twice. Always 0
    /// before REX7.
    burned: u64,
    frame_tracker: FrameLimitTracker<()>,
}

impl ComputeGasTracker {
    pub(crate) fn new(spec: MegaSpecId, tx_limit: u64) -> Self {
        Self {
            detained_limit: tx_limit,
            burned: 0,
            frame_tracker: FrameLimitTracker::new(spec, tx_limit),
            rex1_enabled: spec.is_enabled(MegaSpecId::REX1),
            rex4_enabled: spec.is_enabled(MegaSpecId::REX4),
        }
    }

    /// Pushes a new frame onto the tracker.
    ///
    /// - **Rex4+ top-level**: budget = `tx_entry.remaining()` (accounts for intrinsic compute gas
    ///   already recorded before the first frame is pushed).
    /// - **Rex4+ nested**: budget = parent's remaining × 98/100 (generic behavior).
    /// - **Pre-Rex4**: `u64::MAX` (TX-level enforcement only).
    fn push_frame(&mut self) {
        if self.rex4_enabled {
            self.frame_tracker.push_frame(());
        } else {
            self.frame_tracker.push_frame_with_limit(u64::MAX, ());
        }
    }

    /// Sets the detained compute gas limit (takes the minimum of current and new effective limit).
    /// This is used to dynamically lower the compute gas limit when volatile data is accessed.
    ///
    /// - **REX4+**: The cap is **relative** to current usage — allows `cap` more compute gas after
    ///   the access point. Effective limit = `current_usage + cap`.
    /// - **Pre-REX4**: The cap is **absolute** — effective limit = `cap`.
    pub(crate) fn set_detained_limit(&mut self, cap: u64) {
        let new_limit = if self.rex4_enabled {
            // REX4+: cap is relative to current usage (limits post-access computation)
            self.enforced_tx_usage().saturating_add(cap)
        } else {
            // Pre-REX4: cap is absolute
            cap
        };
        self.detained_limit = self.detained_limit.min(new_limit);
    }

    /// Returns the remaining compute gas of the current call.
    ///
    /// In Rex4+, returns the minimum of the caller's per-frame remaining compute gas
    /// and the TX-level detained remaining. This ensures the returned value reflects
    /// the actual compute gas available before execution halts, whether due to
    /// per-frame budget exhaustion or TX-level gas detention (e.g., after volatile
    /// data access like TIMESTAMP).
    /// If no frame exists yet (direct TX → system contract), returns the TX-level
    /// remaining which accounts for intrinsic compute gas.
    /// In pre-Rex4, falls back to TX-level remaining compute gas.
    ///
    /// Called during system contract interception, before the callee's frame is pushed.
    /// At that point `frame_stack.last()` is the caller's frame, so
    /// `current_frame_remaining()` gives the caller's remaining compute gas.
    pub(crate) fn current_call_remaining(&self) -> u64 {
        let tx_remaining = self.tx_limit().saturating_sub(self.enforced_tx_usage());
        if self.rex4_enabled {
            self.frame_tracker.current_frame_remaining().min(tx_remaining)
        } else {
            tx_remaining
        }
    }

    /// Returns the current detained compute gas limit.
    pub(crate) fn detained_limit(&self) -> u64 {
        self.detained_limit
    }

    /// Returns the base (undetained) TX compute gas limit.
    pub(crate) fn base_tx_limit(&self) -> u64 {
        self.frame_tracker.tx_limit()
    }

    /// Returns the constraint the gas clamp must bind to at this point in the transaction.
    ///
    /// The headroom is the tighter of the current frame's remaining compute budget (Rex4+) and
    /// the TX-level remaining under the effective (possibly detained) limit — the same pair
    /// [`check_limit`](TxRuntimeLimit::check_limit) enforces. Gas hidden beyond this headroom is
    /// therefore reachable only by a transaction that would exceed one of those two limits.
    /// Equal remainders bind to the TX-level constraint, so a top-level frame — where the two are
    /// equal whenever the same base limit still governs both — halts with gas rescue rather than
    /// absorbing the exceed into a revert.
    #[inline]
    pub(crate) fn clamp_binding(&self) -> ClampBinding {
        let tx_limit = self.tx_limit();
        let tx_remaining = tx_limit.saturating_sub(self.enforced_tx_usage());
        if self.rex4_enabled {
            let frame_remaining = self.frame_tracker.current_frame_remaining();
            if frame_remaining < tx_remaining {
                return ClampBinding {
                    headroom: frame_remaining,
                    frame_local: true,
                    limit: self.frame_tracker.current_frame_limit(),
                };
            }
        }
        ClampBinding { headroom: tx_remaining, frame_local: false, limit: tx_limit }
    }

    /// Returns `true` when gas detention is the binding TX-level constraint, i.e., the detained
    /// limit is tighter than the base TX limit AND actual usage exceeds it.
    pub(crate) fn is_detained_exceed(&self) -> bool {
        let used = self.enforced_tx_usage();
        used > self.detained_limit && self.detained_limit < self.frame_tracker.tx_limit()
    }

    /// Caps the current (top) frame's compute gas budget.
    ///
    /// Used by the **REX4 legacy** `STORAGE_CALL_STIPEND` path: the child's total gas
    /// (`gas_limit`) is inflated by `STORAGE_CALL_STIPEND`, but compute gas must remain
    /// bounded at the original `forwarded_gas + CALL_STIPEND` so the extra storage stipend
    /// cannot be spent on compute opcodes. This method tightens the per-frame compute gas
    /// limit to enforce that cap.
    ///
    /// REX5+ does not call this method: under the separated-allowance model the child's
    /// `gas.limit()` is never inflated, so the per-frame compute budget already equals the
    /// pre-inflation amount naturally.
    pub(crate) fn cap_current_frame_limit(&mut self, cap: u64) {
        if let Some(frame) = self.frame_tracker.frame_mut() {
            frame.limit = frame.limit.min(cap);
        }
    }

    /// Records compute gas as persistent usage in the current frame.
    /// If no frame exists (before `frame_init` or after last frame pop),
    /// records to the `tx_entry`.
    ///
    /// Compute gas is always persistent because CPU cycles cannot be undone.
    #[inline]
    pub(crate) fn record_gas_used(&mut self, gas: u64) {
        if !self.frame_tracker.add_frame_persistent(gas) {
            self.frame_tracker.add_tx_persistent(gas);
        }
    }

    /// Records the destroyed remainder of an exceptionally halted frame (REX7+).
    ///
    /// Counts toward the transaction's reported compute total and block-level accounting, and is
    /// excluded from every limit comparison — see [`burned`](Self::burned).
    pub(crate) fn record_burned_gas(&mut self, amount: u64) {
        self.burned = self.burned.saturating_add(amount);
        self.frame_tracker.add_tx_persistent(amount);
    }

    /// Reclassifies `amount` of already-merged usage as a destroyed remainder (REX7+).
    ///
    /// The sandbox path merges one compute total through
    /// [`merge_persistent_usage`](Self::merge_persistent_usage) and then declares how much of it
    /// the sandbox destroyed, rather than adding the amount a second time.
    pub(crate) fn merge_burned_usage(&mut self, amount: u64) {
        self.burned = self.burned.saturating_add(amount);
    }

    /// The destroyed remainders inside [`tx_usage`](TxRuntimeLimit::tx_usage) — see
    /// [`burned`](Self::burned).
    #[inline]
    pub(crate) fn burned_usage(&self) -> u64 {
        self.burned
    }

    /// Total recorded usage minus the destroyed remainders that must not enforce.
    ///
    /// This is the transaction's claim about how much compute work it actually performed. It is
    /// built only from [`record_gas_used`](Self::record_gas_used) and
    /// [`merge_persistent_usage`](Self::merge_persistent_usage) — a
    /// [`record_burned_gas`](Self::record_burned_gas) raises `net_usage` and `burned` by the same
    /// amount and so cancels here — which is what lets the destroyed remainder be re-derived from
    /// it independently of how it was booked.
    #[inline]
    pub(crate) fn enforced_tx_usage(&self) -> u64 {
        self.frame_tracker.net_usage().saturating_sub(self.burned)
    }

    /// Merges external persistent usage into the TX-level entry.
    ///
    /// Used by `KeylessDeploy` (REX5+) to propagate sandbox compute gas consumption
    /// back to the parent transaction.
    pub(crate) fn merge_persistent_usage(&mut self, amount: u64) {
        self.frame_tracker.add_tx_persistent(amount);
    }

    /// Pushes a frame with an explicit budget, for tests that need a specific frame-local edge
    /// without executing a transaction to get there.
    #[cfg(test)]
    pub(crate) fn push_frame_with_limit_for_test(&mut self, limit: u64) {
        self.frame_tracker.push_frame_with_limit(limit, ());
    }

    /// [`check_limit`](TxRuntimeLimit::check_limit) evaluated as if `extra` gas had already been
    /// recorded, without recording it.
    ///
    /// This is the whole verdict a caller would get by recording and then checking: the Rex4+
    /// per-frame budget first, then the TX-level (possibly detained) limit, reported exactly as
    /// enforcement reports them. A caller that must decide whether to make a charge at all asks
    /// here; `check_limit` is this method at `extra = 0`, so there is no second copy of the
    /// predicate to drift.
    #[inline]
    pub(crate) fn check_limit_with_extra(&self, extra: u64) -> LimitCheck {
        self.check_limit_with_extra_on(&self.frame_tracker.view(), extra)
    }

    /// [`check_limit`](TxRuntimeLimit::check_limit) as it will read once the current frame has
    /// been popped and merged into its caller, computed without popping it.
    #[inline]
    pub(crate) fn check_limit_after_pop(&self, success: bool) -> LimitCheck {
        self.check_limit_with_extra_on(&self.frame_tracker.view_after_pop(success), 0)
    }

    /// [`check_limit_with_extra`](Self::check_limit_with_extra) against an explicit reading of the
    /// tracker.
    ///
    /// The reading is a parameter so that one body can answer both questions asked of this check:
    /// what it says now, and what it will say once a returning frame has been merged into its
    /// caller. A frame return needs the second answer before the merge happens, and a second copy
    /// of the predicates would be free to drift from the first.
    pub(crate) fn check_limit_with_extra_on(
        &self,
        view: &super::FrameLimitView,
        extra: u64,
    ) -> LimitCheck {
        if self.rex4_enabled {
            let frame_check = view.would_exceed_frame_limit(LimitKind::ComputeGas, extra);
            if frame_check.exceeded_limit() {
                return frame_check;
            }
            // Do not early-return on frame WithinLimit:
            // 1) pre-frame intrinsic compute gas is recorded in `tx_entry`, outside current frame
            //    budget;
            // 2) `detained_limit` can be lowered at runtime by volatile-data access.
            // So TX-level detained check must still run even when frame check is within limit.
        }
        // TX-level detained check (all specs): total usage vs effective limit (min of tx/detained).
        // The comparison runs on enforced usage — burned remainders are excluded — while the
        // reported `used` is the full settled total, so a halt reason states the usage the
        // transaction actually ends with. The two coincide on every spec before REX7.
        let limit = self.tx_limit();
        if view.net_usage().saturating_sub(self.burned).saturating_add(extra) > limit {
            LimitCheck::ExceedsLimit {
                kind: LimitKind::ComputeGas,
                frame_local: false,
                limit,
                used: view.net_usage().saturating_add(extra),
            }
        } else {
            LimitCheck::WithinLimit
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
        self.burned = 0;
        // Rex1+: reset detained limit to original TX limit between transactions.
        // Pre-Rex1: the detained limit persists across transactions.
        if self.rex1_enabled {
            self.detained_limit = self.frame_tracker.tx_limit();
        }
    }

    /// Returns whether the compute gas limit has been exceeded.
    ///
    /// In Rex4+, checks the per-frame budget first (`frame_local`: true on exceed), then falls
    /// through to the TX-level detained check. Gas detention is always TX-level (`frame_local`:
    /// false) across all specs — accessing volatile data caps the whole transaction.
    ///
    /// Note: unlike state growth in Rex4+, we intentionally do NOT return early on
    /// frame-within-limit. Compute gas has TX-scope components (e.g., pre-frame intrinsic compute
    /// gas recorded in `tx_entry`) and TX-level detention (`detained_limit`) that can exceed even
    /// when the current frame budget is still within limit.
    #[inline]
    fn check_limit(&self) -> LimitCheck {
        self.check_limit_with_extra(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Usage exactly equal to the detained limit must NOT count as a detained exceed:
    /// the predicate is strictly-greater-than (`used > detained_limit`), not `>=`.
    #[test]
    fn test_is_detained_exceed_is_strict_at_boundary() {
        // tx_limit = 100, detained lowered to 50 (< tx_limit, so detention is binding).
        let mut tracker = ComputeGasTracker::new(MegaSpecId::MINI_REX, 100);
        tracker.set_detained_limit(50);

        // Usage exactly at the detained limit: not exceeded.
        tracker.record_gas_used(50);
        assert!(
            !tracker.is_detained_exceed(),
            "usage == detained_limit must not be a detained exceed"
        );

        // One unit over the limit: exceeded.
        tracker.record_gas_used(1);
        assert!(tracker.is_detained_exceed(), "usage > detained_limit must be a detained exceed");
    }
}
