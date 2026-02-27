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
/// KV updates are enforced at the TX level only (no per-frame budgets).
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
    frame_tracker: FrameLimitTracker<CallFrameInfo>,
}

impl KVUpdateTracker {
    pub(crate) fn new(_spec: MegaSpecId, tx_limit: u64) -> Self {
        Self { frame_tracker: FrameLimitTracker::new(tx_limit) }
    }

    /// Pushes a new frame onto the tracker with `u64::MAX` limit.
    /// KV updates use TX-level enforcement only (no per-frame budgets).
    fn push_frame(&mut self, info: CallFrameInfo) {
        self.frame_tracker.push_frame_with_limit(u64::MAX, info);
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
    /// Checks total KV updates across all frames against the TX limit.
    fn check_limit(&self) -> super::LimitCheck {
        let used = self.tx_usage();
        let limit = self.frame_tracker.tx_limit();
        if used > limit {
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
    fn after_inspector_intercept_frame_init(&mut self) {
        self.push_frame(CallFrameInfo { target_address: None, target_updated: false });
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
    ) {
        match &frame_init.frame_input {
            FrameInput::Call(call_inputs) => {
                let has_transfer = call_inputs.transfers_value();
                // Check if parent's account info needs updating BEFORE pushing the child frame.
                // Note: we do NOT set parent's target_updated to true — matching the old tracker,
                // which never mutates it after frame creation.
                let parent_needs_update = has_transfer &&
                    self.frame_tracker
                        .frame_mut()
                        .is_some_and(|entry| !entry.info.target_updated);
                // Push new frame
                self.push_frame(CallFrameInfo {
                    target_address: Some(call_inputs.target_address),
                    target_updated: has_transfer,
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
                let parent_needs_update =
                    self.frame_tracker.frame_mut().is_some_and(|entry| !entry.info.target_updated);
                // Push new frame (address unknown until after init)
                self.push_frame(CallFrameInfo { target_address: None, target_updated: true });
                if parent_needs_update {
                    // Parent's account info update goes to child's discardable,
                    self.record_discardable(1);
                }
            }
            FrameInput::Empty => unreachable!(),
        }
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
    fn before_frame_return_result<const LAST_FRAME: bool>(&mut self, result: &FrameResult) {
        assert!(LAST_FRAME || self.frame_tracker.has_active_frame(), "frame stack is empty");
        self.frame_tracker.pop_frame(result.instruction_result().is_ok());
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
