//! This module defines the `SaltEnv` trait, which provides bucket capacity information for dynamic
//! gas pricing. Storage slots and accounts are organized into buckets, and the gas cost scales
//! with bucket capacity to incentivize efficient resource allocation.

use core::{
    convert::Infallible,
    fmt::{Debug, Display},
};

#[cfg(feature = "std")]
use alloy_primitives::B256;
use alloy_primitives::{Address, U256};
use auto_impl::auto_impl;

#[cfg(feature = "std")]
use salt::state::hasher;

#[cfg(feature = "std")]
const ADDRESS_KEY_LEN: usize = Address::len_bytes();
#[cfg(feature = "std")]
const STORAGE_SLOT_LEN: usize = B256::len_bytes();
#[cfg(feature = "std")]
const STORAGE_KEY_LEN: usize = ADDRESS_KEY_LEN + STORAGE_SLOT_LEN;

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
    type Error: Display + Send + Sync + 'static;

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
    ///
    /// # Panics
    ///
    /// Panics in `no_std` environments if not overridden, as the default implementation
    /// requires the `salt` crate which is only available with the `std` feature.
    fn bucket_id_for_account(account: Address) -> BucketId {
        #[cfg(feature = "std")]
        {
            hasher::bucket_id(account.as_slice())
        }
        #[cfg(not(feature = "std"))]
        {
            unimplemented!(
                "bucket_id_for_account({:?}) requires std feature or must be overridden in no_std environments",
                account
            )
        }
    }

    /// Maps a storage slot to its bucket ID.
    ///
    /// This method determines which bucket tracks the storage slot's gas costs.
    /// The default implementation can be overridden to customize bucket assignment.
    ///
    /// # Arguments
    ///
    /// * `address` - The contract address owning the storage
    /// * `key` - The storage slot key
    ///
    /// # Panics
    ///
    /// Panics in `no_std` environments if not overridden, as the default implementation
    /// requires the `salt` crate which is only available with the `std` feature.
    fn bucket_id_for_slot(address: Address, key: U256) -> BucketId {
        #[cfg(feature = "std")]
        {
            hasher::bucket_id(
                address.concat_const::<STORAGE_SLOT_LEN, STORAGE_KEY_LEN>(key.into()).as_slice(),
            )
        }
        #[cfg(not(feature = "std"))]
        {
            unimplemented!(
                "bucket_id_for_slot({:?}, {:?}) requires std feature or must be overridden in no_std environments",
                address,
                key
            )
        }
    }
}

/// No-op implementation that returns minimum bucket size for all buckets.
///
/// This implementation assigns all accounts and storage slots to bucket 0 with minimum
/// capacity, effectively disabling dynamic gas pricing. Useful for testing or when SALT
/// functionality is not needed.
impl SaltEnv for super::EmptyExternalEnv {
    type Error = Infallible;

    fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, Self::Error> {
        Ok(MIN_BUCKET_SIZE as u64)
    }
}

/// Trait that combines `Database` and `SaltEnv` to ensure they operate on the same database
/// instance.
///
/// This trait is automatically implemented for any type that implements both `Database` and
/// `SaltEnv` with matching error types. It ensures type safety by requiring that the database and
/// SALT environment read from the same underlying data source.
///
/// # Design Rationale
///
/// Previously, `SaltEnv` was a separate field in `ExternalEnvs`, which could not guarantee that
/// it accessed the same database snapshot as the EVM's `Database`. This trait solves the problem
/// by requiring the database itself to provide SALT functionality, ensuring consistency.
///
/// # Implementation
///
/// Downstream users should implement both `Database` and `SaltEnv` for their database type:
///
/// ```rust,ignore
/// use mega_evm::{Database, SaltEnv, BucketId};
///
/// struct MyDatabase {
///     state_snapshot: StateSnapshot,
/// }
///
/// impl Database for MyDatabase {
///     type Error = MyError;
///     // ... implement Database methods
/// }
///
/// impl SaltEnv for MyDatabase {
///     type Error = MyError;  // Must match Database::Error
///
///     fn get_bucket_capacity(&mut self, bucket_id: BucketId) -> Result<u64, Self::Error> {
///         // Read from the same snapshot
///         self.state_snapshot.get_bucket_capacity(bucket_id)
///     }
///     // ... implement other SaltEnv methods
/// }
///
/// // MyDatabase now automatically implements MegaDatabase
/// ```
pub trait MegaDatabase:
    revm::Database<Error: Send + Sync + 'static> + SaltEnv<Error = <Self as revm::Database>::Error>
{
}

/// Blanket implementation: any type that implements both `Database` and `SaltEnv` with matching
/// error types automatically implements `MegaDatabase`.
impl<T> MegaDatabase for T where
    T: revm::Database<Error: Send + Sync + 'static> + SaltEnv<Error = <T as revm::Database>::Error>
{
}

/// `SaltEnv` implementation for `&mut State<DB>` where `DB` implements `SaltEnv`.
///
/// This allows `State` wrappers to delegate SALT operations to the underlying database.
/// Note: We implement for both `revm::database::State` and the internal `revm_database::State`
/// path to ensure compatibility across different import contexts.
impl<DB> SaltEnv for &mut revm::database::State<DB>
where
    DB: revm::Database + SaltEnv<Error = <DB as revm::Database>::Error>,
    <DB as revm::Database>::Error: Send + Sync + 'static,
{
    type Error = <DB as revm::Database>::Error;

    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        self.database.get_bucket_capacity(bucket_id)
    }
}

/// Wrapper that provides default `SaltEnv` implementation for any `Database`.
///
/// This wrapper allows any `Database` to be used as a `MegaDatabase` by providing
/// a default `SaltEnv` implementation that returns minimum bucket sizes for all queries.
/// This is useful for databases that don't need dynamic gas pricing.
#[derive(Debug)]
pub struct DefaultSaltEnv<DB>(pub DB);

impl<DB: revm::Database> revm::Database for DefaultSaltEnv<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        self.0.basic(address)
    }

    fn code_by_hash(
        &mut self,
        code_hash: alloy_primitives::B256,
    ) -> Result<revm::state::Bytecode, Self::Error> {
        self.0.code_by_hash(code_hash)
    }

    fn storage(
        &mut self,
        address: Address,
        index: alloy_primitives::U256,
    ) -> Result<alloy_primitives::U256, Self::Error> {
        self.0.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
        self.0.block_hash(number)
    }
}

impl<DB> SaltEnv for DefaultSaltEnv<DB>
where
    DB: revm::Database + Debug + Unpin,
    DB::Error: Send + Sync + 'static,
{
    type Error = DB::Error;

    fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, Self::Error> {
        Ok(MIN_BUCKET_SIZE as u64)
    }
}

/// `SaltEnv` implementation for `revm::database::EmptyDB`.
///
/// This allows `EmptyDB` to be used directly as a `MegaDatabase` without wrapping.
impl SaltEnv for revm::database::EmptyDB {
    type Error = core::convert::Infallible;

    fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, Self::Error> {
        Ok(MIN_BUCKET_SIZE as u64)
    }
}

/// `SaltEnv` implementation for `revm::database::CacheDB`.
///
/// This delegates to the underlying database's `SaltEnv` implementation.
impl<DB> SaltEnv for revm::database::CacheDB<DB>
where
    DB: revm::DatabaseRef + SaltEnv<Error = <DB as revm::DatabaseRef>::Error>,
    <DB as revm::DatabaseRef>::Error: Send + Sync + 'static,
{
    type Error = <DB as revm::DatabaseRef>::Error;

    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        self.db.get_bucket_capacity(bucket_id)
    }
}

/// `SaltEnv` implementation for `&mut revm::database::CacheDB`.
impl<DB> SaltEnv for &mut revm::database::CacheDB<DB>
where
    DB: revm::DatabaseRef + SaltEnv<Error = <DB as revm::DatabaseRef>::Error>,
    <DB as revm::DatabaseRef>::Error: Send + Sync + 'static,
{
    type Error = <DB as revm::DatabaseRef>::Error;

    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        (**self).get_bucket_capacity(bucket_id)
    }
}
