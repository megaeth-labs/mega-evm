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
    /// Whether Rex6 is active. Used by the `CallFrameInfo` specialization: a CREATE's
    /// creator-side charge survives the child's revert (revm bumps the creator nonce before the
    /// create checkpoint), so under Rex6 `push_create_frame` does not arm the
    /// `charged_parent_update` unwind — a reverted-then-retried CREATE must not charge the
    /// creator account update twice. Rex5 keeps the (over-unwinding) frozen behavior.
    rex6_enabled: bool,

    /// Cached `Σ(persistent_usage + discardable_usage)` across `tx_entry` and every entry on
    /// `frame_stack`. Maintained incrementally so `net_usage()` is O(1) instead of O(depth)
    /// per opcode. See `net_usage()` for the invariant and `pop_frame` for the revert delta.
    cached_total_used: u64,
    /// Cached `Σ refund` across `tx_entry` and every entry on `frame_stack`. Maintained
    /// together with `cached_total_used` so `net_usage = saturating_sub(used, refund)` is
    /// a single subtraction.
    cached_total_refund: u64,
}

/// Per-frame budget entry on the frame stack.
#[derive(Debug, Clone)]
pub(crate) struct FrameLimitEntry<I> {
    /// Maximum usage allowed in this frame.
    pub(crate) limit: u64,

    // The three budget fields below MUST only be mutated through `FrameLimitTracker`'s
    // cache-aware helpers (`add_tx_persistent`, `add_frame_persistent`,
    // `add_frame_discardable`, `add_frame_refund`) or through `pop_frame`'s explicit
    // cache deltas. Writing them directly desyncs `cached_total_used` /
    // `cached_total_refund` and silently corrupts every subsequent `net_usage()` result.
    /// Persistent usage in this frame even if it is reverted.
    persistent_usage: u64,
    /// Discardable usage if this frame is reverted.
    discardable_usage: u64,
    /// Refund usage in this frame.
    refund: u64,

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
            rex6_enabled: spec.is_enabled(MegaSpecId::REX6),
            cached_total_used: 0,
            cached_total_refund: 0,
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
        self.cached_total_used = 0;
        self.cached_total_refund = 0;
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
    ///
    /// Cache invariant: every mutation to `persistent_usage` / `discardable_usage` / `refund`
    /// — whether via the public helpers or via this pop — keeps `cached_total_used` and
    /// `cached_total_refund` in sync. On revert the child's `discardable_usage` and `refund`
    /// are subtracted from the cache because they vanish (they are neither kept on the child
    /// nor merged into the parent).
    pub(crate) fn pop_frame(&mut self, success: bool) -> Option<FrameLimitEntry<I>> {
        let child = self.frame_stack.pop();
        if let Some(child) = &child {
            if let Some(parent) = self.frame_stack.last_mut() {
                // Persistent usage is always preserved: move from child slot to parent slot.
                // The cache already counts it once, so no cache update is needed here.
                parent.persistent_usage += child.persistent_usage;
                if success {
                    // Discardable & refund are preserved on success — same total, just relocated
                    // from child entry to parent entry. Cache stays the same.
                    parent.discardable_usage += child.discardable_usage;
                    parent.refund += child.refund;
                } else {
                    // Revert: child's discardable_usage and refund vanish entirely. Subtract them
                    // from the cache to maintain the `Σ(persistent + discardable) - Σ refund`
                    // invariant.
                    self.cached_total_used -= child.discardable_usage;
                    self.cached_total_refund -= child.refund;
                }
            } else {
                // Last frame popped — merge into tx_entry. Same cache reasoning as the parent
                // branch above.
                self.tx_entry.persistent_usage += child.persistent_usage;
                if success {
                    self.tx_entry.discardable_usage += child.discardable_usage;
                    self.tx_entry.refund += child.refund;
                } else {
                    self.cached_total_used -= child.discardable_usage;
                    self.cached_total_refund -= child.refund;
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
    ///
    /// O(1): served from `cached_total_used` / `cached_total_refund`, which are maintained
    /// incrementally by `add_tx_persistent`, `add_frame_persistent`, `add_frame_discardable`,
    /// `add_frame_refund`, and `pop_frame`.
    #[inline]
    pub(crate) fn net_usage(&self) -> u64 {
        let net_usage = self.cached_total_used.saturating_sub(self.cached_total_refund);
        #[cfg(debug_assertions)]
        assert_eq!(
            net_usage,
            self.net_usage_uncached(),
            "cached net_usage must match uncached reference"
        );
        net_usage
    }

    /// Reference implementation of `net_usage` that walks `tx_entry + frame_stack` directly.
    /// Active only in debug builds (gated by `cfg(debug_assertions)`), where it backs the
    /// per-call assertion inside `net_usage()` so the incremental cache is verified against
    /// the slow O(depth) walk on every opcode in tests and dev runs. Release builds
    /// (including `cargo bench` and production) compile this out entirely.
    #[cfg(any(debug_assertions, test))]
    fn net_usage_uncached(&self) -> u64 {
        let mut total_used: u64 = self.tx_entry.used();
        let mut total_refund: u64 = self.tx_entry.refund;
        for entry in &self.frame_stack {
            total_used += entry.used();
            total_refund += entry.refund;
        }
        total_used.saturating_sub(total_refund)
    }

    /// Adds `n` to `tx_entry.persistent_usage` and keeps the cache in sync.
    ///
    /// Used by trackers to record intrinsic / pre-frame usage (e.g. base TX size,
    /// EIP-7702 authority updates, caller account update). The current frame stack
    /// may be empty (pre-`frame_init`) or non-empty (`compute_gas`'s fallback when no
    /// frame exists).
    #[inline]
    pub(crate) fn add_tx_persistent(&mut self, n: u64) {
        self.tx_entry.persistent_usage += n;
        self.cached_total_used += n;
    }

    /// Adds `n` to the current top frame's `persistent_usage` and keeps the cache in sync.
    /// Returns `false` (and does nothing) when the frame stack is empty — callers that
    /// need a tx-level fallback must check the return value.
    #[inline]
    pub(crate) fn add_frame_persistent(&mut self, n: u64) -> bool {
        match self.frame_stack.last_mut() {
            Some(entry) => {
                entry.persistent_usage += n;
                self.cached_total_used += n;
                true
            }
            None => false,
        }
    }

    /// Adds `n` to the current top frame's `discardable_usage` and keeps the cache in sync.
    /// No-op when the frame stack is empty (discardable usage requires a frame to be
    /// discardable into; pre-frame usage is always persistent).
    #[inline]
    pub(crate) fn add_frame_discardable(&mut self, n: u64) {
        if let Some(entry) = self.frame_stack.last_mut() {
            entry.discardable_usage += n;
            self.cached_total_used += n;
        }
    }

    /// Adds `n` to the current top frame's `refund` and keeps the cache in sync.
    /// No-op when the frame stack is empty.
    #[inline]
    pub(crate) fn add_frame_refund(&mut self, n: u64) {
        if let Some(entry) = self.frame_stack.last_mut() {
            entry.refund += n;
            self.cached_total_refund += n;
        }
    }

    /// Adds `n` to the parent frame's `discardable_usage` (the frame directly beneath the
    /// current top) and keeps the cache in sync. No-op when the current frame is top-level.
    ///
    /// Used by REX6 CREATE accounting to charge the creator's nonce-bump account-info write to
    /// the parent (creator) frame instead of the child (created) frame. revm increments the
    /// creator's nonce *before* it takes the create checkpoint, so the nonce bump survives the
    /// created contract's revert and is undone only if the creator's own frame reverts.
    /// Recording the charge on the parent's discardable lane matches that lifetime: it is kept
    /// when the child reverts and dropped only with the parent.
    ///
    /// This is the opposite of a value-transferring CALL, whose balance moves *are* rolled back
    /// when the callee reverts — those are correctly charged to the child's discardable lane.
    #[inline]
    pub(crate) fn add_parent_discardable(&mut self, n: u64) {
        if let Some((_current, below)) = self.frame_stack.split_last_mut() {
            if let Some(parent) = below.last_mut() {
                parent.discardable_usage += n;
                self.cached_total_used += n;
            }
        }
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
    pub(crate) fn push_dummy_frame(&mut self) {
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
        // Rex6: the creator's nonce bump survives the child CREATE's revert, and its charge
        // lives on the parent's discardable lane — do not arm the unwind, or a
        // revert-then-retry CREATE re-charges the same creator account update. Rex5 keeps the
        // frozen unwind (its charge lived on the child lane and was dropped with it).
        let charged_parent_update = rex5_enabled && parent_needs_update && !self.rex6_enabled;
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
    pub(crate) fn pop_frame_unwind_parent(&mut self, success: bool) {
        let child = self.pop_frame(success);
        if !success {
            if let Some(child_entry) = child {
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

#[cfg(test)]
mod tests {
    use alloy_primitives::address;

    use super::*;

    const ADDR: Address = address!("0000000000000000000000000000000000001234");

    /// `set_created_address` on an empty frame stack must be a no-op.
    #[test]
    fn test_set_created_address_empty_stack_is_noop() {
        let mut t = FrameLimitTracker::<CallFrameInfo>::new(MegaSpecId::EQUIVALENCE, u64::MAX);
        // No frame on the stack — should not panic, just do nothing.
        t.set_created_address(ADDR);
    }

    /// `set_created_address` panics when called twice on the same CREATE frame
    /// (invariant: `target_address` must be `None` when first filled in).
    #[test]
    #[should_panic(expected = "created account already recorded")]
    fn test_set_created_address_duplicate_panics() {
        let mut t = FrameLimitTracker::<CallFrameInfo>::new(MegaSpecId::EQUIVALENCE, u64::MAX);
        t.push_create_frame();
        t.set_created_address(ADDR);
        t.set_created_address(ADDR); // second call must panic
    }

    /// Drives `FrameLimitTracker` through a representative sequence of pushes, mutations,
    /// and pops (both success and revert) and asserts that the cached `net_usage()` stays
    /// in sync with the uncached reference walk after every step. This is the load-bearing
    /// guarantee of the cache-based refactor: any future change that introduces a new
    /// mutation site without going through a helper will break this test.
    #[test]
    fn test_net_usage_cache_matches_uncached() {
        let mut t = FrameLimitTracker::<()>::new(MegaSpecId::EQUIVALENCE, u64::MAX);
        assert_eq!(t.net_usage(), t.net_usage_uncached());
        assert_eq!(t.net_usage(), 0);

        // Pre-frame intrinsic usage goes to tx_entry.
        t.add_tx_persistent(100);
        assert_eq!(t.net_usage(), t.net_usage_uncached());

        // add_frame_persistent on empty stack must be a no-op and return false.
        assert!(!t.add_frame_persistent(50));
        assert_eq!(t.net_usage(), t.net_usage_uncached());
        // add_frame_discardable / add_frame_refund are no-ops on empty stack.
        t.add_frame_discardable(50);
        t.add_frame_refund(50);
        assert_eq!(t.net_usage(), t.net_usage_uncached());
        assert_eq!(t.net_usage(), 100);

        // Push frame 1 and mix in persistent/discardable/refund.
        t.push_frame(());
        assert!(t.add_frame_persistent(20));
        t.add_frame_discardable(30);
        t.add_frame_refund(10);
        assert_eq!(t.net_usage(), t.net_usage_uncached());

        // Push frame 2 (nested) and mutate.
        t.push_frame(());
        assert!(t.add_frame_persistent(7));
        t.add_frame_discardable(15);
        t.add_frame_refund(3);
        assert_eq!(t.net_usage(), t.net_usage_uncached());

        // Push frame 3 (deeper) and revert it — discardable & refund must vanish from cache,
        // persistent must merge into the parent (frame 2).
        t.push_frame(());
        assert!(t.add_frame_persistent(5));
        t.add_frame_discardable(11);
        t.add_frame_refund(2);
        let before_revert = t.net_usage();
        let popped = t.pop_frame(false).expect("frame 3 popped");
        assert_eq!(popped.discardable_usage, 11);
        assert_eq!(popped.refund, 2);
        assert_eq!(t.net_usage(), t.net_usage_uncached());
        // After revert: cache should drop the child's discardable (11) and refund (2),
        // so net_usage changes by -(11) + 2 = -9 vs before.
        assert_eq!(t.net_usage(), before_revert - 9);

        // Push frame 3 again and pop it successfully — discardable/refund merge into parent,
        // cache is unchanged by the pop itself (totals are invariant under transfer).
        t.push_frame(());
        assert!(t.add_frame_persistent(4));
        t.add_frame_discardable(6);
        t.add_frame_refund(1);
        let before_success = t.net_usage();
        t.pop_frame(true);
        assert_eq!(t.net_usage(), t.net_usage_uncached());
        assert_eq!(t.net_usage(), before_success);

        // Pop frame 2 with success → merge into frame 1.
        t.pop_frame(true);
        assert_eq!(t.net_usage(), t.net_usage_uncached());

        // Pop the last frame (frame 1) with revert → discardable/refund vanish, persistent
        // merges into tx_entry.
        let before_last_revert = t.net_usage();
        let frame_1 = t.pop_frame(false).expect("frame 1 popped");
        assert_eq!(t.net_usage(), t.net_usage_uncached());
        // The popped frame's discardable/refund leave the cache.
        assert_eq!(t.net_usage(), before_last_revert - frame_1.discardable_usage + frame_1.refund);

        // Reset returns the cache to zero in sync with the entries.
        t.reset();
        assert_eq!(t.net_usage(), t.net_usage_uncached());
        assert_eq!(t.net_usage(), 0);
    }

    /// Verifies that a refund exceeding the cumulative usage clamps `net_usage()` to 0,
    /// matching the saturating semantics of the uncached reference. This guards against
    /// signed-style accounting bugs where refunds outrun used and an unsigned subtraction
    /// would otherwise wrap.
    #[test]
    fn test_net_usage_saturates_when_refund_exceeds_used() {
        let mut t = FrameLimitTracker::<()>::new(MegaSpecId::EQUIVALENCE, u64::MAX);
        t.push_frame(());
        t.add_frame_discardable(10);
        t.add_frame_refund(100);
        assert_eq!(t.net_usage(), 0);
        assert_eq!(t.net_usage(), t.net_usage_uncached());
    }
}
