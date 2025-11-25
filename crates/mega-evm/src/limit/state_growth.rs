//! # State Growth Tracking
//!
//! This module implements tracking of **net state growth** during transaction execution.
//! State growth measures the expansion of blockchain state by counting new accounts and
//! storage slots created, offset by any that are cleared.
//!
//! ## What Counts as State Growth
//!
//! State growth is measured in discrete units, where each unit represents:
//!
//! - **+1** for creating a new account (via `CREATE`, `CREATE2`, or `CALL` with value to empty
//!   account)
//! - **+1** for writing a storage slot from zero to non-zero for the first time
//! - **-1** for clearing a storage slot back to zero (only when the slot was empty at transaction
//!   start)
//!
//! ## Net Growth Model
//!
//! This implementation uses a **net growth** model, meaning:
//! - The counter increases when state is created
//! - The counter decreases when state is cleared
//! - The internal `total_growth` can temporarily become negative during execution
//! - The externally reported growth is clamped to a minimum of zero
//!
//! ### Example: Net Growth Calculation
//!
//! ```text
//! Transaction creates 3 new storage slots:    total_growth = +3
//! Transaction clears 1 slot back to zero:     total_growth = +2
//! Transaction clears 2 more slots:            total_growth = 0
//! ```
//!
//! ## Frame-Based Tracking
//!
//! State growth is tracked using a frame stack that mirrors the EVM's call frame stack.
//! This ensures proper handling of reverts:
//!
//! - Each `CALL`/`CREATE` pushes a new frame
//! - Growth within a frame is tracked as "discardable" (can be reverted)
//! - On successful frame exit, growth is merged into the parent frame
//! - On failed frame exit (revert), growth is discarded
//!
//! ### Example: Frame Revert Behavior
//!
//! ```text
//! Main transaction starts:                    Frame 0: discardable = 0
//! Main creates 2 storage slots:               Frame 0: discardable = 2, total = 2
//! Main calls contract A:                      Frame 1: discardable = 0
//! Contract A creates 3 storage slots:         Frame 1: discardable = 3, total = 5
//! Contract A calls contract B:                Frame 2: discardable = 0
//! Contract B creates 1 storage slot:          Frame 2: discardable = 1, total = 6
//! Contract B reverts:                         Frame 2 discarded, total = 5
//! Contract A completes successfully:          Frame 1 merged to Frame 0, total = 5
//! Transaction completes:                      Final growth = 5
//! ```
//!
//! ## EIP-161 Compliance
//!
//! The tracker implements EIP-161 account clearing rules:
//! - Only `CALL`-like opcodes with value transfer to empty accounts count as creating an account
//! - `CALL` without value transfer to empty accounts does not count (empty account remains empty)
//!
//! ## Storage Slot Tracking
//!
//! Storage slot state transitions are tracked based on three values:
//! - `original_value`: Value at the start of the transaction
//! - `present_value`: Current value before the SSTORE
//! - `new_value`: Value being written
//!
//! Only transitions that affect the **transaction-level** state growth are counted:
//! - `(zero, zero, non-zero)`: First write to empty slot → **+1**
//! - `(zero, non-zero, zero)`: Clear a slot that was empty at transaction start → **-1**
//! - Other transitions: No change (slot was already non-zero at transaction start)

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_primitives::{Address, U256};
use revm::interpreter::InstructionResult;

/// A tracker for tracking the net state growth during transaction execution.
///
/// This tracker maintains a running count of state expansion by monitoring:
/// - New accounts created
/// - Storage slots written from zero to non-zero
/// - Storage slots cleared from non-zero back to zero
///
/// The tracker uses a frame stack to properly handle reverts in nested calls.
/// See module-level documentation for details on the net growth model.
#[derive(Debug, Default, Clone)]
pub struct StateGrowthTracker {
    /// The total net state growth. Can be negative internally if more state is
    /// cleared than created, but reported as zero minimum via `current_growth()`.
    total_growth: i64,

    /// Stack of frames tracking revertable state growth. Each frame corresponds
    /// to a call frame in the EVM execution.
    frame_stack: Vec<FrameInfo>,
}

/// Information about state growth within a single call frame.
#[derive(Debug, Clone)]
struct FrameInfo {
    /// The amount of state growth in this frame that can be reverted if the frame fails.
    /// This value can be negative if more state is cleared than created within the frame.
    discardable: i64,
}

impl StateGrowthTracker {
    /// Creates a new state growth tracker with zero growth.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Resets the tracker to its initial state.
    ///
    /// This clears all accumulated growth and frame stack, preparing the tracker
    /// for a new transaction.
    pub(crate) fn reset(&mut self) {
        self.total_growth = 0;
        self.frame_stack.clear();
    }

    /// Returns the current net state growth, clamped to a minimum of zero.
    ///
    /// # Returns
    ///
    /// The net state growth as a non-negative integer. If the internal `total_growth`
    /// is negative (more state cleared than created), this returns 0.
    ///
    /// # Example
    ///
    /// ```text
    /// internal total_growth = 5  → returns 5
    /// internal total_growth = 0  → returns 0
    /// internal total_growth = -3 → returns 0 (clamped)
    /// ```
    #[inline]
    pub(crate) const fn current_growth(&self) -> u64 {
        if self.total_growth < 0 {
            0
        } else {
            self.total_growth as u64
        }
    }

    /// Checks if the current growth exceeds the specified limit.
    ///
    /// # Arguments
    ///
    /// * `limit` - The maximum allowed state growth
    ///
    /// # Returns
    ///
    /// `true` if current growth strictly exceeds the limit, `false` otherwise.
    #[inline]
    pub(crate) const fn exceeds_limit(&self, limit: u64) -> bool {
        self.current_growth() > limit
    }
}

impl StateGrowthTracker {
    /// Pushes a new frame onto the stack for a new call context.
    ///
    /// This is called at the start of each `CALL`/`CREATE` operation to track
    /// state growth within that call frame separately.
    fn new_frame(&mut self) {
        self.frame_stack.push(FrameInfo { discardable: 0 });
    }

    /// Ends the current frame and handles its state growth based on execution result.
    ///
    /// This method is called when a call frame completes (either successfully or with revert).
    /// It processes the frame's accumulated state growth:
    /// - On success: merges the frame's growth into the parent frame
    /// - On revert: discards the frame's growth from the total
    ///
    /// # Arguments
    ///
    /// * `result` - The execution result of the frame (success or error)
    /// * `last_frame` - Whether this is the last frame of the transaction
    ///
    /// # Frame Stack Handling
    ///
    /// The last frame may be ended twice in some execution paths. This method
    /// handles that case gracefully by returning early if the stack is already empty.
    pub(crate) fn end_frame(&mut self, result: InstructionResult, last_frame: bool) {
        if last_frame && self.frame_stack.is_empty() {
            // the last frame may be ended twice. In such case, we just return.
            return;
        }
        let frame = self.frame_stack.pop().expect("frame stack is empty");
        if result.is_ok() {
            // merge the current frame's discardable into the previous frame
            self.update_current_frame_discardable_size(frame.discardable);
        } else {
            // discard the current frame's discardable
            self.total_growth -= frame.discardable;
        }
    }
}

impl StateGrowthTracker {
    /// Records the new account creation.
    ///
    /// This is invoked when:
    /// - An empty account is called with value transfer (either in a transaction or in a inner
    ///   message call)
    /// - A contract is crated via `CREATE` or `CREATE2` or contract creation transaction
    ///
    /// # Arguments
    ///
    /// * `_address` - The address being created (unused, reserved for future use)
    ///
    /// This implements EIP-161's account clearing rules. Calls without value transfer
    /// to empty accounts do not create an account and thus don't count toward state growth.
    pub(crate) fn on_message_call(&mut self, _address: Address, new_account: bool) {
        self.new_frame();

        if new_account {
            self.total_growth += 1;
            self.update_current_frame_discardable_size(1);
        }
    }

    /// Hook called when a storage slot is written via `SSTORE`.
    ///
    /// This method updates the total growth and discardable size based on the storage
    /// slot's state transition. Only transitions that affect transaction-level state
    /// growth are counted.
    ///
    /// # Arguments
    ///
    /// * `_address` - The address of the storage slot (unused, reserved for future use)
    /// * `_slot` - The storage slot key (unused, reserved for future use)
    /// * `original_value` - The value at the start of the transaction
    /// * `present_value` - The current value before this `SSTORE`
    /// * `new_value` - The value being written
    ///
    /// # State Growth Rules
    ///
    /// The growth change depends on the transition:
    ///
    /// | Original | Present | New   | Growth Change | Reason                                      |
    /// |----------|---------|-------|---------------|---------------------------------------------|
    /// | zero     | zero    | non-0 | **+1**        | First write to empty slot                   |
    /// | zero     | non-0   | zero  | **-1**        | Clear slot that was empty at tx start       |
    /// | zero     | non-0   | non-0 | 0             | Already counted when first written          |
    /// | non-0    | *       | *     | 0             | Slot existed at tx start, no growth change  |
    ///
    /// # Examples
    ///
    /// ```text
    /// Slot starts at 0, write 5:           +1 (new storage)
    /// Slot starts at 0, write 5, write 10: +1 (only counted once)
    /// Slot starts at 0, write 5, write 0:  0  (created then cleared)
    /// Slot starts at 5, write 10:          0  (already existed)
    /// ```
    pub(crate) fn on_sstore(
        &mut self,
        _address: Address,
        _slot: U256,
        original_value: U256,
        present_value: U256,
        new_value: U256,
    ) {
        match (original_value.is_zero(), present_value.is_zero(), new_value.is_zero()) {
            (true, true, false) => {
                // First write to empty slot: slot goes from zero to non-zero
                self.total_growth += 1;
                self.update_current_frame_discardable_size(1);
            }
            (true, false, true) => {
                // Clear slot: slot was zero at tx start, became non-zero, now back to zero
                self.total_growth -= 1;
                self.update_current_frame_discardable_size(-1);
            }
            _ => {
                // No state growth change:
                // - (zero, non-zero, non-zero): Already counted when first written
                // - (non-zero, _, _): Slot existed at tx start, modifications don't count
            }
        }
    }

    /// Updates the current frame's discardable size.
    ///
    /// This internal method adds the specified size to the current frame's
    /// discardable size in the stack. If there is no current frame, meaning that we are at the
    /// beginning of the transaction or the end of the transaction, the changes will not be
    /// reverted (e.g., the caller's nonce will still be updated, even if the transaction is
    /// reverted).
    ///
    /// # Arguments
    ///
    /// * `size` - The size to add to the current frame's discardable size
    fn update_current_frame_discardable_size(&mut self, size: i64) {
        if let Some(frame) = self.frame_stack.last_mut() {
            frame.discardable += size;
        }
    }
}
