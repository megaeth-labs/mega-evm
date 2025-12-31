#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_primitives::{Address, U256};
use revm::{
    context::{transaction::AuthorizationTr, Transaction},
    interpreter::{InstructionResult, SStoreResult},
};

use crate::MegaTransaction;

/// A counter for tracking key-value storage operations during transaction execution.
///
/// This struct provides frame-aware counting of storage operations, properly handling
/// nested calls and reverts. Uses sophisticated logic to track net state changes rather
/// than all operations.
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
#[derive(Debug, Default)]
pub struct KVUpdateCounter {
    /// The total number of key-value updates performed during execution.
    total_count: i64,

    /// The stack of KV update counts per execution frame for proper revert handling.
    ///
    /// This stack allows the counter to properly handle nested calls and reverts
    /// by maintaining separate counts for each execution frame.
    frame_stack: Vec<super::data_size::FrameInfo>,
}

impl KVUpdateCounter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reset(&mut self) {
        self.total_count = 0;
        self.frame_stack.clear();
    }

    #[inline]
    pub(crate) const fn current_count(&self) -> u64 {
        if self.total_count < 0 {
            0
        } else {
            self.total_count as u64
        }
    }

    #[inline]
    pub(crate) const fn exceeds_limit(&self, limit: u64) -> bool {
        self.current_count() > limit
    }

    /// Called when inspector intercepts and skips a call/create.
    ///
    /// Pushes an empty frame so `end_frame` can pop it to keep the stack aligned.
    #[inline]
    pub(crate) fn on_inspector_intercept(&mut self) {
        self.frame_stack.push(super::data_size::FrameInfo {
            discardable: 0,
            target_address: None,
            target_updated: false,
        });
    }
}

impl KVUpdateCounter {
    /// Pushes a new frame to the stack.
    pub(crate) fn new_frame(&mut self, target_address: Option<Address>, target_updated: bool) {
        self.frame_stack.push(super::data_size::FrameInfo {
            discardable: 0,
            target_address,
            target_updated,
        });
    }

    pub(crate) fn parent_frame(&mut self) -> Option<&mut super::data_size::FrameInfo> {
        let len = self.frame_stack.len();
        if len < 2 {
            return None;
        }
        self.frame_stack.get_mut(len - 2)
    }

    /// Hook called when an execution frame returns.
    ///
    /// This method handles the completion of an execution frame, properly managing the KV update
    /// stack and adjusting the total count based on whether the frame was reverted or completed
    /// successfully.
    pub(crate) fn end_frame(&mut self, result: InstructionResult, last_frame: bool) {
        if last_frame && self.frame_stack.is_empty() {
            // the last frame may be ended twice. In such case, we just return.
            return;
        }
        let frame = self.frame_stack.pop().expect("frame stack is empty");
        if result.is_ok() {
            // merge the current frame's kv update into the previous frame
            self.update_current_frame_count(frame.discardable);
        } else {
            // discard the current frame's kv update
            self.total_count -= frame.discardable;
        }
    }
}

impl KVUpdateCounter {
    /// Records a call frame.
    pub(crate) fn record_call(&mut self, target_address: Address, transfer: bool) {
        // new frame in kv update counter. The new frame needs to be pushed first so that all kv
        // updates induced by the call can be captured by the new frame.
        self.new_frame(Some(target_address), transfer);

        if transfer {
            // update the caller if the current frame's target address (i.e., the caller) is not
            // updated
            if let Some(previous_frame) = self.parent_frame() {
                if !previous_frame.target_updated {
                    let target = previous_frame.target_address();
                    self.record_account_info_update(target);
                }
            }
            // record the account info update of the target address
            self.record_account_info_update(target_address);
        }
    }

    /// Records a create frame.
    pub(crate) fn record_create(&mut self) {
        // new frame in kv update counter. The new frame needs to be pushed first so that all kv
        // updates induced by the create can be captured by the new frame.
        self.new_frame(None, true);

        // the caller of the create frame is always updated (nonce increment)
        if let Some(previous_frame) = self.parent_frame() {
            if !previous_frame.target_updated {
                let target = previous_frame.target_address();
                self.record_account_info_update(target);
            }
        }
        // the created account also needs to be updated, but we don't know the created address yet.
        // It will be updated in `record_created_account`.
    }

    /// Records the created account address.
    pub(crate) fn record_created_account(&mut self, created_address: Address) {
        if let Some(frame) = self.frame_stack.last_mut() {
            assert!(frame.target_address.is_none(), "created account already recorded");
            frame.target_address = Some(created_address);
            self.record_account_info_update(created_address);
        }
    }

    /// Records a cold update to a storage slot, using the account's address and the slot as the
    /// key. We do an estimation here by counting every sstore regardless of the uniqueness of
    /// whether the storage slot is warm or cold.
    pub(crate) fn record_sstore(
        &mut self,
        _address: Address,
        _slot: U256,
        store_result: &SStoreResult,
    ) {
        if store_result.is_original_eq_present() {
            // the slot was not written before
            if store_result.is_original_eq_new() {
                // write the same value to the slot, no data is induced
            } else {
                // the slot is written to a new value, we record the data size
                self.total_count += 1;
                self.update_current_frame_count(1);
            }
        } else {
            // the slot has already been written before
            if store_result.is_original_eq_new() {
                // the slot is reset to original value, we refund the data size
                self.total_count -= 1;
                self.update_current_frame_count(-1);
            } else {
                // rewrite the slot to a new value, no data is induced
            }
        }
    }

    /// Records an update to an account info, using the account's address as the key.
    /// We do an estimation here by counting every account info update regardless of whether the
    /// account is warm or cold.
    pub(crate) fn record_account_info_update(&mut self, _address: Address) {
        self.total_count += 1;
        self.update_current_frame_count(1);
    }

    /// Records an update to an EIP-7702 account info, using the account's address as the key.
    /// We do an estimation here by counting every account info update regardless of whether the
    /// account is warm or cold.
    pub(crate) fn record_eip7702_account_info_update(&mut self, tx: &MegaTransaction) {
        for authorization in tx.authorization_list() {
            if let Some(authority) = authorization.authority() {
                self.record_account_info_update(authority);
            }
        }
    }

    /// Updates the current frame's KV update count.
    ///
    /// This internal method adds the specified number of KV updates to the current frame's
    /// KV update count in the stack. If there is no current frame, meaning that we are at the
    /// beginning of the transaction or the end of the transaction, the changes will not be
    /// reverted (e.g., the caller's nonce will still be updated, even if the transaction is
    /// reverted).
    ///
    /// # Arguments
    ///
    /// * `n` - The number of KV updates to add
    fn update_current_frame_count(&mut self, n: i64) {
        if let Some(frame) = self.frame_stack.last_mut() {
            frame.discardable += n;
        }
    }
}
