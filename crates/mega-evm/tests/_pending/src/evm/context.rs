//! Unit tests extracted from `crates/mega-evm/src/evm/context.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/context.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::address;
    use revm::{context::CfgEnv, database::EmptyDB};

    use crate::TestExternalEnvs;

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

}
