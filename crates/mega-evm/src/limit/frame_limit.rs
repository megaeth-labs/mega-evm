//! The per-frame tracker of data-size bytes and write records.
//!
//! Every frame on the call stack has a lane. What a frame writes is counted on its lane; when the
//! frame returns, a success merges the lane into its caller's and a failure discards it, the way
//! the journal keeps or reverts the writes themselves. What is counted outside any frame (the
//! records of applied EIP-7702 authorities) and what the outermost frame kept sit on the
//! transaction's own lane.
//!
//! Every operation is O(1): the totals are cached and kept in step with each change.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_primitives::Address;

use super::{LimitUsage, WRITE_RECORD};

/// One frame's lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lane {
    /// What the frame, and the children it kept, counted.
    pub(crate) used: LimitUsage,
    /// What the frame, and the children it kept, took back: a slot written back to its original
    /// value.
    pub(crate) refund: LimitUsage,
    /// The account the frame runs as. `None` until known (a creation learns it at frame init).
    pub(crate) address: Option<Address>,
    /// Whether a write to the frame's own account is already counted, so a value transfer or a
    /// creation from the frame does not count it again.
    pub(crate) account_recorded: bool,
    /// Whether this lane holds the record of its caller's account, made when the frame started
    /// (a value transfer's sender, a creation's creator). Its failure discards that record, so the
    /// caller's account is no longer recorded either.
    pub(crate) holds_caller_record: bool,
    /// Whether the caller's record on this lane is a creator's nonce, which survives the
    /// creation's failure once the nonce was bumped.
    pub(crate) creator_record: bool,
    /// The most data-size bytes the frame may keep; `u64::MAX` when it has no budget.
    pub(crate) budget: u64,
}

impl Lane {
    /// A lane for a frame running as `address`.
    pub(crate) const fn new(address: Option<Address>, account_recorded: bool, budget: u64) -> Self {
        Self {
            used: LimitUsage::ZERO,
            refund: LimitUsage::ZERO,
            address,
            account_recorded,
            holds_caller_record: false,
            creator_record: false,
            budget,
        }
    }

    /// A lane for a frame that does not run: a result built without an interpreter.
    pub(crate) const fn empty() -> Self {
        Self::new(None, false, u64::MAX)
    }

    /// What the frame keeps if it succeeds.
    pub(crate) const fn net(&self) -> LimitUsage {
        self.used.saturating_sub(self.refund)
    }

    /// The data-size bytes the frame may still keep.
    pub(crate) const fn remaining_budget(&self) -> u64 {
        self.budget.saturating_sub(self.net().data_size)
    }
}

/// The lanes of the running transaction.
#[derive(Clone, Debug, Default)]
pub(crate) struct FrameLimitTracker {
    /// What the transaction counted outside any running frame, and what the outermost frame kept.
    tx_used: LimitUsage,
    /// What the outermost frame kept of its refunds.
    tx_refund: LimitUsage,
    /// One lane per frame on the call stack, the running frame last.
    lanes: Vec<Lane>,
    /// `tx_used` plus every lane's `used`.
    total_used: LimitUsage,
    /// `tx_refund` plus every lane's `refund`.
    total_refund: LimitUsage,
}

impl FrameLimitTracker {
    /// Clears the tracker for a new transaction, keeping its allocation.
    pub(crate) fn reset(&mut self) {
        self.tx_used = LimitUsage::ZERO;
        self.tx_refund = LimitUsage::ZERO;
        self.lanes.clear();
        self.total_used = LimitUsage::ZERO;
        self.total_refund = LimitUsage::ZERO;
    }

    /// The number of lanes, which is the number of frames on the call stack.
    pub(crate) fn depth(&self) -> usize {
        self.lanes.len()
    }

    /// What the transaction keeps if every running frame succeeds.
    pub(crate) const fn net(&self) -> LimitUsage {
        self.total_used.saturating_sub(self.total_refund)
    }

    /// The running frame's lane.
    pub(crate) fn current(&self) -> Option<&Lane> {
        self.lanes.last()
    }

    /// The running frame's lane, mutably.
    pub(crate) fn current_mut(&mut self) -> Option<&mut Lane> {
        self.lanes.last_mut()
    }

    /// Pushes the lane of a frame that starts.
    pub(crate) fn push(&mut self, lane: Lane) {
        self.total_used = self.total_used.saturating_add(lane.used);
        self.total_refund = self.total_refund.saturating_add(lane.refund);
        self.lanes.push(lane);
    }

    /// Counts `usage` on the running frame's lane, or on the transaction's outside any frame.
    pub(crate) fn record(&mut self, usage: LimitUsage) {
        match self.lanes.last_mut() {
            Some(lane) => lane.used = lane.used.saturating_add(usage),
            None => self.tx_used = self.tx_used.saturating_add(usage),
        }
        self.total_used = self.total_used.saturating_add(usage);
    }

    /// Takes `usage` back on the running frame's lane. Outside any frame nothing is taken back.
    pub(crate) fn refund(&mut self, usage: LimitUsage) {
        if let Some(lane) = self.lanes.last_mut() {
            lane.refund = lane.refund.saturating_add(usage);
            self.total_refund = self.total_refund.saturating_add(usage);
        }
    }

    /// Records the running frame's caller's account on the running frame's lane, unless the
    /// caller's lane already counts it. A `creator` record is a creation's nonce.
    ///
    /// Must run right after the running frame's lane was pushed, with the caller's lane below it.
    pub(crate) fn record_caller(&mut self, creator: bool) {
        let [.., caller, lane] = self.lanes.as_mut_slice() else { return };
        if caller.account_recorded {
            return;
        }
        caller.account_recorded = true;
        lane.holds_caller_record = true;
        lane.creator_record = creator;
        lane.used = lane.used.saturating_add(WRITE_RECORD);
        self.total_used = self.total_used.saturating_add(WRITE_RECORD);
    }

    /// Drops the caller's record from the running frame's lane: the frame failed before the
    /// write it stands for happened (a creation that did not bump the creator's nonce).
    pub(crate) fn drop_caller_record(&mut self) {
        let [.., caller, lane] = self.lanes.as_mut_slice() else { return };
        if !lane.holds_caller_record {
            return;
        }
        caller.account_recorded = false;
        lane.holds_caller_record = false;
        lane.creator_record = false;
        lane.used = lane.used.saturating_sub(WRITE_RECORD);
        self.total_used = self.total_used.saturating_sub(WRITE_RECORD);
    }

    /// Pops the lane of the frame that returned: `success` merges it into its caller's lane (or
    /// the transaction's), a failure discards it.
    ///
    /// A creator's record outlives the creation's failure; any other record of the caller dies
    /// with it, and the caller's account stops counting as recorded.
    pub(crate) fn pop(&mut self, success: bool) -> Option<Lane> {
        let lane = self.lanes.pop()?;
        if success {
            match self.lanes.last_mut() {
                Some(caller) => {
                    caller.used = caller.used.saturating_add(lane.used);
                    caller.refund = caller.refund.saturating_add(lane.refund);
                    if lane.address.is_some() && lane.address == caller.address {
                        caller.account_recorded |= lane.account_recorded;
                    }
                }
                None => {
                    self.tx_used = self.tx_used.saturating_add(lane.used);
                    self.tx_refund = self.tx_refund.saturating_add(lane.refund);
                }
            }
            return Some(lane);
        }
        self.total_used = self.total_used.saturating_sub(lane.used);
        self.total_refund = self.total_refund.saturating_sub(lane.refund);
        if let Some(caller) = self.lanes.last_mut() {
            if lane.creator_record {
                caller.used = caller.used.saturating_add(WRITE_RECORD);
                self.total_used = self.total_used.saturating_add(WRITE_RECORD);
            } else if lane.holds_caller_record {
                caller.account_recorded = false;
            }
        }
        Some(lane)
    }

    /// [`net`](Self::net) recomputed from the lanes, for checking the cache.
    #[cfg(test)]
    pub(crate) fn net_uncached(&self) -> LimitUsage {
        let mut used = self.tx_used;
        let mut refund = self.tx_refund;
        for lane in &self.lanes {
            used = used.saturating_add(lane.used);
            refund = refund.saturating_add(lane.refund);
        }
        used.saturating_sub(refund)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const ADDR: Address = address!("0000000000000000000000000000000000001234");

    const fn bytes(data_size: u64) -> LimitUsage {
        LimitUsage { data_size, write_records: 0 }
    }

    /// Recording a caller on an empty stack, or on a lane with no caller below it, is a no-op.
    #[test]
    fn test_record_caller_without_a_caller_lane_is_noop() {
        let mut t = FrameLimitTracker::default();
        t.record_caller(true);
        assert_eq!(t.net(), LimitUsage::ZERO);
        t.push(Lane::new(Some(ADDR), false, u64::MAX));
        t.record_caller(true);
        assert_eq!(t.net(), LimitUsage::ZERO);
        assert!(!t.current().unwrap().holds_caller_record);
    }

    /// A frame's caller is recorded once per caller frame: a second child finds it recorded.
    #[test]
    fn test_record_caller_records_a_caller_once() {
        let mut t = FrameLimitTracker::default();
        t.push(Lane::new(Some(ADDR), false, u64::MAX));
        t.push(Lane::new(None, true, u64::MAX));
        t.record_caller(true);
        assert!(t.pop(true).is_some());
        t.push(Lane::new(None, true, u64::MAX));
        t.record_caller(true);
        assert!(!t.current().unwrap().holds_caller_record, "the caller is recorded already");
        assert_eq!(t.net(), WRITE_RECORD);
    }

    /// The cached totals match the lanes after every push, record, refund and pop.
    #[test]
    fn test_net_usage_cache_matches_uncached() {
        let mut t = FrameLimitTracker::default();
        assert_eq!(t.net(), t.net_uncached());
        assert_eq!(t.net(), LimitUsage::ZERO);

        // Outside any frame, usage goes to the transaction and nothing is taken back.
        t.record(bytes(100));
        t.refund(bytes(50));
        assert_eq!(t.net(), t.net_uncached());
        assert_eq!(t.net(), bytes(100));

        // Frame 1.
        t.push(Lane::new(Some(ADDR), false, u64::MAX));
        t.record(bytes(30));
        t.refund(bytes(10));
        assert_eq!(t.net(), t.net_uncached());

        // Frame 2, nested.
        t.push(Lane::new(None, false, u64::MAX));
        t.record(bytes(15));
        t.refund(bytes(3));
        assert_eq!(t.net(), t.net_uncached());

        // Frame 3 fails: its usage and refund leave the totals.
        t.push(Lane::new(None, false, u64::MAX));
        t.record(bytes(11));
        t.refund(bytes(2));
        let before_revert = t.net();
        let popped = t.pop(false).expect("frame 3 popped");
        assert_eq!((popped.used, popped.refund), (bytes(11), bytes(2)));
        assert_eq!(t.net(), t.net_uncached());
        assert_eq!(t.net(), bytes(before_revert.data_size - 9));

        // Frame 3 again, succeeding: merging moves usage, the totals do not change.
        t.push(Lane::new(None, false, u64::MAX));
        t.record(bytes(6));
        t.refund(bytes(1));
        let before_success = t.net();
        t.pop(true);
        assert_eq!(t.net(), t.net_uncached());
        assert_eq!(t.net(), before_success);

        // Frame 2 succeeds into frame 1; frame 1 fails.
        t.pop(true);
        assert_eq!(t.net(), t.net_uncached());
        let frame_1 = t.pop(false).expect("frame 1 popped");
        assert_eq!(t.net(), t.net_uncached());
        assert_eq!(t.net(), bytes(100), "only the transaction's own usage is left");
        assert_eq!(frame_1.net(), bytes(30 + 15 + 6 - 10 - 3 - 1));

        t.reset();
        assert_eq!(t.net(), t.net_uncached());
        assert_eq!(t.net(), LimitUsage::ZERO);
        assert_eq!(t.depth(), 0);
    }

    /// A refund larger than the usage clamps the net at zero.
    #[test]
    fn test_net_usage_saturates_when_refund_exceeds_used() {
        let mut t = FrameLimitTracker::default();
        t.push(Lane::new(None, false, u64::MAX));
        t.record(bytes(10));
        t.refund(bytes(100));
        assert_eq!(t.net(), LimitUsage::ZERO);
        assert_eq!(t.net(), t.net_uncached());
        t.pop(true);
        assert_eq!(t.net(), LimitUsage::ZERO);
        assert_eq!(t.net(), t.net_uncached());
    }

    /// A value transfer's sender record dies with the child that failed, and the caller can be
    /// recorded again; a creator's record survives the creation's failure.
    #[test]
    fn test_failed_child_discards_sender_record_but_keeps_creator_record() {
        let mut t = FrameLimitTracker::default();
        t.push(Lane::new(Some(ADDR), false, u64::MAX));

        t.push(Lane::new(None, true, u64::MAX));
        t.record_caller(false);
        t.record(WRITE_RECORD);
        t.pop(false);
        assert_eq!(t.net(), LimitUsage::ZERO);
        assert!(!t.current().unwrap().account_recorded, "the sender record died with the child");

        t.push(Lane::new(None, true, u64::MAX));
        t.record_caller(true);
        t.record(WRITE_RECORD);
        t.pop(false);
        assert_eq!(t.net(), WRITE_RECORD, "the creator's nonce outlives the creation");
        assert!(t.current().unwrap().account_recorded);
        assert_eq!(t.net(), t.net_uncached());
    }

    /// A creation that fails before bumping the nonce takes the creator record back.
    #[test]
    fn test_drop_caller_record_rearms_the_caller() {
        let mut t = FrameLimitTracker::default();
        t.push(Lane::new(Some(ADDR), false, u64::MAX));
        t.push(Lane::new(None, true, u64::MAX));
        t.record_caller(true);
        t.drop_caller_record();
        t.drop_caller_record();
        assert_eq!(t.net(), LimitUsage::ZERO);
        assert!(!t.lanes[0].account_recorded);
        t.pop(false);
        assert_eq!(t.net(), LimitUsage::ZERO);
        assert_eq!(t.net(), t.net_uncached());
    }

    /// A child running as its caller's account hands its record flag back on success.
    #[test]
    fn test_same_account_child_merges_the_recorded_flag() {
        let mut t = FrameLimitTracker::default();
        t.push(Lane::new(Some(ADDR), false, u64::MAX));
        t.push(Lane::new(Some(ADDR), true, u64::MAX));
        t.pop(true);
        assert!(t.current().unwrap().account_recorded);

        let other = address!("0000000000000000000000000000000000005678");
        t.push(Lane::new(Some(other), false, u64::MAX));
        t.push(Lane::new(Some(other), false, u64::MAX));
        t.current_mut().unwrap().account_recorded = true;
        t.pop(false);
        assert!(!t.current().unwrap().account_recorded, "a failed child hands nothing back");

        t.push(Lane::new(Some(ADDR), true, u64::MAX));
        assert_eq!(t.depth(), 3);
        t.pop(true);
        assert!(!t.current().unwrap().account_recorded, "another account's flag is its own");
        t.push(Lane::new(None, true, u64::MAX));
        t.pop(true);
        assert!(!t.current().unwrap().account_recorded, "an unknown account is not the caller's");
    }
}
