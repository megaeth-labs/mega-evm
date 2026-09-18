//! Unit tests extracted from `crates/mega-evm/src/limit/mod.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/limit/mod.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the predicate truth-table for the `Exempt` variant so a future change that flips one
    /// predicate (e.g., reverting `exceeded_limit` to `!matches!(WithinLimit)`) is caught here
    /// rather than silently re-enabling halts for exempt txs.
    #[test]
    fn test_limit_check_exempt_predicate_truth_table() {
        let exempt = LimitCheck::Exempt;
        assert!(!exempt.exceeded_limit());
        assert!(!exempt.within_limit());
        assert!(exempt.is_exempt());
        assert!(!exempt.is_frame_local());
        assert!(exempt.revert_data().is_empty());
        assert!(exempt.maybe_halt_reason().is_none());
    }

    /// `within_limit` must mirror the enum variant exactly, not return a constant.
    #[test]
    fn test_within_limit_reflects_variant() {
        assert!(LimitCheck::WithinLimit.within_limit());
        let exceeded = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 100,
            used: 150,
            frame_local: false,
        };
        assert!(!exceeded.within_limit());
    }

    /// Every `LimitKind` discriminant must survive an `as_u8` -> `from_u8` round-trip,
    /// and unknown discriminants must map to `None`.
    #[test]
    fn test_limit_kind_u8_roundtrip() {
        for kind in [
            LimitKind::DataSize,
            LimitKind::KVUpdate,
            LimitKind::ComputeGas,
            LimitKind::StateGrowth,
        ] {
            assert_eq!(
                LimitKind::from_u8(kind.as_u8()),
                Some(kind),
                "round-trip failed for {kind:?}"
            );
        }
        assert_eq!(LimitKind::from_u8(4), None);
    }
}
