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

use alloy_primitives::{Address, U256};
use revm::{
    handler::FrameResult,
    interpreter::{interpreter_action::FrameInit, FrameInput, SStoreResult},
    primitives::hardfork::SpecId,
};

use crate::{FrameLimitTracker, JournalInspectTr, MegaSpecId, TxRuntimeLimit};

/// A tracker for net state growth during transaction execution.
///
/// Uses `FrameLimitTracker` for frame-aware tracking with per-frame budgets (Rex4+).
///
/// State growth measures the expansion of blockchain state by counting new accounts and
/// storage slots created, offset by any that are cleared:
///
/// - **+1** for creating a new account (via `CREATE`, `CREATE2`, or `CALL` with value to empty
///   account per EIP-161)
/// - **+1** for writing a storage slot from zero to non-zero for the first time
/// - **-1** for clearing a storage slot back to zero (only when the slot was empty at transaction
///   start)
///
/// See module-level documentation for details on the net growth model and frame-based tracking.
#[derive(Debug, Clone)]
pub(crate) struct StateGrowthTracker {
    spec: MegaSpecId,
    frame_tracker: FrameLimitTracker<()>,
}

impl StateGrowthTracker {
    pub(crate) fn new(spec: MegaSpecId, tx_limit: u64) -> Self {
        Self { spec, frame_tracker: FrameLimitTracker::new(tx_limit) }
    }

    /// Pushes a new frame onto the tracker.
    ///
    /// For Rex4+, delegates to `FrameLimitTracker::push_frame()` which uses
    /// `tx_entry.remaining()` for the top-level frame and parent's remaining × 98/100
    /// for nested frames.
    /// For pre-Rex4, pushes with `u64::MAX` since per-frame limits are not enforced
    /// (the TX-level check in `check_limit()` uses `net_usage()` instead).
    fn push_frame(&mut self) {
        if self.spec.is_enabled(MegaSpecId::REX4) {
            self.frame_tracker.push_frame(());
        } else {
            self.frame_tracker.push_frame_with_limit(u64::MAX, ());
        }
    }

    /// Records positive state growth in the current frame.
    fn record_growth(&mut self, n: u64) {
        if let Some(entry) = self.frame_tracker.frame_mut() {
            // For state growth, all growth in the current transaction is discardable on revert.
            entry.discardable_usage += n;
        }
    }

    /// Records a refund (negative growth) in the current frame.
    fn record_refund(&mut self, n: u64) {
        if let Some(entry) = self.frame_tracker.frame_mut() {
            entry.refund += n;
        }
    }
}

impl TxRuntimeLimit for StateGrowthTracker {
    /// Returns the current effective state growth limit for the entire transaction.
    #[inline]
    fn tx_limit(&self) -> u64 {
        self.frame_tracker.tx_limit()
    }

    /// Returns the current net state growth across all frames, clamped to zero.
    #[inline]
    fn tx_usage(&self) -> u64 {
        self.frame_tracker.net_usage()
    }

    /// Resets the tracker to its initial state.
    ///
    /// This clears the frame stack, preparing the tracker for a new transaction.
    #[inline]
    fn reset(&mut self) {
        self.frame_tracker.reset();
    }

    /// Returns whether the state growth limit has been exceeded.
    ///
    /// For Rex4+, checks the per-frame budget first, then falls through to a TX-level check.
    /// The TX-level fallthrough is a defense-in-depth safety net (state growth currently has
    /// no intrinsic usage in `tx_entry`, so the fallthrough is a no-op today).
    /// For pre-Rex4, checks total net growth across all frames against the TX limit.
    fn check_limit(&self) -> super::LimitCheck {
        if self.spec.is_enabled(MegaSpecId::REX4) {
            let frame_check =
                self.frame_tracker.exceeds_current_frame_limit(super::LimitKind::StateGrowth);
            if frame_check.exceeded_limit() {
                return frame_check;
            }
            // TX-level fallthrough: defense-in-depth safety net.
            // State growth currently has no intrinsic usage in tx_entry, so this
            // should not exceed during normal execution.
        }
        let used = self.tx_usage();
        let limit = self.frame_tracker.tx_limit();
        if used > limit {
            super::LimitCheck::ExceedsLimit {
                kind: super::LimitKind::StateGrowth,
                limit,
                used,
                frame_local: false,
            }
        } else {
            super::LimitCheck::WithinLimit
        }
    }

    /// Called when inspector intercepts and skips a call/create.
    ///
    /// Pushes an empty frame so `before_frame_return_result` can pop it to keep
    /// the frame stack aligned with the EVM's call stack.
    #[inline]
    fn push_empty_frame(&mut self) {
        self.push_frame();
    }

    /// Hook called before a new execution frame is initialized.
    ///
    /// Pushes a new frame onto the tracker and records any state growth caused by
    /// the frame initialization itself:
    ///
    /// - **Call with value transfer to empty account**: +1 growth (EIP-161 compliant). Only
    ///   `CALL`-like opcodes with value transfer to empty accounts count as creating an account.
    ///   `CALL` without value transfer does not count (empty account remains empty).
    /// - **Create** (`CREATE` / `CREATE2`): +1 growth for the new account.
    /// - **Call without value transfer** or **call to non-empty account**: no growth.
    fn before_frame_init<JOURNAL: JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        frame_init: &FrameInit,
        journal: &mut JOURNAL,
    ) -> Result<(), JOURNAL::DBError> {
        self.push_frame();

        match &frame_init.frame_input {
            FrameInput::Call(call_inputs) => {
                // EIP-161: only value transfers to empty accounts count as creating an account.
                if call_inputs.transfers_value() {
                    let to_account =
                        journal.inspect_account_delegated(self.spec, call_inputs.target_address)?;
                    let is_empty = to_account.state_clear_aware_is_empty(SpecId::PRAGUE);
                    if is_empty {
                        self.record_growth(1);
                    }
                }
            }
            FrameInput::Create(_) => {
                self.record_growth(1);
            }
            FrameInput::Empty => unreachable!(),
        }
        Ok(())
    }

    /// Hook called when a storage slot is written via `SSTORE`.
    ///
    /// Updates the frame's growth and refund counters based on the storage slot's state
    /// transition.
    /// Only transitions that affect **transaction-level** state growth are counted:
    ///
    /// | Original | Present | New   | Effect     | Reason                                      |
    /// |----------|---------|-------|------------|---------------------------------------------|
    /// | zero     | zero    | non-0 | +1 growth  | First write to empty slot                   |
    /// | zero     | non-0   | zero  | +1 refund  | Clear slot that was empty at tx start       |
    /// | zero     | non-0   | non-0 | —          | Already counted when first written          |
    /// | non-0    | *       | *     | —          | Slot existed at tx start, no growth change  |
    ///
    /// # Examples
    ///
    /// ```text
    /// Slot starts at 0, write 5:           +1 (new storage)
    /// Slot starts at 0, write 5, write 10: +1 (only counted once)
    /// Slot starts at 0, write 5, write 0:  0  (created then cleared via refund)
    /// Slot starts at 5, write 10:          0  (already existed)
    /// ```
    fn after_sstore(&mut self, _target_address: Address, _slot: U256, store_result: &SStoreResult) {
        match (
            store_result.original_value.is_zero(),
            store_result.present_value.is_zero(),
            store_result.new_value.is_zero(),
        ) {
            (true, true, false) => {
                // First write to empty slot: slot goes from zero to non-zero → +1
                self.record_growth(1);
            }
            (true, false, true) => {
                // Clear slot: was zero at tx start, became non-zero, now back to zero → -1
                self.record_refund(1);
            }
            _ => {
                // No state growth change:
                // - (zero, non-zero, non-zero): Already counted when first written
                // - (non-zero, _, _): Slot existed at tx start, modifications don't count
            }
        }
    }

    /// Hook called when a frame returns its result to the parent frame.
    ///
    /// Pops the current frame from the tracker and handles its accumulated state growth:
    /// - **On success**: merges the frame's growth and refunds into the parent frame via
    ///   `pop_frame::<true>()`.
    /// - **On revert/failure**: discards the frame's discardable growth (refunds and persistent
    ///   usage are still propagated) via `pop_frame::<false>()`.
    fn before_frame_return_result<const LAST_FRAME: bool>(&mut self, result: &FrameResult) {
        assert!(LAST_FRAME || self.frame_tracker.has_active_frame(), "frame stack is empty");
        self.frame_tracker.pop_frame(result.instruction_result().is_ok());
    }

    /// Hook called after a SELFDESTRUCT on a same-TX-created account (REX4+).
    ///
    /// Records a refund for the account and its new storage slots that were
    /// previously counted as state growth. This is frame-aware: if the frame reverts,
    /// both the SELFDESTRUCT and the refund are discarded together.
    fn after_selfdestruct(&mut self, refund: u64) {
        self.record_refund(refund);
    }
}
