#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use core::fmt::Debug;
use revm::primitives::hash_map::Entry;

use alloy_primitives::{Address, BlockNumber, U256};
use revm::{context::BlockEnv, primitives::HashMap};

use crate::{constants, BucketId, MegaSpecId, MIN_BUCKET_SIZE};

/// Calculator for dynamic gas costs based on bucket capacity.
#[derive(Debug)]
pub struct DynamicGasCost {
    /// The spec id.
    spec: MegaSpecId,
    /// The parent block number.
    parent_block: BlockNumber,
    /// Cache of the bucket cost multiplier for each bucket Id. The multiplier will be used to
    /// multiply [`SSTORE_SET_GAS`] to get the actual gas cost for setting a storage slot.
    bucket_cost_mulitipers: HashMap<BucketId, u64>,
}

impl DynamicGasCost {
    /// Creates a new [`DynamicGasCost`].
    pub fn new(spec: MegaSpecId, parent_block: BlockNumber) -> Self {
        Self { spec, parent_block, bucket_cost_mulitipers: HashMap::default() }
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
    pub fn sstore_set_gas<DB: revm::Database>(
        &mut self,
        db: &DB,
        address: Address,
        key: U256,
    ) -> Result<u64, DB::Error> {
        // increase the gas cost according to the bucket capacity
        let (bucket_id, capacity) = db.salt_bucket_capacity(address, Some(key))?;
        let multiplier = self.load_bucket_cost_multiplier(bucket_id, capacity)?;

        let gas = if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::SSTORE_SET_STORAGE_GAS_BASE * (multiplier - 1)
        } else {
            constants::mini_rex::SSTORE_SET_STORAGE_GAS * multiplier
        };

        Ok(gas)
    }

    /// Calculates the gas cost for creating a new account. This overrides the
    /// [`NEWACCOUNT`](revm::interpreter::gas::NEWACCOUNT) gas cost in the original EVM.
    pub fn new_account_gas<DB: revm::Database>(
        &mut self,
        db: &DB,
        address: Address,
    ) -> Result<u64, DB::Error> {
        // increase the gas cost according to the bucket capacity
        let (bucket_id, capacity) = db.salt_bucket_capacity(address, None)?;
        let multiplier = self.load_bucket_cost_multiplier(bucket_id, capacity)?;

        let gas = if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::NEW_ACCOUNT_STORAGE_GAS_BASE * (multiplier - 1)
        } else {
            constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS * multiplier
        };

        Ok(gas)
    }

    /// Calculates the gas cost for creating a new contract. This overrides the
    /// [`CREATE`](revm::interpreter::gas::CREATE) gas cost in the original EVM.
    pub fn create_contract_gas<DB: revm::Database>(
        &mut self,
        db: &DB,
        address: Address,
    ) -> Result<u64, DB::Error> {
        // increase the gas cost according to the bucket capacity
        let (bucket_id, capacity) = db.salt_bucket_capacity(address, None)?;
        let multiplier = self.load_bucket_cost_multiplier(bucket_id, capacity)?;

        let gas = if self.spec.is_enabled(MegaSpecId::REX) {
            constants::rex::CONTRACT_CREATION_STORAGE_GAS_BASE * (multiplier - 1)
        } else {
            constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS * multiplier
        };

        Ok(gas)
    }

    /// Loads the bucket cost multiplier for a given bucket Id.
    fn load_bucket_cost_multiplier<E>(
        &mut self,
        bucket_id: BucketId,
        capacity: u64,
    ) -> Result<u64, E> {
        match self.bucket_cost_mulitipers.entry(bucket_id) {
            Entry::Occupied(occupied_entry) => Ok(*occupied_entry.get()),
            Entry::Vacant(vacant_entry) => {
                let multiplier = capacity / MIN_BUCKET_SIZE as u64;
                vacant_entry.insert(multiplier);
                Ok(multiplier)
            }
        }
    }
}

impl DynamicGasCost {
    pub(crate) fn on_new_block(&mut self, block: &BlockEnv) {
        self.reset(block.number.to::<u64>().saturating_sub(1));
    }
}
