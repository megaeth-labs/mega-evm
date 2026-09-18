//! Unit tests extracted from `crates/mega-evm/src/external/gas.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/external/gas.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::test_utils::TestExternalEnvs;

    fn cost_with_capacity(spec: MegaSpecId, capacity: u64) -> DynamicGasCost<TestExternalEnvs> {
        // Map the bucket id that the simple bucket hasher will produce for the zero address /
        // zero slot to the requested capacity.
        let bucket_for_account =
            <TestExternalEnvs as SaltEnv>::bucket_id_for_account(Address::ZERO);
        let bucket_for_slot =
            <TestExternalEnvs as SaltEnv>::bucket_id_for_slot(Address::ZERO, U256::ZERO);
        let env = TestExternalEnvs::new()
            .with_bucket_capacity(bucket_for_account, capacity)
            .with_bucket_capacity(bucket_for_slot, capacity);
        DynamicGasCost::new(spec, env, 0)
    }

    /// `MIN_BUCKET_SIZE * u64::MAX` cannot be represented in `u64`; verify the hardened
    /// arithmetic does not panic and saturates instead of wrapping.
    #[test]
    fn test_sstore_set_gas_saturates_on_huge_multiplier() {
        let mut cost = cost_with_capacity(MegaSpecId::REX, u64::MAX);
        let gas = cost.sstore_set_gas(Address::ZERO, U256::ZERO).unwrap();
        assert_eq!(gas, u64::MAX);

        let mut cost = cost_with_capacity(MegaSpecId::MINI_REX, u64::MAX);
        let gas = cost.sstore_set_gas(Address::ZERO, U256::ZERO).unwrap();
        assert_eq!(gas, u64::MAX);
    }

    #[test]
    fn test_new_account_gas_saturates_on_huge_multiplier() {
        let mut cost = cost_with_capacity(MegaSpecId::REX, u64::MAX);
        let gas = cost.new_account_gas(Address::ZERO).unwrap();
        assert_eq!(gas, u64::MAX);

        let mut cost = cost_with_capacity(MegaSpecId::MINI_REX, u64::MAX);
        let gas = cost.new_account_gas(Address::ZERO).unwrap();
        assert_eq!(gas, u64::MAX);
    }

    #[test]
    fn test_create_contract_gas_saturates_on_huge_multiplier() {
        let mut cost = cost_with_capacity(MegaSpecId::REX, u64::MAX);
        let gas = cost.create_contract_gas(Address::ZERO).unwrap();
        assert_eq!(gas, u64::MAX);

        let mut cost = cost_with_capacity(MegaSpecId::MINI_REX, u64::MAX);
        let gas = cost.create_contract_gas(Address::ZERO).unwrap();
        assert_eq!(gas, u64::MAX);
    }

    /// REX6 system-exempt path: the unscaled cost (`multiplier = 1`) is `0` for the REX-family
    /// formula `base × (multiplier − 1)`, and is independent of the actual bucket capacity. This
    /// is what makes a system call's storage cost immune to SALT bucket scaling.
    #[test]
    fn test_unscaled_gas_is_zero_for_rex_and_independent_of_capacity() {
        for capacity in [MIN_BUCKET_SIZE as u64, 1_280_000, u64::MAX] {
            let cost = cost_with_capacity(MegaSpecId::REX6, capacity);
            assert_eq!(cost.sstore_set_gas_unscaled(), 0);
            assert_eq!(cost.new_account_gas_unscaled(), 0);
            assert_eq!(cost.create_contract_gas_unscaled(), 0);
        }
    }

    /// The unscaled helpers are REX-family API: pin the debug assert exactly at the REX
    /// boundary (the first spec where the `base × (multiplier − 1)` formula exists), so a
    /// tightened gate (e.g. REX1) fails here.
    #[test]
    fn test_unscaled_gas_is_available_from_rex_exactly() {
        let cost = cost_with_capacity(MegaSpecId::REX, MIN_BUCKET_SIZE as u64);
        assert_eq!(cost.sstore_set_gas_unscaled(), 0);
        assert_eq!(cost.new_account_gas_unscaled(), 0);
        assert_eq!(cost.create_contract_gas_unscaled(), 0);
    }

    /// Pre-REX callers must trip the debug assert — a loosened gate (e.g. `MiniRex`) would
    /// silently serve the REX-family formula to a spec that prices storage differently.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "assertion failed")]
    fn test_sstore_set_gas_unscaled_asserts_pre_rex() {
        let _ = cost_with_capacity(MegaSpecId::MINI_REX, MIN_BUCKET_SIZE as u64)
            .sstore_set_gas_unscaled();
    }

    /// See [`test_sstore_set_gas_unscaled_asserts_pre_rex`].
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "assertion failed")]
    fn test_new_account_gas_unscaled_asserts_pre_rex() {
        let _ = cost_with_capacity(MegaSpecId::MINI_REX, MIN_BUCKET_SIZE as u64)
            .new_account_gas_unscaled();
    }

    /// See [`test_sstore_set_gas_unscaled_asserts_pre_rex`].
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "assertion failed")]
    fn test_create_contract_gas_unscaled_asserts_pre_rex() {
        let _ = cost_with_capacity(MegaSpecId::MINI_REX, MIN_BUCKET_SIZE as u64)
            .create_contract_gas_unscaled();
    }

    /// The unscaled result equals the SALT-driven result evaluated at the minimum bucket
    /// capacity, confirming the shared formula helper produces consistent values across both paths.
    #[test]
    fn test_unscaled_matches_salt_path_at_min_capacity() {
        let mut cost = cost_with_capacity(MegaSpecId::REX6, MIN_BUCKET_SIZE as u64);
        assert_eq!(
            cost.sstore_set_gas_unscaled(),
            cost.sstore_set_gas(Address::ZERO, U256::ZERO).unwrap(),
        );
    }
}
