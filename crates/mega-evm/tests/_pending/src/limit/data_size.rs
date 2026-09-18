//! Unit tests extracted from `crates/mega-evm/src/limit/data_size.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/limit/data_size.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    /// The originated data-size constants are *sums* of their salt key/value components,
    /// not products. Pin the values against independent literals so an arithmetic slip
    /// (e.g. `+` -> `*`) is caught (8 + 32 = 40, not 8 * 32 = 256).
    #[test]
    fn test_originated_data_size_constants() {
        assert_eq!(STORAGE_SLOT_WRITE_SIZE, 40);
        assert_eq!(ACCOUNT_INFO_WRITE_SIZE, 40);
    }

    /// `has_active_frame` must reflect the underlying frame stack, not a constant.
    #[test]
    fn test_has_active_frame_reflects_frame_stack() {
        let mut tracker = DataSizeTracker::new(MegaSpecId::MINI_REX, u64::MAX);
        assert!(!tracker.has_active_frame(), "no frame pushed yet");
        tracker.push_empty_frame();
        assert!(tracker.has_active_frame(), "a frame is on the stack");
    }
}
