//! This module defines the SaltEnv trait, which provides bucket capacity information for dynamic
//! gas pricing. Storage slots and accounts are organized into buckets, and the gas cost scales
//! with bucket capacity to incentivize efficient resource allocation.

use core::{convert::Infallible, fmt::Debug};

use alloy_primitives::{Address, U256};
use auto_impl::auto_impl;

use crate::EmptyExternalEnv;

/// SALT bucket identifier. Accounts and storage slots are mapped to buckets, which have
/// dynamic capacities that affect gas costs.
pub type BucketId = u32;

/// Number of bits to represent the minimum bucket size (8 bits = 256 slots).
pub const MIN_BUCKET_SIZE_BITS: usize = 8;

/// Minimum capacity of a SALT bucket in number of slots (256).
///
/// Buckets hold accounts or storage slots and can grow beyond this size. The gas cost
/// multiplier is calculated as `capacity / MIN_BUCKET_SIZE`, so a bucket at minimum
/// capacity has a 1x multiplier.
pub const MIN_BUCKET_SIZE: usize = 1 << MIN_BUCKET_SIZE_BITS;

/// Interface for SALT bucket capacity information.
///
/// This trait provides bucket capacity data needed for dynamic gas pricing. Implementations
/// typically read from the underlying blockchain database to ensure deterministic execution.
///
/// # Block-Awareness
///
/// This trait does not take a block parameter. Block context is provided when the environment
/// is created via [`ExternalEnvFactory::external_envs`](crate::ExternalEnvFactory::external_envs),
/// allowing implementations to snapshot state at a specific block.
///
/// # Bucket ID Calculation
///
/// The trait provides default methods [`bucket_id_for_account`](SaltEnv::bucket_id_for_account)
/// and [`bucket_id_for_slot`](SaltEnv::bucket_id_for_slot) that can be overridden by
/// implementations to customize bucket assignment logic.
#[auto_impl(&, Box, Arc)]
pub trait SaltEnv: Debug + Unpin {
    /// Error type returned when bucket capacity cannot be retrieved.
    type Error;

    /// Returns the current capacity of the specified bucket.
    ///
    /// # Gas Cost Calculation
    ///
    /// The returned capacity is used to calculate a gas multiplier:
    /// ```text
    /// multiplier = capacity / MIN_BUCKET_SIZE
    /// ```
    /// This multiplier scales the base storage gas costs, making operations more expensive
    /// as buckets grow.
    ///
    /// # Arguments
    ///
    /// * `bucket_id` - The bucket to query
    ///
    /// # Returns
    ///
    /// The bucket's capacity in number of slots, or an error if unavailable.
    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error>;

    /// Maps an account address to its bucket ID.
    ///
    /// This method determines which bucket tracks the account creation gas costs.
    /// The default implementation can be overridden to customize bucket assignment.
    ///
    /// # Arguments
    ///
    /// * `account` - The account address to map
    fn bucket_id_for_account(account: Address) -> BucketId;

    /// Maps a storage slot to its bucket ID.
    ///
    /// This method determines which bucket tracks the storage slot's gas costs.
    /// The default implementation can be overridden to customize bucket assignment.
    ///
    /// # Arguments
    ///
    /// * `address` - The contract address owning the storage
    /// * `key` - The storage slot key
    fn bucket_id_for_slot(address: Address, key: U256) -> BucketId;
}

/// No-op implementation that returns minimum bucket size for all buckets.
///
/// This implementation assigns all accounts and storage slots to bucket 0 with minimum
/// capacity, effectively disabling dynamic gas pricing. Useful for testing or when SALT
/// functionality is not needed.
impl SaltEnv for EmptyExternalEnv {
    type Error = Infallible;

    fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, Self::Error> {
        Ok(MIN_BUCKET_SIZE as u64)
    }

    fn bucket_id_for_account(_account: Address) -> BucketId {
        0 as BucketId
    }

    fn bucket_id_for_slot(_address: Address, _key: U256) -> BucketId {
        0 as BucketId
    }
}
