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

use crate::{constants, JournalInspectTr, MegaSpecId, MegaTransaction};

use super::{LimitCheck, LimitKind};

/// Per-frame metadata for trackers that need account update deduplication
/// (data size and KV update trackers).
///
/// All fields are private to this module: the
/// `push_call_frame` / `push_create_frame` / `pop_frame_unwind_parent` methods on
/// `FrameLimitTracker<CallFrameInfo>` own the invariant that wires `target_updated` and
/// `charged_parent_update` together. Callers should not touch these fields directly.
#[derive(Debug, Clone, Default)]
pub(crate) struct CallFrameInfo {
    /// The target address of the frame. `None` during CREATE until the address is known.
    target_address: Option<Address>,
    /// Whether this frame's target address has been marked as updated.
    target_updated: bool,
    /// True when this frame caused the parent's `target_updated` to be flipped to `true`
    /// (Rex5+ only). Used by `pop_frame_unwind_parent` to undo the parent flag on revert,
    /// so a subsequent successful call from the same parent still charges the parent account.
    charged_parent_update: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FrameLimitTracker<I> {
    /// Top-level (TX-scope) entry. Holds the TX limit and accumulates usage
    /// from pre-frame data and from the last frame pop.
    tx_entry: FrameLimitEntry<()>,
    /// Stack of child frame entries.
    frame_stack: Vec<FrameLimitEntry<I>>,
    /// Whether Rex4 (per-frame budgeting) is active. Used by the `CallFrameInfo`
    /// specialization to choose between `push_frame` (Rex4+) and `push_frame_with_limit`
    /// (`u64::MAX` for pre-Rex4) when constructing call/create/intercept frames.
    /// Other instantiations (`FrameLimitTracker<()>`) currently do not consult this flag.
    rex4_enabled: bool,
    /// Whether Rex5 (parent account update dedup) is active. Used by the `CallFrameInfo`
    /// specialization to gate the `target_updated` mutation in `push_call_frame` /
    /// `push_create_frame` and the matching unwind in `pop_frame_unwind_parent`.
    /// Other instantiations do not consult this flag.
    rex5_enabled: bool,
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
    pub(crate) fn new(spec: MegaSpecId, tx_limit: u64) -> Self {
        Self {
            tx_entry: FrameLimitEntry::new(tx_limit, ()),
            frame_stack: Vec::new(),
            rex4_enabled: spec.is_enabled(MegaSpecId::REX4),
            rex5_enabled: spec.is_enabled(MegaSpecId::REX5),
        }
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

    /// Returns the remaining budget of the current frame.
    ///
    /// If the frame stack is non-empty, returns the top frame's remaining budget.
    /// If the frame stack is empty (before the first frame is pushed), returns
    /// the TX-level remaining which accounts for pre-frame usage (e.g. intrinsic charges).
    pub(crate) fn current_frame_remaining(&self) -> u64 {
        match self.frame_stack.last() {
            Some(entry) => entry.remaining(),
            None => self.tx_entry.remaining(),
        }
    }

    /// Returns the maximum limit that can be forwarded to the next frame.
    ///
    /// - **Nested frame**: parent's remaining × 98/100.
    /// - **Top-level frame** (empty stack): `tx_entry.remaining()`, which accounts for pre-frame
    ///   intrinsic usage (e.g., calldata size, access lists) already charged into `tx_entry`.
    ///
    /// Individual trackers that need a different top-level budget should use
    /// `push_frame_with_limit()` instead of `push_frame()`.
    fn max_forward_limit(&self) -> u64 {
        match self.frame_stack.last() {
            Some(entry) => {
                let remaining = u128::from(entry.remaining());
                let numerator = u128::from(constants::rex4::FRAME_LIMIT_NUMERATOR);
                let denominator = u128::from(constants::rex4::FRAME_LIMIT_DENOMINATOR);
                ((remaining * numerator) / denominator) as u64
            }
            None => self.tx_entry.remaining(),
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

    /// Pushes a new frame with a custom limit, bypassing `max_forward_limit()`.
    ///
    /// Used by pre-Rex4 specs (`u64::MAX`, per-frame limits not enforced) and by any tracker that
    /// intentionally wants to bypass the default `max_forward_limit()` behavior.
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

impl FrameLimitTracker<CallFrameInfo> {
    /// Pushes a new `CallFrameInfo` frame using the per-spec budget choice:
    /// - **Rex4+**: `max_forward_limit()` (parent × 98/100, or `tx_entry.remaining()` at top
    ///   level).
    /// - **Pre-Rex4**: `u64::MAX` since per-frame limits are not enforced.
    fn push_call_frame_info(&mut self, info: CallFrameInfo) {
        if self.rex4_enabled {
            self.push_frame(info);
        } else {
            self.push_frame_with_limit(u64::MAX, info);
        }
    }

    /// Pushes a synthetic frame for inspector / access-control interception paths.
    ///
    /// No parent-flag mutation: an intercepted call is short-circuited before any
    /// state-modifying account update is recorded, so the parent does not need to be marked.
    /// The frame exists only to keep the per-tracker frame stack aligned with the EVM's call
    /// stack so that the matching `pop_frame_unwind_parent` finds an entry to pop.
    pub(crate) fn push_intercept_frame(&mut self) {
        self.push_call_frame_info(CallFrameInfo::default());
    }

    /// Pushes a CALL frame and updates the parent's dedup flag.
    ///
    /// Returns `true` when the parent's account info should be charged by the caller
    /// (i.e., the parent has not been marked updated yet *and* this call transfers value).
    /// Rex5+: the parent's `target_updated` flag is set so subsequent value-transferring
    /// calls from the same parent frame don't double-charge the caller account; the
    /// `charged_parent_update` flag is recorded on the new child so a revert can unwind it.
    /// Pre-Rex5: the parent flag is left untouched, preserving pre-Rex5 semantics.
    pub(crate) fn push_call_frame(&mut self, target: Address, has_transfer: bool) -> bool {
        let rex5_enabled = self.rex5_enabled;
        let parent_needs_update = has_transfer &&
            match self.frame_mut() {
                Some(entry) if !entry.info.target_updated => {
                    if rex5_enabled {
                        entry.info.target_updated = true;
                    }
                    true
                }
                _ => false,
            };
        let charged_parent_update = rex5_enabled && parent_needs_update;
        self.push_call_frame_info(CallFrameInfo {
            target_address: Some(target),
            target_updated: has_transfer,
            charged_parent_update,
        });
        parent_needs_update
    }

    /// Pushes a CREATE frame and updates the parent's dedup flag.
    ///
    /// Returns `true` when the parent's account info should be charged by the caller
    /// (i.e., the parent has not been marked updated yet — CREATE always increments the
    /// caller's nonce, so there is no `has_transfer` gate).
    /// The created address is unknown at push time; callers should fill it in via
    /// `set_created_address` once `frame_init` completes.
    /// Rex5+ deduplication semantics match `push_call_frame`.
    pub(crate) fn push_create_frame(&mut self) -> bool {
        let rex5_enabled = self.rex5_enabled;
        let parent_needs_update = match self.frame_mut() {
            Some(entry) if !entry.info.target_updated => {
                if rex5_enabled {
                    entry.info.target_updated = true;
                }
                true
            }
            _ => false,
        };
        let charged_parent_update = rex5_enabled && parent_needs_update;
        self.push_call_frame_info(CallFrameInfo {
            target_address: None,
            target_updated: true,
            charged_parent_update,
        });
        parent_needs_update
    }

    /// Records the created address on the current CREATE frame.
    ///
    /// Asserts that the current frame's `target_address` has not already been set,
    /// matching the previous inline behavior. Does nothing if the frame stack is empty.
    pub(crate) fn set_created_address(&mut self, addr: Address) {
        if let Some(entry) = self.frame_mut() {
            assert!(entry.info.target_address.is_none(), "created account already recorded");
            entry.info.target_address = Some(addr);
        }
    }

    /// Pops the current frame and, on revert, unwinds the parent's `target_updated` flag
    /// if this child set it.
    ///
    /// Rex5+ only: when the reverting child had `charged_parent_update` set, the parent's
    /// `target_updated` is reset so the next successful call from the same parent still
    /// charges the parent account (avoiding undercounting after a revert-then-retry pattern).
    /// Pre-Rex5 child frames have `charged_parent_update = false`, so this method behaves
    /// identically to `pop_frame` for older specs.
    pub(crate) fn pop_frame_unwind_parent(
        &mut self,
        success: bool,
    ) -> Option<FrameLimitEntry<CallFrameInfo>> {
        let child = self.pop_frame(success);
        if !success {
            if let Some(child_entry) = &child {
                if child_entry.info.charged_parent_update {
                    // charged_parent_update=true implies a parent frame exists
                    // (the flag is only set when frame_mut() returned Some).
                    self.frame_mut()
                        .expect("parent frame must exist when charged_parent_update is true")
                        .info
                        .target_updated = false;
                }
            }
        }
        child
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
    #[inline]
    fn after_selfdestruct(&mut self, _refund: u64) {}
}
