//! SALT environment oracle trait and implementations.

use core::{convert::Infallible, fmt::Debug};

use alloy_primitives::{Address, U256};
use auto_impl::auto_impl;

use crate::EmptyExternalEnv;

/// The type of the SALT bucket ID.
pub type BucketId = u32;

/// Number of bits to represent the minimum bucket size.
pub const MIN_BUCKET_SIZE_BITS: usize = 8;
/// Minimum capacity of a SALT bucket (256 slots).
/// Buckets are dynamically resized but their capacities cannot drop below this value.
/// This represents the number of key-value pairs a bucket can hold at minimum.
pub const MIN_BUCKET_SIZE: usize = 1 << MIN_BUCKET_SIZE_BITS;

/// An oracle service that provides external information to the EVM. This trait provides a mechanism
/// for the EVM to access additional information from an external environment.
///
/// Typically, one implementation of this trait can be a reader of the underlying blockchain
/// database of `MegaETH` to provide deterministic information (e.g., bucket capacity) during EVM
/// execution.
#[auto_impl(&, Box, Arc)]
pub trait SaltEnv: Debug + Unpin {
    /// The error type for the oracle.
    type Error;

    /// Gets the capacity of the SALT bucket for a given bucket ID.
    ///
    /// # Arguments
    ///
    /// * `bucket_id` - The ID of the SALT bucket
    ///
    /// # Returns
    ///
    /// The capacity of the SALT bucket for the given bucket ID.
    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error>;

    /// Gets the bucket ID for a given account.
    fn bucket_id_for_account(account: Address) -> BucketId;

    /// Gets the bucket ID for a given storage slot.
    fn bucket_id_for_slot(address: Address, key: U256) -> BucketId;
}

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
