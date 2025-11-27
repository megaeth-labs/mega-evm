#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use core::fmt::Debug;
use revm::primitives::hash_map::Entry;

use alloy_primitives::{Address, BlockNumber, U256};
use revm::{context::BlockEnv, primitives::HashMap};

use crate::{constants, BucketId, MegaSpecId, SaltEnv};

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
    /// multiple [`SSTORE_SET_GAS`] to get the actual gas cost for setting a storage slot.
    bucket_cost_mulitipers: HashMap<BucketId, u64>,
}

impl<SaltEnvImpl: SaltEnv> DynamicGasCost<SaltEnvImpl> {
    /// Creates a new [`SaltBucketCostFeed`].
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

        let gas = if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::SSTORE_SET_STORAGE_GAS_BASE * (multiplier - 1)
        } else {
            constants::mini_rex::SSTORE_SET_STORAGE_GAS * multiplier
        };

        Ok(gas)
    }

    /// Calculates the gas cost for creating a new account. This overrides the
    /// [`NEWACCOUNT`](revm::interpreter::gas::NEWACCOUNT) gas cost in the original EVM.
    pub fn new_account_gas(&mut self, address: Address) -> Result<u64, SaltEnvImpl::Error> {
        // increase the gas cost according to the bucket capacity
        let bucket_id = SaltEnvImpl::bucket_id_for_account(address);
        let multiplier = self.load_bucket_cost_multiplier(bucket_id)?;

        let gas = if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::NEW_ACCOUNT_STORAGE_GAS_BASE * (multiplier - 1)
        } else {
            constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS * multiplier
        };

        Ok(gas)
    }

    /// Calculates the gas cost for creating a new contract. This overrides the
    /// [`CREATE`](revm::interpreter::gas::CREATE) gas cost in the original EVM.
    pub fn create_contract_gas(&mut self, address: Address) -> Result<u64, SaltEnvImpl::Error> {
        // increase the gas cost according to the bucket capacity
        let bucket_id = SaltEnvImpl::bucket_id_for_account(address);
        let multiplier = self.load_bucket_cost_multiplier(bucket_id)?;

        let gas = if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::CONTRACT_CREATION_STORAGE_GAS_BASE * (multiplier - 1)
        } else {
            constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS * multiplier
        };

        Ok(gas)
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
                let multiplier = capacity / salt::constant::MIN_BUCKET_SIZE as u64;
                vacant_entry.insert(multiplier);
                Ok(multiplier)
            }
        }
    }
}

impl<SaltEnvImpl: SaltEnv> DynamicGasCost<SaltEnvImpl> {
    pub(crate) fn on_new_block(&mut self, block: &BlockEnv) {
        self.reset(block.number.to::<u64>() - 1);
    }
}
