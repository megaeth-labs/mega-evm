//! Unit tests extracted from `crates/mega-evm/src/limit/limit.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/limit/limit.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod metering_exemption_tests {
    use super::*;

    /// Tiny per-dimension limits so a single recording trivially exceeds them.
    fn tiny_limits() -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits {
            tx_data_size_limit: 1,
            tx_kv_updates_limit: 1,
            tx_compute_gas_limit: 1,
            tx_state_growth_limit: 1,
            block_env_access_compute_gas_limit: u64::MAX,
            oracle_access_compute_gas_limit: u64::MAX,
        }
    }

    #[test]
    fn test_metering_enforced_when_not_exempt() {
        // Default (non-exempt): halted once compute gas exceeds the (tiny) limit. The REX6 ×
        // system-origin gate that would stamp `Exempt` lives in `MegaContext::on_new_tx`; here we
        // exercise the tracker directly.
        let mut al = AdditionalLimit::new(MegaSpecId::REX6, tiny_limits());
        assert!(!al.record_compute_gas(1_000_000), "non-exempt tx must report exceeded limit");
        assert!(al.check_limit().exceeded_limit());
    }

    #[test]
    fn test_metering_bypassed_when_exempt() {
        // When the system-tx exemption is stamped, `check_limit` short-circuits on the sticky
        // `Exempt` state, covering the four dimensions *and* gas detention, so the same
        // over-limit usage never halts. Usage is still recorded (only the halt decision is
        // suppressed).
        let mut al = AdditionalLimit::new(MegaSpecId::REX6, tiny_limits());
        al.mark_exempt();
        assert!(al.record_compute_gas(1_000_000), "exempt tx must not report exceeded limit");
        assert!(!al.check_limit().exceeded_limit());
        assert!(al.check_limit().is_exempt(), "check_limit must surface the sticky Exempt state");
        assert!(al.get_usage().compute_gas >= 1_000_000, "usage is still accumulated while exempt");
    }

    #[test]
    fn test_detained_compute_gas_does_not_halt_when_exempt() {
        // Gas detention lowers the detained compute-gas limit; enforcement runs through the same
        // `check_limit` chokepoint, so the exemption neutralizes detention too.
        let mut al =
            AdditionalLimit::new(MegaSpecId::REX6, EvmTxRuntimeLimits::from_spec(MegaSpecId::REX6));
        al.mark_exempt();
        al.set_compute_gas_limit(1); // detain hard
        assert!(al.record_compute_gas(10_000_000), "exempt tx must ignore gas detention");
        assert!(!al.check_limit().exceeded_limit());
    }

    #[test]
    fn test_reset_clears_exempt() {
        // The sticky `Exempt` state must not leak to the next transaction reusing the same tracker.
        let mut al = AdditionalLimit::new(MegaSpecId::REX6, tiny_limits());
        al.mark_exempt();
        assert!(al.has_exceeded_limit.is_exempt());
        al.reset();
        assert!(!al.has_exceeded_limit.is_exempt(), "reset must clear the sticky Exempt state");
        assert!(
            al.has_exceeded_limit.within_limit(),
            "reset must restore the WithinLimit baseline"
        );
        assert!(!al.record_compute_gas(1_000_000), "after reset, metering is enforced again");
    }
}

#[cfg(test)]
mod tests {
    use revm::context::tx::TxEnvBuilder;

    use super::{super::LimitKind, *};

    fn test_limits() -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits {
            tx_data_size_limit: 100,
            tx_kv_updates_limit: 1_000,
            tx_compute_gas_limit: 1_000_000,
            tx_state_growth_limit: 1_000,
            block_env_access_compute_gas_limit: 1_000_000,
            oracle_access_compute_gas_limit: 1_000_000,
        }
    }

    /// Returns the latched limit kind, or `None` when within limit.
    fn latched_kind(limit: &AdditionalLimit) -> Option<LimitKind> {
        match limit.has_exceeded_limit {
            LimitCheck::ExceedsLimit { kind, .. } => Some(kind),
            LimitCheck::WithinLimit | LimitCheck::Exempt => None,
        }
    }

}
