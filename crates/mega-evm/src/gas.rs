use core::{cell::RefCell, fmt::Debug};
use std::{collections::hash_map::Entry, rc::Rc, sync::Arc};

use alloy_primitives::{Address, BlockNumber, B256, U256};
use auto_impl::auto_impl;
use revm::{context::BlockEnv, primitives::HashMap};
pub use salt::{BucketId, BucketMeta};

use crate::constants;

/// An oracle of the gas cost for setting a storage slot to a non-zero value.
#[derive(Debug)]
pub(crate) struct GasCostOracle<Oracle> {
    /// The parent block number.
    parent_block: BlockNumber,
    /// The external env oracle.
    oracle: Oracle,
    /// Cache of the bucket cost multiplier for each bucket Id. The multiplier will be used to
    /// multiple [`SSTORE_SET_GAS`] to get the actual gas cost for setting a storage slot.
    bucket_cost_mulitipers: HashMap<BucketId, u64>,
}

impl<Oracle: ExternalEnvOracle> GasCostOracle<Oracle> {
    /// Creates a new [`SaltBucketCostFeed`].
    pub(crate) fn new(oracle: Oracle, parent_block: BlockNumber) -> Self {
        Self { parent_block, oracle, bucket_cost_mulitipers: HashMap::default() }
    }

    /// Resets the cache of the bucket cost multiplier.
    pub(crate) fn reset(&mut self, parent_block: BlockNumber) {
        self.bucket_cost_mulitipers.clear();
        self.parent_block = parent_block;
    }

    /// Calculates the gas cost for setting a storage slot to a non-zero value. This overrides the
    /// [`SSTORE_SET`](revm::interpreter::gas::SSTORE_SET) gas cost in the original EVM.
    pub(crate) fn sstore_set_gas(&mut self, address: Address, key: U256) -> u64 {
        let mut gas = constants::mini_rex::SSTORE_SET_GAS;

        // increase the gas cost according to the bucket capacity
        let bucket_id = slot_to_bucket_id(address, key);
        let multiplier = self.load_bucket_cost_multiplier(bucket_id);
        gas *= multiplier;

        gas
    }

    /// Calculates the gas cost for creating a new account. This overrides the
    /// [`NEWACCOUNT`](revm::interpreter::gas::NEWACCOUNT) gas cost in the original EVM.
    pub(crate) fn new_account_gas(&mut self, address: Address) -> u64 {
        let mut gas = constants::mini_rex::NEW_ACCOUNT_GAS;

        // increase the gas cost according to the bucket capacity
        let bucket_id = address_to_bucket_id(address);
        let multiplier = self.load_bucket_cost_multiplier(bucket_id);
        gas *= multiplier;

        gas
    }

    /// Loads the bucket cost multiplier for a given bucket Id.
    fn load_bucket_cost_multiplier(&mut self, bucket_id: BucketId) -> u64 {
        match self.bucket_cost_mulitipers.entry(bucket_id) {
            Entry::Occupied(occupied_entry) => *occupied_entry.get(),
            Entry::Vacant(vacant_entry) => {
                let meta = self.oracle.get_bucket_meta(bucket_id, self.parent_block);
                let multiplier = meta.capacity / salt::constant::MIN_BUCKET_SIZE as u64;
                vacant_entry.insert(multiplier);
                multiplier
            }
        }
    }
}

impl<Oracle: ExternalEnvOracle> GasCostOracle<Oracle> {
    pub(crate) fn on_new_block(&mut self, block: &BlockEnv) {
        self.reset(block.number.to::<u64>() - 1);
    }
}

/// data length of Key of Storage Slot
const SLOT_KEY_LEN: usize = B256::len_bytes();
/// data length of Key of Account
const PLAIN_ACCOUNT_KEY_LEN: usize = Address::len_bytes();
/// data length of Key of Storage
const PLAIN_STORAGE_KEY_LEN: usize = PLAIN_ACCOUNT_KEY_LEN + SLOT_KEY_LEN;

/// Convert an address to a bucket id.
pub(crate) fn address_to_bucket_id(address: Address) -> BucketId {
    salt::pk_hasher::bucket_id(address.as_slice())
}

/// Convert an address and a storage slot key to a bucket id.
pub(crate) fn slot_to_bucket_id(address: Address, key: U256) -> BucketId {
    salt::pk_hasher::bucket_id(
        address.concat_const::<SLOT_KEY_LEN, PLAIN_STORAGE_KEY_LEN>(key.into()).as_slice(),
    )
}

/// An oracle service that provides external information to the EVM. This trait provides a mechanism
/// for the EVM to access additional information from an external environment.
///
/// Typically, one implementation of this trait can be a reader of the underlying blockchain
/// database of `MegaETH` to provide deterministic information (e.g., bucket capacity) during EVM
/// execution.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvOracle: Debug + Send + Sync + Unpin {
    /// Gets the metadata of the SALT bucket for a given bucket ID at a specific block (according
    /// to its post-execution state).
    ///
    /// # Arguments
    ///
    /// * `bucket_id` - The ID of the SALT bucket
    /// * `at_block` - The block number at which to get the bucket metadata
    ///
    /// # Returns
    ///
    /// The metadata of the SALT bucket for the given bucket ID at the given block.
    fn get_bucket_meta(&self, bucket_id: BucketId, at_block: BlockNumber) -> BucketMeta;
}

/// A no-op implementation of the [`ExternalEnvOracle`] trait. It is useful when the EVM does not
/// need to access any additional information from an external environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpOracle;

impl NoOpOracle {
    /// Consumes and wraps `self` into an Arc-wrapped boxed instance of the [`ExternalEnvOracle`]
    /// trait.
    pub fn boxed_arc(self) -> Arc<Box<dyn ExternalEnvOracle>> {
        Arc::new(self.boxed())
    }

    /// Consumes and wraps `self` into a boxed instance of the [`ExternalEnvOracle`] trait.
    pub fn boxed(self) -> Box<dyn ExternalEnvOracle> {
        Box::new(self)
    }
}

impl ExternalEnvOracle for NoOpOracle {
    fn get_bucket_meta(&self, _bucket_id: BucketId, _at_block: BlockNumber) -> BucketMeta {
        // By default, return a default BucketMeta with maximum capacity
        BucketMeta::default()
    }
}
