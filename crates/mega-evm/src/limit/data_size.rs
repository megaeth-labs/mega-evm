use alloy_primitives::{Address, U256};
use revm::{
    context::{transaction::AuthorizationTr, Transaction},
    handler::{EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, FrameInput, InterpreterAction,
        SStoreResult,
    },
};

use super::frame_limit::CallFrameInfo;
use crate::{FrameLimitTracker, MegaSpecId, TxRuntimeLimit};

/// The number of bytes for the base transaction data.
pub const BASE_TX_SIZE: u64 = 110;
/// The number of bytes for the each EIP-7702 authorization.
pub const AUTHORIZATION_SIZE: u64 = 101;
/// The number of bytes for the each log topic.
pub const LOG_TOPIC_SIZE: u64 = 32;
/// The number of bytes for the salt key.
pub const SALT_KEY_SIZE: u64 = 8;
/// The number of bytes for the salt value delta of the account info. We assume the XOR delta
/// of address, nonce, and code hash is very small, so we can ignore them. The only significant
/// delta is the balance. We over-estimate it to 32 bytes.
pub const SALT_VALUE_DELTA_ACCOUNT_INFO_SIZE: u64 = 32;
/// The number of bytes for the salt value XOR delta of the storage slot. We over-estimate it to
/// 32 bytes.
pub const SALT_VALUE_DELTA_STORAGE_SLOT_SIZE: u64 = 32;
/// The originated data size for reading an account info.
pub const ACCOUNT_INFO_WRITE_SIZE: u64 = SALT_KEY_SIZE + SALT_VALUE_DELTA_ACCOUNT_INFO_SIZE;
/// The originated data size for writing a storage slot.
pub const STORAGE_SLOT_WRITE_SIZE: u64 = SALT_KEY_SIZE + SALT_VALUE_DELTA_STORAGE_SLOT_SIZE;

/// A tracker for the total data size (in bytes) generated from transaction execution.
///
/// Uses `FrameLimitTracker` for frame-aware tracking with per-frame budgets (Rex4+).
///
/// In Rex4+, data size is enforced at the per-frame level: each inner call frame receives
/// `remaining * 98 / 100` of the parent's remaining data size budget.
/// When a frame exceeds its budget, it reverts (not halts) and its discardable data is dropped,
/// protecting the parent's budget.
/// In pre-Rex4, data size is enforced at the TX level only.
///
/// ## Tracked Data Types
///
/// **Non-discardable (permanent):**
/// - Transaction base data: 110 bytes
/// - Calldata: actual byte length
/// - Access lists: sum of access entry sizes
/// - EIP-7702 authorizations: 101 bytes per authorization
/// - Transaction caller account update: 40 bytes
/// - EIP-7702 authority account updates: 40 bytes each
///
/// **Discardable (reverted on frame revert):**
/// - Log data: 32 bytes per topic + data length
/// - Storage writes: 40 bytes (only when original ≠ new value, refunded when reset to original)
/// - Account updates from calls/creates: 40 bytes each
/// - Contract code: actual deployed bytecode size
#[derive(Debug, Clone)]
pub(crate) struct DataSizeTracker {
    rex4_enabled: bool,
    frame_tracker: FrameLimitTracker<CallFrameInfo>,
}

impl DataSizeTracker {
    pub(crate) fn new(spec: MegaSpecId, tx_limit: u64) -> Self {
        Self {
            rex4_enabled: spec.is_enabled(MegaSpecId::REX4),
            frame_tracker: FrameLimitTracker::new(tx_limit),
        }
    }

    /// Pushes a new frame onto the tracker.
    /// In Rex4+, uses the 98/100 budget-based limit derived from parent's remaining budget.
    /// In pre-Rex4, pushes with `u64::MAX` since per-frame limits are not enforced.
    fn push_frame(&mut self, info: CallFrameInfo) {
        if self.rex4_enabled {
            self.frame_tracker.push_frame(info);
        } else {
            self.frame_tracker.push_frame_with_limit(u64::MAX, info);
        }
    }

    /// Returns whether there is at least one active frame on the stack.
    pub(crate) fn has_active_frame(&self) -> bool {
        self.frame_tracker.has_active_frame()
    }

    /// Records discardable data in the current frame.
    fn record_discardable(&mut self, size: u64) {
        if let Some(entry) = self.frame_tracker.frame_mut() {
            entry.discardable_usage += size;
        }
    }

    /// Records a refund (negative data) in the current frame.
    fn record_refund(&mut self, size: u64) {
        if let Some(entry) = self.frame_tracker.frame_mut() {
            entry.refund += size;
        }
    }
}

impl TxRuntimeLimit for DataSizeTracker {
    /// Returns the current effective data size limit for the entire transaction.
    #[inline]
    fn tx_limit(&self) -> u64 {
        self.frame_tracker.tx_limit()
    }

    /// Returns the current total data size across all frames, clamped to zero.
    #[inline]
    fn tx_usage(&self) -> u64 {
        self.frame_tracker.net_usage()
    }

    fn reset(&mut self) {
        self.frame_tracker.reset();
    }

    /// Returns whether the data size limit has been exceeded.
    ///
    /// In Rex4+, checks the current frame's per-frame budget (frame_local: true on exceed).
    /// In pre-Rex4, checks total data size across all frames against the TX limit.
    fn check_limit(&self) -> super::LimitCheck {
        if self.rex4_enabled {
            return self.frame_tracker.exceeds_current_frame_limit(super::LimitKind::DataSize);
        }
        let used = self.tx_usage();
        let limit = self.frame_tracker.tx_limit();
        if used > limit {
            super::LimitCheck::ExceedsLimit {
                kind: super::LimitKind::DataSize,
                limit,
                used,
                frame_local: false,
            }
        } else {
            super::LimitCheck::WithinLimit
        }
    }

    /// Records the data size of a transaction at the start of execution.
    ///
    /// This includes:
    /// - 110 bytes base transaction data
    /// - Calldata byte length
    /// - Access list sizes
    /// - EIP-7702 authorizations (101 bytes each) + authority account updates (40 bytes each)
    /// - Caller account update (40 bytes)
    ///
    /// All recorded as pre-frame (non-discardable) since no frame exists yet.
    fn before_tx_start(&mut self, tx: &crate::MegaTransaction) {
        // TX intrinsic data (non-discardable, recorded before any frame is pushed)
        let mut size = BASE_TX_SIZE;
        size += tx.input().len() as u64;
        size += tx
            .access_list()
            .map(|item| item.map(|access| access.size() as u64).sum::<u64>())
            .unwrap_or_default();
        size += tx.authorization_list_len() as u64 * AUTHORIZATION_SIZE;
        self.frame_tracker.tx_mut().persistent_usage += size;

        // EIP-7702 authority account updates (non-discardable)
        for authorization in tx.authorization_list() {
            if authorization.authority().is_some() {
                self.frame_tracker.tx_mut().persistent_usage += ACCOUNT_INFO_WRITE_SIZE;
            }
        }

        // Caller account update (non-discardable)
        self.frame_tracker.tx_mut().persistent_usage += ACCOUNT_INFO_WRITE_SIZE;
    }

    /// Called when inspector intercepts and skips a call/create.
    ///
    /// Pushes an empty frame so `before_frame_return_result` can pop it to keep
    /// the frame stack aligned with the EVM's call stack.
    #[inline]
    fn push_empty_frame(&mut self) {
        self.push_frame(CallFrameInfo { target_address: None, target_updated: false });
    }

    /// Hook called before a new execution frame is initialized.
    ///
    /// Pushes a new frame and records account info updates:
    /// - **Call with value transfer**: Updates parent's account info if not yet marked, then
    ///   records target account info update (40 bytes each).
    /// - **Create**: Updates parent's account info if not yet marked (caller nonce increment).
    ///   Created address is set later in `after_frame_init_on_frame`.
    /// - **Call without transfer**: No account info updates.
    fn before_frame_init<JOURNAL: crate::JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        frame_init: &FrameInit,
        _journal: &mut JOURNAL,
    ) -> Result<(), JOURNAL::DBError> {
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
                        self.record_discardable(ACCOUNT_INFO_WRITE_SIZE);
                    }
                    // Record target account info update in child's discardable
                    self.record_discardable(ACCOUNT_INFO_WRITE_SIZE);
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
                    // matching the old tracker's behavior.
                    self.record_discardable(ACCOUNT_INFO_WRITE_SIZE);
                }
            }
            FrameInput::Empty => unreachable!(),
        }
        Ok(())
    }

    /// Hook called when a new execution frame is successfully initialized.
    ///
    /// For CREATE frames, records the created address and its account info update (40 bytes).
    fn after_frame_init_on_frame(&mut self, frame: &EthFrame<EthInterpreter>) {
        if frame.data.is_create() {
            let created_address =
                frame.data.created_address().expect("created address is none for create frame");
            if let Some(entry) = self.frame_tracker.frame_mut() {
                assert!(entry.info.target_address.is_none(), "created account already recorded");
                entry.info.target_address = Some(created_address);
                // Record account info update for created address
                entry.discardable_usage += ACCOUNT_INFO_WRITE_SIZE;
            }
        }
    }

    /// Hook called after a frame finishes running.
    ///
    /// For CREATE frames, records the deployed contract bytecode size as discardable data.
    fn after_frame_run<'a>(
        &mut self,
        frame: &'a EthFrame<EthInterpreter>,
        action: &'a mut InterpreterAction,
    ) {
        if let InterpreterAction::Return(interpreter_result) = action {
            if frame.data.is_create() {
                let code_size = interpreter_result.output.len() as u64;
                self.record_discardable(code_size);
            }
        }
    }

    /// Hook called when a frame returns its result to the parent frame.
    ///
    /// Pops the current frame from the tracker:
    /// - **On success**: merges the frame's data into the parent frame.
    /// - **On revert/failure**: discards the frame's discardable data.
    fn before_frame_return_result<const LAST_FRAME: bool>(&mut self, result: &FrameResult) {
        assert!(LAST_FRAME || self.frame_tracker.has_active_frame(), "frame stack is empty");
        self.frame_tracker.pop_frame(result.instruction_result().is_ok());
    }

    /// Hook called when a storage slot is written via `SSTORE`.
    ///
    /// Records SSTORE data based on the storage slot's state transition:
    ///
    /// | Original == Present | Original == New | Effect                | Reason                  |
    /// |---------------------|-----------------|------------------------|-------------------------|
    /// | yes                 | yes             | —                      | No change               |
    /// | yes                 | no              | +40 bytes (discardable)| First write to slot     |
    /// | no                  | yes             | -40 bytes (refund)     | Reset to original value |
    /// | no                  | no              | —                      | Rewrite, no new data    |
    fn after_sstore(&mut self, _target_address: Address, _slot: U256, store_result: &SStoreResult) {
        if store_result.is_original_eq_present() {
            if !store_result.is_original_eq_new() {
                // First write to slot: original == present, but new differs
                self.record_discardable(STORAGE_SLOT_WRITE_SIZE);
            }
        } else if store_result.is_original_eq_new() {
            // Reset to original: refund
            self.record_refund(STORAGE_SLOT_WRITE_SIZE);
        }
    }

    /// Hook called when a log is emitted.
    ///
    /// Records: (`num_topics` * 32 bytes) + `data_size` as discardable.
    fn after_log(&mut self, num_topics: u64, data_size: u64) {
        let size = num_topics * LOG_TOPIC_SIZE + data_size;
        self.record_discardable(size);
    }
}
