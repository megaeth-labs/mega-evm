use alloy_primitives::{Address, U256};
use revm::{
    context::{transaction::AuthorizationTr, Transaction},
    handler::{EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, FrameInput, SStoreResult,
    },
};

use super::frame_limit::{CallFrameInfo, FrameLimitTracker, TxRuntimeLimit};
use crate::{MegaSpecId, MegaTransaction};

/// A counter for tracking key-value storage operations during transaction execution.
///
/// Uses `FrameLimitTracker` for frame-aware counting.
///
/// In Rex4+, KV updates are enforced at the per-frame level: each inner call frame receives
/// `remaining * 98 / 100` of the parent's remaining KV update budget.
/// When a frame exceeds its budget, it reverts (not halts) and its discardable KV updates are
/// dropped, protecting the parent's budget.
/// In pre-Rex4, KV updates are enforced at the TX level only.
/// Units are 1 per KV operation (not bytes).
///
/// ## Tracked Operations
///
/// **Non-discardable (permanent):**
/// - Transaction caller account update: 1 KV update
/// - EIP-7702 authority account updates: 1 KV update each
///
/// **Discardable (reverted on frame revert):**
/// - Storage writes: 1 KV update (only when original ≠ new value, refunded when reset to original)
/// - Account updates from CREATE: 1 KV update for created account
/// - Account updates from CALL with transfer: 2 KV updates (caller + callee)
#[derive(Debug, Clone)]
pub(crate) struct KVUpdateTracker {
    rex4_enabled: bool,
    rex6_enabled: bool,
    frame_tracker: FrameLimitTracker<CallFrameInfo>,
}

impl KVUpdateTracker {
    pub(crate) fn new(spec: MegaSpecId, tx_limit: u64) -> Self {
        Self {
            rex4_enabled: spec.is_enabled(MegaSpecId::REX4),
            rex6_enabled: spec.is_enabled(MegaSpecId::REX6),
            frame_tracker: FrameLimitTracker::new(spec, tx_limit),
        }
    }

    /// Records a discardable KV update in the current frame.
    fn record_discardable(&mut self, n: u64) {
        self.frame_tracker.add_frame_discardable(n);
    }

    /// Records discardable usage into the PARENT frame (one below the top). Used for a
    /// child-CREATE's creator-side account-info write, whose on-chain effect (the creator
    /// nonce bump) is undone only by the parent's revert, not the child's.
    fn record_parent_discardable(&mut self, n: u64) {
        self.frame_tracker.add_parent_discardable(n);
    }

    /// Records a KV update refund in the current frame.
    fn record_refund(&mut self, n: u64) {
        self.frame_tracker.add_frame_refund(n);
    }

    /// Records a single account update as discardable KV update in the current frame.
    ///
    /// Used by SELFDESTRUCT beneficiary metering (REX5+) to charge a KV update for
    /// creating a new beneficiary account.
    pub(crate) fn record_account_update(&mut self) {
        self.record_discardable(1);
    }

    /// Records a single account update as TX-level persistent (non-discardable) KV usage.
    ///
    /// Used by the REX6 EIP-7702 authorization scan, which runs in `validate` before any frame
    /// exists, so the charge cannot go through the frame-scoped `record_account_update`.
    pub(crate) fn record_persistent_account_update(&mut self) {
        self.frame_tracker.add_tx_persistent(1);
    }

    /// Merges external persistent usage into the TX-level entry.
    ///
    /// Used by `KeylessDeploy` (REX5+) to propagate sandbox KV update consumption
    /// back to the parent transaction.
    pub(crate) fn merge_persistent_usage(&mut self, amount: u64) {
        self.frame_tracker.add_tx_persistent(amount);
    }

    /// Returns the remaining KV update budget for the current call frame, capped by
    /// the TX-level remaining.
    pub(crate) fn current_call_remaining(&self) -> u64 {
        let tx_remaining =
            self.frame_tracker.tx_limit().saturating_sub(self.frame_tracker.net_usage());
        if self.rex4_enabled {
            self.frame_tracker.current_frame_remaining().min(tx_remaining)
        } else {
            tx_remaining
        }
    }
}

impl TxRuntimeLimit for KVUpdateTracker {
    /// Returns the current effective KV update limit for the entire transaction.
    #[inline]
    fn tx_limit(&self) -> u64 {
        self.frame_tracker.tx_limit()
    }

    /// Returns the current total KV update count across all frames, clamped to zero.
    #[inline]
    fn tx_usage(&self) -> u64 {
        self.frame_tracker.net_usage()
    }

    #[inline]
    fn reset(&mut self) {
        self.frame_tracker.reset();
    }

    /// Returns whether the KV update limit has been exceeded.
    ///
    /// In Rex4+, checks the per-frame budget first, then falls through to a TX-level check.
    /// The TX-level fallthrough catches intrinsic overflow when no frame exists yet
    /// (intrinsic usage is recorded in `tx_entry` before the first frame is pushed).
    /// In pre-Rex4, checks total KV updates across all frames against the TX limit.
    fn check_limit(&self) -> super::LimitCheck {
        if self.rex4_enabled {
            let frame_check =
                self.frame_tracker.exceeds_current_frame_limit(super::LimitKind::KVUpdate);
            if frame_check.exceeded_limit() {
                return frame_check;
            }
            // TX-level fallthrough: defense-in-depth safety net.
            // In Rex4+ during execution, per-frame budgets are derived from remaining TX
            // budget, so this should only exceed when no frame exists (intrinsic overflow).
        }
        let used = self.tx_usage();
        let limit = self.frame_tracker.tx_limit();
        if used > limit {
            debug_assert!(
                !self.rex4_enabled || !self.frame_tracker.has_active_frame(),
                "KVUpdate TX-level exceeded with active frame — budget invariant violated"
            );
            super::LimitCheck::ExceedsLimit {
                kind: super::LimitKind::KVUpdate,
                limit,
                used,
                frame_local: false,
            }
        } else {
            super::LimitCheck::WithinLimit
        }
    }

    /// Records the KV updates at the start of a transaction.
    ///
    /// This includes:
    /// - EIP-7702 authority account updates (1 each)
    /// - Caller account update (1)
    ///
    /// All recorded as pre-frame (non-discardable) since no frame exists yet.
    fn before_tx_start(&mut self, tx: &MegaTransaction) {
        // EIP-7702 authority account updates (non-discardable).
        //
        // Pre-REX6: charged here for every recoverable authority, including ones that fail the
        // application gates. REX6+ moves this into the journal-aware authorization scan in
        // `validate` so only *applied* authorities are charged; skip here.
        if !self.rex6_enabled {
            for authorization in tx.authorization_list() {
                if authorization.authority().is_some() {
                    self.frame_tracker.add_tx_persistent(1);
                }
            }
        }

        // Caller account update (non-discardable)
        self.frame_tracker.add_tx_persistent(1);
    }

    #[inline]
    fn push_empty_frame(&mut self) {
        self.frame_tracker.push_dummy_frame();
    }

    /// Hook called before a new execution frame is initialized.
    ///
    /// Records KV updates for account info changes:
    /// - **Call with value transfer**: Parent account update (1, if not yet marked) + target
    ///   account update (1).
    /// - **Create**: Parent account update (1, if not yet marked). Created address is set later in
    ///   `after_frame_init_on_frame`.
    /// - **Call without transfer**: No KV updates.
    fn before_frame_init<JOURNAL: crate::JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        frame_init: &FrameInit,
        _journal: &mut JOURNAL,
    ) -> Result<(), JOURNAL::DBError> {
        match &frame_init.frame_input {
            FrameInput::Call(call_inputs) => {
                let has_transfer = call_inputs.transfers_value();
                let parent_needs_update =
                    self.frame_tracker.push_call_frame(call_inputs.target_address, has_transfer);
                if has_transfer {
                    if parent_needs_update {
                        // Parent's account info update goes to child's discardable.
                        self.record_discardable(1);
                    }
                    // A value transfer to the caller itself touches a single account, already
                    // accounted by the caller-side write above (or, at the top level, by the
                    // transaction-start caller record). Recording the target side again would
                    // double-count that one account, so skip it under REX6.
                    if !(self.rex6_enabled && call_inputs.target_address == call_inputs.caller) {
                        // Record target account info update in child's discardable.
                        self.record_discardable(1);
                    }
                }
            }
            FrameInput::Create(_) => {
                let parent_needs_update = self.frame_tracker.push_create_frame();
                if parent_needs_update {
                    if self.rex6_enabled {
                        // The creator's nonce bump survives the child's revert (revm bumps it
                        // before the create checkpoint), so charge it to the parent frame —
                        // see `FrameLimitTracker::add_parent_discardable`.
                        self.record_parent_discardable(1);
                    } else {
                        // Pre-REX6: the creator nonce-bump charge is bundled into the child frame's
                        // discardable lane (frozen behavior).
                        self.record_discardable(1);
                    }
                }
            }
            FrameInput::Empty => unreachable!(),
        }
        Ok(())
    }

    /// Hook called when a new execution frame is successfully initialized.
    ///
    /// For CREATE frames, records the created address and its account info update (1 KV).
    fn after_frame_init_on_frame(&mut self, frame: &EthFrame<EthInterpreter>) {
        if frame.data.is_create() {
            let created_address =
                frame.data.created_address().expect("created address is none for create frame");
            self.frame_tracker.set_created_address(created_address);
            // Record account info update for created address
            self.record_discardable(1);
        }
    }

    /// Hook called when a frame returns its result to the parent frame.
    ///
    /// Rex5+: if the reverting child had set the parent's account-update flag, the flag
    /// is reset so the next successful call from the same parent still charges the parent
    /// account (avoiding undercounting after a revert-then-retry pattern). The unwind is
    /// owned by `FrameLimitTracker::pop_frame_unwind_parent`.
    fn before_frame_return_result<const LAST_FRAME: bool>(&mut self, result: &FrameResult) {
        assert!(LAST_FRAME || self.frame_tracker.has_active_frame(), "frame stack is empty");
        let is_success = result.instruction_result().is_ok();
        self.frame_tracker.pop_frame_unwind_parent(is_success);
    }

    /// Hook called when a storage slot is written via `SSTORE`.
    ///
    /// | Original == Present | Original == New | Effect     | Reason                  |
    /// |---------------------|-----------------|------------|-------------------------|
    /// | yes                 | yes             | —          | No change               |
    /// | yes                 | no              | +1 (disc.) | First write to slot     |
    /// | no                  | yes             | +1 (refund)| Reset to original value |
    /// | no                  | no              | —          | Rewrite, no new KV      |
    fn after_sstore(&mut self, _target_address: Address, _slot: U256, store_result: &SStoreResult) {
        if store_result.is_original_eq_present() {
            if !store_result.is_original_eq_new() {
                self.record_discardable(1);
            }
        } else if store_result.is_original_eq_new() {
            self.record_refund(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `record_account_update` must charge exactly one KV update against the current frame
    /// (used by REX5+ SELFDESTRUCT-beneficiary metering); it must not be a no-op.
    #[test]
    fn test_record_account_update_charges_one_kv() {
        let mut tracker = KVUpdateTracker::new(MegaSpecId::MINI_REX, u64::MAX);
        tracker.push_empty_frame();
        assert_eq!(tracker.tx_usage(), 0);
        tracker.record_account_update();
        assert_eq!(tracker.tx_usage(), 1, "record_account_update must add exactly 1 KV update");
    }
}
