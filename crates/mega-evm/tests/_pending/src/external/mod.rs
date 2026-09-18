//! Unit tests extracted from `crates/mega-evm/src/external/mod.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/external/mod.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OracleEnv, SaltEnv, MIN_BUCKET_SIZE};
    use alloy_primitives::{Address, Bytes, B256, U256};

    #[test]
    fn test_empty_external_env_factory_returns_minimum_bucket_and_no_oracle() {
        let envs = EmptyExternalEnv.external_envs(42);

        assert_eq!(envs.salt_env.get_bucket_capacity(123).unwrap(), MIN_BUCKET_SIZE as u64);
        assert_eq!(envs.oracle_env.get_oracle_storage(U256::from(7)), None);
        envs.oracle_env.on_hint(Address::ZERO, B256::ZERO, Bytes::new());
        assert_eq!(<EmptyExternalEnv as SaltEnv>::bucket_id_for_account(Address::ZERO), 0);
        assert_eq!(<EmptyExternalEnv as SaltEnv>::bucket_id_for_slot(Address::ZERO, U256::ZERO), 0);
    }
}
