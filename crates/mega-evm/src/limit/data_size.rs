use alloy_primitives::{Address, U256};
use revm::{
    context::{transaction::AuthorizationTr, Transaction},
    interpreter::{InstructionResult, SStoreResult},
};

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
/// This struct provides comprehensive tracking of all data generated during transaction
/// execution, including transaction data, logs, storage operations, and account updates.
/// It maintains frame-aware tracking to properly handle nested calls and reverts.
///
/// ## Tracked Data Types
///
/// The tracker monitors the following types of data:
/// - Intrinsic transaction data (gas limit, value, signature, etc.)
/// - Transaction calldata
/// - Access list data
/// - EIP-7702 authorization list data
/// - Log topics and data
/// - Storage slot keys and values
/// - Updated account information
/// - Created contract code
///
/// ## Frame Management
///
/// The tracker maintains a stack of frame to properly handle
/// nested calls and reverts. When a frame is reverted, its data is discarded;
/// when a frame completes successfully, its data is merged into the parent frame.
#[derive(Debug, Default)]
pub struct DataSizeTracker {
    /// The current total data size generated from the transaction execution.
    total_size: i64,

    /// The stack of frames
    frame_stack: Vec<FrameInfo>,
}

#[derive(Debug)]
pub(crate) struct FrameInfo {
    /// The data size that should be discarded when the frame is reverted.
    pub(crate) discardable: i64,
    // The target address of the frame. The target address may be temporarily `None` when it is
    // create frame and the target address is not yet created.
    pub(crate) target_address: Option<Address>,
    // Whether the target address's account info marked as updated.
    pub(crate) target_updated: bool,
}

impl FrameInfo {
    pub(crate) fn target_address(&self) -> Address {
        self.target_address.expect("target address is none for frame")
    }
}

impl DataSizeTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reset(&mut self) {
        self.total_size = 0;
        self.frame_stack.clear();
    }

    pub(crate) fn current_size(&self) -> u64 {
        if self.total_size < 0 {
            0
        } else {
            self.total_size as u64
        }
    }

    pub(crate) fn exceeds_limit(&self, limit: u64) -> bool {
        self.current_size() > limit
    }
}

impl DataSizeTracker {
    /// Returns the current frame as a mutable reference.
    pub(crate) fn current_frame(&mut self) -> Option<&mut FrameInfo> {
        self.frame_stack.last_mut()
    }

    /// Returns the parent frame as a mutable reference.
    pub(crate) fn parent_frame(&mut self) -> Option<&mut FrameInfo> {
        let len = self.frame_stack.len();
        if len < 2 {
            return None;
        }
        self.frame_stack.get_mut(len - 2)
    }

    /// Hook called when an execution frame returns.
    ///
    /// This method handles the completion of an execution frame, properly managing the data size
    /// stack based on whether the frame was reverted or completed successfully.
    ///
    /// # Arguments
    ///
    /// * `result` - The frame execution result
    pub(crate) fn end_frame(&mut self, result: InstructionResult, last_frame: bool) {
        if last_frame && self.frame_stack.is_empty() {
            // the last frame may be ended twice. In such case, we just return.
            return;
        }
        let frame = self.frame_stack.pop().expect("frame stack is empty");
        if result.is_ok() {
            // merge the current frame's discardable data into the previous frame or do nothing if
            // the current frame is the last frame.
            self.update_current_frame_discardable_size(frame.discardable);
        } else {
            // discard the current frame's discardable data
            self.total_size -= frame.discardable;
        }
    }
}

impl DataSizeTracker {
    /// Records the data size of a transaction at the start of execution.
    pub(crate) fn record_tx_data(&mut self, tx: &crate::MegaTransaction) {
        // 110 bytes for the intrinsic data of a transaction, including the gas limit, value,
        // signature, gas price, etc.
        let mut size = BASE_TX_SIZE;
        // bytes for the calldata of a transaction
        size += tx.input().len() as u64;
        // bytes for the access list of a transaction
        size += tx
            .access_list()
            .map(|item| item.map(|access| access.size() as u64).sum::<u64>())
            .unwrap_or_default();
        // bytes for the EIP-7702 authorization list of a transaction (101 bytes per authorization)
        size += tx.authorization_list_len() as u64 * AUTHORIZATION_SIZE;
        self.total_size += size as i64;
        // tx data are non-discardable when the frame (or the transaction) is reverted
    }

    /// Records the data size generated by the EIP-7702 authority account info update.
    pub(crate) fn record_eip7702_account_info_update(&mut self, tx: &crate::MegaTransaction) {
        // the 7702 authorization of each account needs one update on its account info
        for authorization in tx.authorization_list() {
            let authority = authorization.authority();
            if let Some(authority) = authority {
                self.record_account_info_update(authority);
            }
        }
    }

    /// Pushes a new frame to the stack.
    pub(crate) fn record_call(&mut self, target_address: Address, transfer: bool) {
        // push a new frame to the stack. The new frame needs to be pushed first so that all data
        // induced by the call can be captured by the new frame.
        self.frame_stack.push(FrameInfo {
            discardable: 0,
            target_address: Some(target_address),
            target_updated: transfer,
        });

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

    pub(crate) fn record_create(&mut self) {
        // push a new frame to the stack. The new frame needs to be pushed first so that all data
        // induced by the create can be captured by the new frame.
        self.frame_stack.push(FrameInfo {
            discardable: 0,
            target_address: None,
            target_updated: true,
        });

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
        if let Some(frame) = self.current_frame() {
            assert!(frame.target_address.is_none(), "created account already recorded");
            frame.target_address = Some(created_address);
            self.record_account_info_update(created_address);
        }
    }

    /// Records the data size of created contract code.
    ///
    /// This internal method records the size of contract code created during
    /// CREATE/CREATE2 operations.
    ///
    /// # Arguments
    ///
    /// * `size` - The size of the created contract code in bytes
    pub(crate) fn record_created_contract_code(&mut self, size: u64) {
        self.total_size += size as i64;
        // if the last frame creates a contract, we don't need to record the data size for future
        // discard. This is because at this point the transaction execution ends and we know the
        // transaction didn't revert.
        if !self.frame_stack.is_empty() {
            // the created contract code should be discarded when the frame is reverted
            self.update_current_frame_discardable_size(size as i64);
        }
    }

    /// Records the data size generated by LOG operations.
    ///
    /// This method calculates and records the data size for log operations,
    /// including both topics and data.
    ///
    /// # Arguments
    ///
    /// * `num_topics` - Number of log topics
    /// * `data_size` - Size of log data in bytes
    pub(crate) fn record_log(&mut self, num_topics: u64, data_size: u64) {
        let size = num_topics * LOG_TOPIC_SIZE + data_size;
        self.total_size += size as i64;
        self.update_current_frame_discardable_size(size as i64);
    }

    /// Records the data size generated by cold SSTORE operations.
    ///
    /// This method calculates and records the estimated data size for storage
    /// write operations, including address, key, value, salt data, and witness data.
    ///
    /// # Arguments
    ///
    /// * `_address` - The contract address (unused but kept for interface consistency)
    /// * `_key` - The storage slot key (unused but kept for interface consistency)
    /// * `store_reuslt` - The result of the SSTORE operation
    pub(crate) fn record_sstore(
        &mut self,
        _address: Address,
        _key: U256,
        store_result: &SStoreResult,
    ) {
        if store_result.is_original_eq_present() {
            // the slot was not written before
            if store_result.is_original_eq_new() {
                // write the same value to the slot, no data is induced
            } else {
                // the slot is written to a new value, we record the data size
                // store a non-zero value to a originally zero slot for the first time
                let size = STORAGE_SLOT_WRITE_SIZE;
                self.total_size += size as i64;
                // the SSTORE data should be discarded when the frame is reverted
                self.update_current_frame_discardable_size(size as i64);
            }
        } else {
            // the slot has already been written before
            if store_result.is_original_eq_new() {
                // the slot is reset to original value, we refund the data size
                let size = STORAGE_SLOT_WRITE_SIZE as i64;
                self.total_size -= size;
                // the SSTORE data should be restored when the frame is reverted
                self.update_current_frame_discardable_size(-size);
            } else {
                // rewrite the slot to a new value, no data is induced
            }
        }
    }

    /// Records the data size generated by account information updates.
    ///
    /// This internal method calculates and records the estimated data size for
    /// account updates, including address, nonce, balance, code hash, and witness data.
    ///
    /// # Arguments
    ///
    /// * `_address` - The account address (unused but kept for interface consistency)
    pub(crate) fn record_account_info_update(&mut self, _address: Address) {
        let size = ACCOUNT_INFO_WRITE_SIZE;
        self.total_size += size as i64;
        // the account info should be discarded when the frame is reverted
        self.update_current_frame_discardable_size(size as i64);
    }

    /// Updates the current frame's discardable data size.
    ///
    /// This internal method adds the specified size to the current frame's
    /// discardable data size in the stack. If there is no current frame, meaning that we are at the
    /// beginning of the transaction or the end of the transaction, the changes will not be
    /// reverted (e.g., the caller's nonce will still be updated, even if the transaction is
    /// reverted).
    ///
    /// # Arguments
    ///
    /// * `size` - The data size to add to the current frame
    fn update_current_frame_discardable_size(&mut self, size: i64) {
        if let Some(frame) = self.frame_stack.last_mut() {
            frame.discardable += size;
        }
    }
}
