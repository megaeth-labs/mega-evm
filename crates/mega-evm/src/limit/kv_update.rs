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
    rex5_enabled: bool,
    frame_tracker: FrameLimitTracker<CallFrameInfo>,
}

impl KVUpdateTracker {
    pub(crate) fn new(spec: MegaSpecId, tx_limit: u64) -> Self {
        Self {
            rex4_enabled: spec.is_enabled(MegaSpecId::REX4),
            rex5_enabled: spec.is_enabled(MegaSpecId::REX5),
            frame_tracker: FrameLimitTracker::new(tx_limit),
        }
    }

    /// Pushes a new frame onto the tracker.
    ///
    /// In Rex4+, delegates to `FrameLimitTracker::push_frame()` which uses
    /// `tx_entry.remaining()` for the top-level frame (accounts for intrinsic usage)
    /// and parent's remaining × 98/100 for nested frames.
    /// In pre-Rex4, pushes with `u64::MAX` since per-frame limits are not enforced.
    fn push_frame(&mut self, info: CallFrameInfo) {
        if self.rex4_enabled {
            self.frame_tracker.push_frame(info);
        } else {
            self.frame_tracker.push_frame_with_limit(u64::MAX, info);
        }
    }

    /// Records a discardable KV update in the current frame.
    fn record_discardable(&mut self, n: u64) {
        if let Some(entry) = self.frame_tracker.frame_mut() {
            entry.discardable_usage += n;
        }
    }

    /// Records a KV update refund in the current frame.
    fn record_refund(&mut self, n: u64) {
        if let Some(entry) = self.frame_tracker.frame_mut() {
            entry.refund += n;
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
        // EIP-7702 authority account updates (non-discardable)
        for authorization in tx.authorization_list() {
            if authorization.authority().is_some() {
                self.frame_tracker.tx_mut().persistent_usage += 1;
            }
        }

        // Caller account update (non-discardable)
        self.frame_tracker.tx_mut().persistent_usage += 1;
    }

    #[inline]
    fn push_empty_frame(&mut self) {
        self.push_frame(CallFrameInfo {
            target_address: None,
            target_updated: false,
            charged_parent_update: false,
        });
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
                // Check if parent's account info needs updating BEFORE pushing the child frame.
                // In Rex5+, the parent's `target_updated` flag is set to true after charging
                // so repeated value-transferring calls from the same frame don't double-charge
                // the caller account. Pre-Rex5 keeps the old behavior (flag never set),
                // preserving backward compatibility for stable specs.
                // The check and mutation share a single frame_mut() borrow to avoid a redundant
                // second call whose None branch would be unreachable.
                let parent_needs_update = has_transfer &&
                    match self.frame_tracker.frame_mut() {
                        Some(entry) if !entry.info.target_updated => {
                            if self.rex5_enabled {
                                entry.info.target_updated = true;
                            }
                            true
                        }
                        _ => false,
                    };
                // Push new frame; record whether we set the parent's flag so
                // before_frame_return_result can undo it on revert.
                let charged_parent_update = self.rex5_enabled && parent_needs_update;
                self.push_frame(CallFrameInfo {
                    target_address: Some(call_inputs.target_address),
                    target_updated: has_transfer,
                    charged_parent_update,
                });
                if has_transfer {
                    if parent_needs_update {
                        // Parent's account info update goes to child's discardable,
                        // matching the old tracker's behavior.
                        self.record_discardable(1);
                    }
                    // Record target account info update in child's discardable
                    self.record_discardable(1);
                }
            }
            FrameInput::Create(_) => {
                // Check if parent's account info needs updating BEFORE pushing the child frame.
                // See the Call arm for the Rex5+ deduplication rationale.
                let parent_needs_update = match self.frame_tracker.frame_mut() {
                    Some(entry) if !entry.info.target_updated => {
                        if self.rex5_enabled {
                            entry.info.target_updated = true;
                        }
                        true
                    }
                    _ => false,
                };
                // Push new frame (address unknown until after init); record whether we set the
                // parent's flag so before_frame_return_result can undo it on revert.
                let charged_parent_update = self.rex5_enabled && parent_needs_update;
                self.push_frame(CallFrameInfo {
                    target_address: None,
                    target_updated: true,
                    charged_parent_update,
                });
                if parent_needs_update {
                    // Parent's account info update goes to child's discardable,
                    self.record_discardable(1);
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
            if let Some(entry) = self.frame_tracker.frame_mut() {
                assert!(entry.info.target_address.is_none(), "created account already recorded");
                entry.info.target_address = Some(created_address);
                // Record account info update for created address
                entry.discardable_usage += 1;
            }
        }
    }

    /// Hook called when a frame returns its result to the parent frame.
    ///
    /// Rex5+: if the reverting child had set the parent's `target_updated` flag, the flag
    /// is reset so the next successful call from the same parent still charges the parent
    /// account (avoiding undercounting after a revert-then-retry pattern).
    fn before_frame_return_result<const LAST_FRAME: bool>(&mut self, result: &FrameResult) {
        assert!(LAST_FRAME || self.frame_tracker.has_active_frame(), "frame stack is empty");
        let is_success = result.instruction_result().is_ok();
        let child = self.frame_tracker.pop_frame(is_success);
        if !is_success {
            if let Some(child_entry) = child {
                if child_entry.info.charged_parent_update {
                    // charged_parent_update=true implies a parent frame exists
                    // (the flag is only set when frame_mut() returned Some).
                    self.frame_tracker
                        .frame_mut()
                        .expect("parent frame must exist when charged_parent_update is true")
                        .info
                        .target_updated = false;
                }
            }
        }
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
