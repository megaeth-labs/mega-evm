//! Unit tests extracted from `crates/mega-evm/src/limit/compute_gas.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/limit/compute_gas.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    /// Usage exactly equal to the detained limit must NOT count as a detained exceed:
    /// the predicate is strictly-greater-than (`used > detained_limit`), not `>=`.
    #[test]
    fn test_is_detained_exceed_is_strict_at_boundary() {
        // tx_limit = 100, detained lowered to 50 (< tx_limit, so detention is binding).
        let mut tracker = ComputeGasTracker::new(MegaSpecId::MINI_REX, 100);
        tracker.set_detained_limit(50);

        // Usage exactly at the detained limit: not exceeded.
        tracker.record_gas_used(50);
        assert!(
            !tracker.is_detained_exceed(),
            "usage == detained_limit must not be a detained exceed"
        );

        // One unit over the limit: exceeded.
        tracker.record_gas_used(1);
        assert!(tracker.is_detained_exceed(), "usage > detained_limit must be a detained exceed");
    }
}
