//! Unit tests extracted from `crates/mega-evm/src/evm/factory.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/factory.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dyn_precompiles_builder_receives_the_behavior_spec() {
        use alloy_evm::EvmFactory as _;
        use core::sync::atomic::{AtomicU8, Ordering};

        // The builder must see the behavior projection, never an alias rung: an external
        // builder keyed on exact specs would otherwise install a different precompile set
        // during a rollback window.
        static SEEN_SPEC: AtomicU8 = AtomicU8::new(u8::MAX);

        let factory =
            MegaEvmFactory::new().with_dyn_precompiles_builder(std::sync::Arc::new(|spec| {
                SEEN_SPEC.store(spec as u8, Ordering::SeqCst);
                revm::primitives::HashMap::default()
            }));

        let mut evm_env = EvmEnv::<MegaSpecId>::default();
        evm_env.cfg_env.spec = MegaSpecId::MINI_REX_1;
        let _evm = factory.create_evm(crate::test_utils::MemoryDatabase::default(), evm_env);

        assert_eq!(SEEN_SPEC.load(Ordering::SeqCst), MegaSpecId::EQUIVALENCE as u8);
    }
}
