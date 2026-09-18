//! Unit tests extracted from `crates/mega-evm/src/evm/context.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/context.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::address;
    use revm::{context::CfgEnv, database::EmptyDB};

    use crate::TestExternalEnvs;

    #[test]
    fn test_with_cfg_updates_spec() {
        // Create context with initial spec
        let mut context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE);

        // Verify initial state
        assert_eq!(context.mega_spec(), MegaSpecId::EQUIVALENCE);
        assert_eq!(context.inner.cfg.spec, OpSpecId::from(MegaSpecId::EQUIVALENCE));

        // Create new config with different spec
        let new_cfg = CfgEnv::new_with_spec(MegaSpecId::MINI_REX);

        // Apply new config using with_cfg
        context = context.with_cfg(new_cfg);

        // Verify that both the context's spec and inner config's spec are updated
        assert_eq!(context.mega_spec(), MegaSpecId::MINI_REX);
        assert_eq!(context.inner.cfg.spec, OpSpecId::from(MegaSpecId::MINI_REX));
    }

    #[test]
    fn test_with_cfg_spec_consistency() {
        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE);

        // Test multiple spec transitions
        let specs_to_test = [MegaSpecId::MINI_REX, MegaSpecId::EQUIVALENCE];

        let mut current_context = context;
        for spec in specs_to_test {
            let cfg = CfgEnv::new_with_spec(spec);
            current_context = current_context.with_cfg(cfg);

            // Verify consistency between context spec and inner config spec
            assert_eq!(current_context.mega_spec(), spec);
            assert_eq!(current_context.inner.cfg.spec, OpSpecId::from(spec));
        }
    }

    /// Sharing SALT env handles between parent and sandbox must not merge their bucket caches.
    #[test]
    fn test_shared_salt_env_keeps_dynamic_gas_cache_isolated() {
        let external_envs = TestExternalEnvs::new();
        let parent = MegaContext::new(EmptyDB::default(), MegaSpecId::REX4)
            .with_external_envs(external_envs.into());
        let parent_address = address!("0000000000000000000000000000000000100001");
        let sandbox_address = address!("0000000000000000000000000000000000100002");

        parent
            .dynamic_storage_gas_cost
            .borrow_mut()
            .new_account_gas(parent_address)
            .expect("parent bucket lookup should succeed");
        let parent_bucket_ids = parent.accessed_bucket_ids();

        let sandbox =
            MegaContext::<_, TestExternalEnvs<std::convert::Infallible>>::new_with_shared_ext_envs(
                EmptyDB::default(),
                MegaSpecId::REX4,
                Rc::clone(&parent.salt_env),
                Rc::clone(&parent.oracle_env),
            )
            .with_block(parent.block().clone())
            .with_chain(parent.chain().clone())
            .with_inside_sandbox(true);
        sandbox
            .dynamic_storage_gas_cost
            .borrow_mut()
            .new_account_gas(sandbox_address)
            .expect("sandbox bucket lookup should succeed");

        assert_eq!(parent.accessed_bucket_ids(), parent_bucket_ids);
        assert_ne!(sandbox.accessed_bucket_ids(), parent_bucket_ids);
    }

    /// The test/bench-only `new_with_ext_envs` wrapper builds a `MegaContext`
    /// over a caller-supplied external environment (`TestExternalEnvs`), at the
    /// requested spec and wired to the given SALT/oracle handles.
    #[test]
    fn test_new_with_ext_envs_builds_over_configurable_env() {
        let env = TestExternalEnvs::<std::convert::Infallible>::new();
        let context =
            MegaContext::<_, TestExternalEnvs<std::convert::Infallible>>::new_with_ext_envs(
                EmptyDB::default(),
                MegaSpecId::REX5,
                Rc::new(env.clone()),
                Rc::new(RefCell::new(env)),
            );

        assert_eq!(context.mega_spec(), MegaSpecId::REX5);
        // The supplied SALT env is wired through: a bucket lookup against the
        // dynamic-gas cache succeeds.
        context
            .dynamic_storage_gas_cost
            .borrow_mut()
            .new_account_gas(address!("0000000000000000000000000000000000100003"))
            .expect("bucket lookup against the supplied env should succeed");
    }
}
