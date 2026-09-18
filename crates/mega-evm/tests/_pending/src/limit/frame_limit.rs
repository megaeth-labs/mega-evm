//! Unit tests extracted from `crates/mega-evm/src/limit/frame_limit.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/limit/frame_limit.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

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
