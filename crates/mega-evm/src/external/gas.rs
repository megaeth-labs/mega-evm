#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use core::fmt::Debug;
use revm::primitives::hash_map::Entry;

use alloy_primitives::{Address, BlockNumber, U256};
use revm::{context::BlockEnv, primitives::HashMap};

use crate::{constants, BucketId, MegaSpecId, SaltEnv, MIN_BUCKET_SIZE};

/// Calculator for dynamic gas costs based on bucket capacity.
#[derive(Debug)]
pub struct DynamicGasCost<SaltEnvImpl> {
    /// The spec id.
    spec: MegaSpecId,
    /// The parent block number.
    parent_block: BlockNumber,
    /// The external environment for SALT bucket information.
    salt_env: SaltEnvImpl,
    /// Cache of the bucket cost multiplier for each bucket Id. The multiplier will be used to
    /// multiply [`SSTORE_SET_GAS`] to get the actual gas cost for setting a storage slot.
    bucket_cost_mulitipers: HashMap<BucketId, u64>,
}

impl<SaltEnvImpl: SaltEnv> DynamicGasCost<SaltEnvImpl> {
    /// Creates a new [`DynamicGasCost`].
    pub fn new(spec: MegaSpecId, salt_env: SaltEnvImpl, parent_block: BlockNumber) -> Self {
        Self { spec, parent_block, salt_env, bucket_cost_mulitipers: HashMap::default() }
    }

    /// Resets the cache of the bucket cost multiplier.
    pub fn reset(&mut self, parent_block: BlockNumber) {
        self.bucket_cost_mulitipers.clear();
        self.parent_block = parent_block;
    }

    /// Gets the bucket IDs used during transaction execution.
    pub fn get_bucket_ids(&self) -> Vec<BucketId> {
        self.bucket_cost_mulitipers.keys().copied().collect()
    }

    /// `SSTORE_SET` storage gas for an explicit bucket-capacity `multiplier` (always ≥ 1).
    ///
    /// Single source of the per-spec `SSTORE_SET` dynamic storage-gas formula, shared by the
    /// SALT-driven path ([`sstore_set_gas`](Self::sstore_set_gas)) and the REX6 system-exempt
    /// unscaled path ([`sstore_set_gas_unscaled`](Self::sstore_set_gas_unscaled)).
    fn sstore_set_gas_for_multiplier(&self, multiplier: u64) -> u64 {
        if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::SSTORE_SET_STORAGE_GAS_BASE.saturating_mul(multiplier - 1)
        } else {
            constants::mini_rex::SSTORE_SET_STORAGE_GAS.saturating_mul(multiplier)
        }
    }

    /// `NEW_ACCOUNT` storage gas for an explicit bucket-capacity `multiplier` (always ≥ 1).
    fn new_account_gas_for_multiplier(&self, multiplier: u64) -> u64 {
        if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::NEW_ACCOUNT_STORAGE_GAS_BASE.saturating_mul(multiplier - 1)
        } else {
            constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS.saturating_mul(multiplier)
        }
    }

    /// CREATE storage gas for an explicit bucket-capacity `multiplier` (always ≥ 1).
    fn create_contract_gas_for_multiplier(&self, multiplier: u64) -> u64 {
        if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::CONTRACT_CREATION_STORAGE_GAS_BASE.saturating_mul(multiplier - 1)
        } else {
            constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS.saturating_mul(multiplier)
        }
    }

    /// Calculates the gas cost for setting a storage slot to a non-zero value. This overrides the
    /// [`SSTORE_SET`](revm::interpreter::gas::SSTORE_SET) gas cost in the original EVM.
    pub fn sstore_set_gas(
        &mut self,
        address: Address,
        key: U256,
    ) -> Result<u64, SaltEnvImpl::Error> {
        // increase the gas cost according to the bucket capacity
        let bucket_id = SaltEnvImpl::bucket_id_for_slot(address, key);
        let multiplier = self.load_bucket_cost_multiplier(bucket_id)?;

        Ok(self.sstore_set_gas_for_multiplier(multiplier))
    }

    /// Calculates the gas cost for creating a new account. This overrides the
    /// [`NEWACCOUNT`](revm::interpreter::gas::NEWACCOUNT) gas cost in the original EVM.
    pub fn new_account_gas(&mut self, address: Address) -> Result<u64, SaltEnvImpl::Error> {
        // increase the gas cost according to the bucket capacity
        let bucket_id = SaltEnvImpl::bucket_id_for_account(address);
        let multiplier = self.load_bucket_cost_multiplier(bucket_id)?;

        Ok(self.new_account_gas_for_multiplier(multiplier))
    }

    /// Calculates the gas cost for creating a new contract. This overrides the
    /// [`CREATE`](revm::interpreter::gas::CREATE) gas cost in the original EVM.
    pub fn create_contract_gas(&mut self, address: Address) -> Result<u64, SaltEnvImpl::Error> {
        // increase the gas cost according to the bucket capacity
        let bucket_id = SaltEnvImpl::bucket_id_for_account(address);
        let multiplier = self.load_bucket_cost_multiplier(bucket_id)?;

        Ok(self.create_contract_gas_for_multiplier(multiplier))
    }

    /// SALT-unscaled `SSTORE_SET` storage gas: the cost a write would pay if the target bucket
    /// were at its minimum capacity, equivalent to taking the REX-family formula
    /// `base × (multiplier − 1)` to `0`. REX6+ charges system-originated transactions this so
    /// their storage cost is independent of how full SALT buckets actually are. Unlike
    /// [`sstore_set_gas`](Self::sstore_set_gas) this does not query the SALT env or record bucket
    /// access.
    ///
    /// REX-family only: the production caller (`HostExt::sstore_set_storage_gas` under the
    /// REX6-gated `LimitCheck::Exempt` stamp) is unreachable pre-REX, and the "0 additional
    /// storage gas" property holds only for the REX-family `base × (multiplier − 1)` formula.
    pub fn sstore_set_gas_unscaled(&self) -> u64 {
        debug_assert!(self.spec.is_enabled(MegaSpecId::REX));
        // `multiplier = 1` ≡ bucket at minimum capacity, no excess to charge for —
        // REX-family `base × (multiplier − 1)` evaluates to `0`.
        self.sstore_set_gas_for_multiplier(1)
    }

    /// SALT-unscaled `NEW_ACCOUNT` storage gas.
    /// See [`sstore_set_gas_unscaled`](Self::sstore_set_gas_unscaled).
    pub fn new_account_gas_unscaled(&self) -> u64 {
        debug_assert!(self.spec.is_enabled(MegaSpecId::REX));
        self.new_account_gas_for_multiplier(1)
    }

    /// SALT-unscaled CREATE storage gas.
    /// See [`sstore_set_gas_unscaled`](Self::sstore_set_gas_unscaled).
    pub fn create_contract_gas_unscaled(&self) -> u64 {
        debug_assert!(self.spec.is_enabled(MegaSpecId::REX));
        self.create_contract_gas_for_multiplier(1)
    }

    /// Loads the bucket cost multiplier for a given bucket Id.
    fn load_bucket_cost_multiplier(
        &mut self,
        bucket_id: BucketId,
    ) -> Result<u64, SaltEnvImpl::Error> {
        match self.bucket_cost_mulitipers.entry(bucket_id) {
            Entry::Occupied(occupied_entry) => Ok(*occupied_entry.get()),
            Entry::Vacant(vacant_entry) => {
                let capacity = self.salt_env.get_bucket_capacity(bucket_id)?;
                assert!(
                    capacity >= MIN_BUCKET_SIZE as u64,
                    "SaltEnv returned bucket_capacity={capacity} below MIN_BUCKET_SIZE ({})",
                    MIN_BUCKET_SIZE,
                );
                let multiplier = capacity / MIN_BUCKET_SIZE as u64;
                vacant_entry.insert(multiplier);
                Ok(multiplier)
            }
        }
    }
}

impl<SaltEnvImpl: SaltEnv> DynamicGasCost<SaltEnvImpl> {
    pub(crate) fn on_new_block(&mut self, block: &BlockEnv) {
        self.reset(block.number.to::<u64>().saturating_sub(1));
    }
}

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

    /// `MiniRex` prices contract creation with the flat `NEW_ACCOUNT_STORAGE_GAS × multiplier`
    /// formula; REX switches to `CONTRACT_CREATION_STORAGE_GAS_BASE × (multiplier − 1)`. Pin
    /// the boundary from the `MiniRex` side so a loosened REX gate fails here.
    #[test]
    fn test_create_contract_gas_uses_mini_rex_formula_before_rex() {
        let cost = cost_with_capacity(MegaSpecId::MINI_REX, MIN_BUCKET_SIZE as u64);
        assert_eq!(
            cost.create_contract_gas_for_multiplier(3),
            constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS.saturating_mul(3),
        );
        assert_ne!(
            constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS.saturating_mul(3),
            constants::rex::CONTRACT_CREATION_STORAGE_GAS_BASE.saturating_mul(2),
            "the two formulas must disagree for this pin to mean anything",
        );
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
