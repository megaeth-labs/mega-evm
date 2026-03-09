#[cfg(not(feature = "std"))]
use alloc as std;
use alloy_primitives::{Address, U256};
use revm::{
    handler::{EthFrame, FrameResult},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, InterpreterAction, SStoreResult,
    },
};
use std::vec::Vec;

use crate::{constants, JournalInspectTr, MegaTransaction};

use super::{LimitCheck, LimitKind};

/// Per-frame metadata for trackers that need account update deduplication
/// (data size and KV update trackers).
#[derive(Debug, Clone, Default)]
pub(crate) struct CallFrameInfo {
    /// The target address of the frame. `None` during CREATE until the address is known.
    pub(crate) target_address: Option<Address>,
    /// Whether this frame's target address has been marked as updated.
    pub(crate) target_updated: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FrameLimitTracker<I> {
    /// Top-level (TX-scope) entry. Holds the TX limit and accumulates usage
    /// from pre-frame data and from the last frame pop.
    tx_entry: FrameLimitEntry<()>,
    /// Stack of child frame entries.
    frame_stack: Vec<FrameLimitEntry<I>>,
}

/// Per-frame budget entry on the frame stack.
#[derive(Debug, Clone)]
pub(crate) struct FrameLimitEntry<I> {
    /// Maximum usage allowed in this frame.
    pub(crate) limit: u64,
    /// Persistent usage in this frame even if it is reverted.
    pub(crate) persistent_usage: u64,
    /// Discardable usage if this frame is reverted.
    pub(crate) discardable_usage: u64,
    /// Refund usage in this frame.
    pub(crate) refund: u64,

    /// Additional information about the frame.
    #[allow(dead_code)]
    pub(crate) info: I,
}

impl<I> FrameLimitEntry<I> {
    pub(crate) fn new(limit: u64, info: I) -> Self {
        Self { limit, persistent_usage: 0, discardable_usage: 0, refund: 0, info }
    }

    /// Returns the remaining budget for this frame.
    ///
    /// Computed as `limit - (used - refund)`, clamped to `[0, limit]`.
    /// The net usage (`used - refund`) is computed first to stay consistent with
    /// the exceed check in `exceeds_current_frame_limit`.
    #[inline]
    pub(crate) fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used().saturating_sub(self.refund))
    }

    /// Returns usage for this frame.
    #[inline]
    pub(crate) fn used(&self) -> u64 {
        self.persistent_usage.checked_add(self.discardable_usage).expect("overflow")
    }
}

impl<I> FrameLimitTracker<I> {
    pub(crate) fn new(tx_limit: u64) -> Self {
        Self { tx_entry: FrameLimitEntry::new(tx_limit, ()), frame_stack: Vec::new() }
    }

    /// Returns the TX-level limit.
    pub(crate) fn tx_limit(&self) -> u64 {
        self.tx_entry.limit
    }

    /// Resets the tracker for a new transaction.
    pub(crate) fn reset(&mut self) {
        self.tx_entry.persistent_usage = 0;
        self.tx_entry.discardable_usage = 0;
        self.tx_entry.refund = 0;
        self.frame_stack.clear();
    }

    /// Returns the remaining compute gas of the current frame.
    ///
    /// If the frame stack is non-empty, returns the top frame's remaining budget.
    /// If the frame stack is empty (before the first frame is pushed), returns
    /// the TX-level remaining which accounts for pre-frame usage (e.g. intrinsic compute gas).
    pub(crate) fn current_frame_remaining(&self) -> u64 {
        match self.frame_stack.last() {
            Some(entry) => entry.remaining(),
            None => self.tx_entry.remaining(),
        }
    }

    /// Returns the maximum limit that can be forwarded to the next frame.
    /// This is a utility function to help calculate the limit for the next frame.
    fn max_forward_limit(&self) -> u64 {
        match self.frame_stack.last() {
            Some(entry) => {
                let remaining = u128::from(entry.remaining());
                let numerator = u128::from(constants::rex4::FRAME_LIMIT_NUMERATOR);
                let denominator = u128::from(constants::rex4::FRAME_LIMIT_DENOMINATOR);
                ((remaining * numerator) / denominator) as u64
            }
            None => self.tx_entry.limit,
        }
    }

    pub(crate) fn push_frame(&mut self, info: I) {
        self.frame_stack.push(FrameLimitEntry::new(self.max_forward_limit(), info));
    }

    /// Pops the current frame from the stack and merges its usage into the parent.
    ///
    /// On success: `persistent_usage`, `discardable_usage`, and `refund` are all merged.
    /// On failure: only `persistent_usage` is merged; `discardable_usage` and `refund` are dropped.
    pub(crate) fn pop_frame(&mut self, success: bool) -> Option<FrameLimitEntry<I>> {
        let child = self.frame_stack.pop();
        if let Some(child) = &child {
            if let Some(parent) = self.frame_stack.last_mut() {
                parent.persistent_usage += child.persistent_usage;
                if success {
                    parent.discardable_usage += child.discardable_usage;
                    parent.refund += child.refund;
                }
            } else {
                // Last frame popped — merge into tx_entry.
                self.tx_entry.persistent_usage += child.persistent_usage;
                if success {
                    self.tx_entry.discardable_usage += child.discardable_usage;
                    self.tx_entry.refund += child.refund;
                }
            }
        }
        child
    }

    /// Returns whether the current frame has exceeded its frame-local limit.
    /// If exceeded, `frame_local` is always `true` since this checks per-frame budgets.
    pub(crate) fn exceeds_current_frame_limit(&self, kind: LimitKind) -> LimitCheck {
        match self.frame_stack.last() {
            Some(entry) if entry.used().saturating_sub(entry.refund) > entry.limit => {
                LimitCheck::ExceedsLimit {
                    kind,
                    limit: entry.limit,
                    used: entry.used(),
                    frame_local: true,
                }
            }
            _ => LimitCheck::WithinLimit,
        }
    }

    /// Returns a mutable reference to the TX-level entry.
    pub(crate) fn tx_mut(&mut self) -> &mut FrameLimitEntry<()> {
        &mut self.tx_entry
    }

    /// Returns a mutable reference to the current (top) frame entry.
    pub(crate) fn frame_mut(&mut self) -> Option<&mut FrameLimitEntry<I>> {
        self.frame_stack.last_mut()
    }

    /// Returns whether there is at least one active frame on the stack.
    pub(crate) fn has_active_frame(&self) -> bool {
        !self.frame_stack.is_empty()
    }

    /// Pushes a new frame with a custom limit, bypassing the 98/100 calculation.
    /// Used by pre-Rex4 specs where per-frame limits are not enforced.
    pub(crate) fn push_frame_with_limit(&mut self, limit: u64, info: I) {
        self.frame_stack.push(FrameLimitEntry::new(limit, info));
    }

    /// Returns the total net usage across `tx_entry` and all frames on the stack.
    /// Net usage = Σ(`persistent_usage` + `discardable_usage`) - Σ(refund), clamped to 0.
    pub(crate) fn net_usage(&self) -> u64 {
        let mut total_used: u64 = self.tx_entry.used();
        let mut total_refund: u64 = self.tx_entry.refund;
        for entry in &self.frame_stack {
            total_used += entry.used();
            total_refund += entry.refund;
        }
        total_used.saturating_sub(total_refund)
    }
}

pub(crate) trait TxRuntimeLimit {
    fn tx_limit(&self) -> u64;
    fn tx_usage(&self) -> u64;
    fn reset(&mut self);
    fn check_limit(&self) -> LimitCheck;

    #[inline]
    fn before_tx_start(&mut self, _tx: &MegaTransaction) {}
    #[inline]
    fn push_empty_frame(&mut self) {}
    #[inline]
    fn before_frame_init<JOURNAL: JournalInspectTr<DBError: core::fmt::Debug>>(
        &mut self,
        _frame_init: &FrameInit,
        _journal: &mut JOURNAL,
    ) -> Result<(), JOURNAL::DBError> {
        Ok(())
    }
    #[inline]
    fn after_frame_init_on_frame(&mut self, _frame: &EthFrame<EthInterpreter>) {}
    #[inline]
    fn before_frame_run(&mut self, _frame: &EthFrame<EthInterpreter>) {}
    #[inline]
    fn after_frame_run<'a>(
        &mut self,
        _frame: &'a EthFrame<EthInterpreter>,
        _action: &'a mut InterpreterAction,
    ) {
    }
    #[inline]
    fn before_frame_return_result<const LAST_FRAME: bool>(&mut self, _result: &FrameResult) {}
    #[inline]
    fn after_sstore(
        &mut self,
        _target_address: Address,
        _slot: U256,
        _store_result: &SStoreResult,
    ) {
    }
    fn after_log(&mut self, _num_topics: u64, _data_size: u64) {}
}
