//! SALT environment oracle trait and implementations.

use core::fmt::Debug;

use alloy_primitives::BlockNumber;
use auto_impl::auto_impl;
pub use salt::{BucketId, BucketMeta};

use super::DefaultExternalEnvs;

/// An oracle service that provides external information to the EVM. This trait provides a mechanism
/// for the EVM to access additional information from an external environment.
///
/// Typically, one implementation of this trait can be a reader of the underlying blockchain
/// database of `MegaETH` to provide deterministic information (e.g., bucket capacity) during EVM
/// execution.
#[auto_impl(&, Box, Arc)]
pub trait SaltEnv: Debug + Send + Sync + Unpin {
    /// The error type for the oracle.
    type Error;

    /// Gets the capacity of the SALT bucket for a given bucket ID at a specific block (according
    /// to its post-execution state).
    ///
    /// # Arguments
    ///
    /// * `bucket_id` - The ID of the SALT bucket
    /// * `at_block` - The block number at which to get the bucket capacity
    ///
    /// # Returns
    ///
    /// The capacity of the SALT bucket for the given bucket ID at the given block.
    fn get_bucket_capacity(
        &self,
        bucket_id: BucketId,
        at_block: BlockNumber,
    ) -> Result<u64, Self::Error>;
}

impl<Error: Unpin + Send + Sync + Clone + 'static> SaltEnv for DefaultExternalEnvs<Error> {
    type Error = Error;

    fn get_bucket_capacity(
        &self,
        bucket_id: BucketId,
        at_block: BlockNumber,
    ) -> Result<u64, Self::Error> {
        // Return the value from storage, or MIN_BUCKET_SIZE if not set
        Ok(self
            .bucket_capacity
            .read()
            .expect("RwLock poisoned")
            .get(&(bucket_id, at_block))
            .copied()
            .unwrap_or(salt::constant::MIN_BUCKET_SIZE as u64))
    }
}

impl<Error: Unpin + Send + Sync + Clone + 'static> DefaultExternalEnvs<Error> {
    /// Sets a bucket capacity for a given bucket ID at a specific block for testing purposes.
    ///
    /// # Arguments
    ///
    /// * `bucket_id` - The ID of the SALT bucket
    /// * `at_block` - The block number at which to set the capacity
    /// * `capacity` - The capacity value to set
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_bucket_capacity(
        self,
        bucket_id: BucketId,
        at_block: BlockNumber,
        capacity: u64,
    ) -> Self {
        self.bucket_capacity
            .write()
            .expect("RwLock poisoned")
            .insert((bucket_id, at_block), capacity);
        self
    }

    /// Clears all bucket capacity values.
    pub fn clear_bucket_capacity(&self) {
        self.bucket_capacity.write().expect("RwLock poisoned").clear();
    }
}
