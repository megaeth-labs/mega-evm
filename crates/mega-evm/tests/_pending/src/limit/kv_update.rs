//! Unit tests extracted from `crates/mega-evm/src/limit/kv_update.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/limit/kv_update.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    /// `record_account_update` must charge exactly one KV update against the current frame
    /// (used by REX5+ SELFDESTRUCT-beneficiary metering); it must not be a no-op.
    #[test]
    fn test_record_account_update_charges_one_kv() {
        let mut tracker = KVUpdateTracker::new(MegaSpecId::MINI_REX, u64::MAX);
        tracker.push_empty_frame();
        assert_eq!(tracker.tx_usage(), 0);
        tracker.record_account_update();
        assert_eq!(tracker.tx_usage(), 1, "record_account_update must add exactly 1 KV update");
    }
}
