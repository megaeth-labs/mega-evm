//! Test utilities for external environment implementations.
//!
//! Provides [`TestExternalEnvs`], a configurable mock implementation of Oracle
//! environments for use in tests, and [`TestDatabaseWrapper`], a database wrapper
//! that implements `salt_bucket_capacity` for testing.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{cell::RefCell, convert::Infallible, fmt::Display};
use std::{rc::Rc, vec::Vec};

use alloy_primitives::{Address, Bytes, B256, U256};
use revm::primitives::HashMap;

use crate::{BucketId, ExternalEnvFactory, ExternalEnvTypes, ExternalEnvs, OracleEnv};

/// A recorded oracle hint from `on_hint` calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedHint {
    /// The sender address who called `sendHint`.
    pub from: Address,
    /// The user-defined hint topic.
    pub topic: B256,
    /// The hint data.
    pub data: Bytes,
}

/// Configurable external environment implementation for testing.
///
/// This struct provides mutable state for oracle storage and recorded hints,
/// allowing tests to set up specific scenarios and verify hint mechanism behavior.
///
/// # Example
/// ```ignore
/// let env = TestExternalEnvs::new()
///     .with_oracle_storage(U256::ZERO, U256::from(42));  // Set oracle slot 0 to 42
/// ```
#[derive(derive_more::Debug, Clone)]
pub struct TestExternalEnvs<Error = Infallible> {
    #[debug(ignore)]
    _phantom: core::marker::PhantomData<Error>,
    /// Oracle contract storage values. Maps storage slot keys to their values.
    oracle_storage: Rc<RefCell<HashMap<U256, U256>>>,
    /// Recorded hints from `on_hint` calls. Used for testing the hint mechanism.
    recorded_hints: Rc<RefCell<Vec<RecordedHint>>>,
}

impl Default for TestExternalEnvs {
    fn default() -> Self {
        Self::new()
    }
}

impl From<TestExternalEnvs> for ExternalEnvs<TestExternalEnvs> {
    fn from(value: TestExternalEnvs) -> Self {
        Self { oracle_env: value }
    }
}

impl<'a> From<&'a TestExternalEnvs> for ExternalEnvs<&'a TestExternalEnvs> {
    fn from(value: &'a TestExternalEnvs) -> Self {
        ExternalEnvs { oracle_env: value.clone() }
    }
}

impl<Error: Unpin + Clone + Display + 'static> TestExternalEnvs<Error> {
    /// Creates a new test environment with empty oracle storage.
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
            oracle_storage: Rc::new(RefCell::new(HashMap::default())),
            recorded_hints: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Returns all recorded hints from `on_hint` calls.
    ///
    /// This is useful for testing that the hint mechanism is working correctly.
    pub fn recorded_hints(&self) -> Vec<RecordedHint> {
        self.recorded_hints.borrow().clone()
    }

    /// Clears all recorded hints.
    pub fn clear_recorded_hints(&self) {
        self.recorded_hints.borrow_mut().clear();
    }

    /// Configures a storage slot in the oracle contract to have a specific value.
    ///
    /// # Arguments
    ///
    /// * `slot` - The storage slot key
    /// * `value` - The value to store
    ///
    /// # Returns
    ///
    /// `self` for method chaining
    pub fn with_oracle_storage(self, slot: U256, value: U256) -> Self {
        self.oracle_storage.borrow_mut().insert(slot, value);
        self
    }

    /// Removes all configured oracle storage values.
    ///
    /// After calling this, all oracle storage queries will return `None`.
    pub fn clear_oracle_storage(&self) {
        self.oracle_storage.borrow_mut().clear();
    }
}

impl<Error: Unpin + Clone + Display> ExternalEnvFactory for TestExternalEnvs<Error> {
    type EnvTypes = Self;

    fn external_envs(&self) -> ExternalEnvs<Self::EnvTypes> {
        ExternalEnvs { oracle_env: self.clone() }
    }
}

impl<Error: Unpin + Display> ExternalEnvTypes for TestExternalEnvs<Error> {
    type OracleEnv = Self;
}

impl<Error: Unpin + Display> OracleEnv for TestExternalEnvs<Error> {
    fn get_oracle_storage(&self, slot: U256) -> Option<U256> {
        self.oracle_storage.borrow().get(&slot).copied()
    }

    fn on_hint(&self, from: Address, topic: B256, data: Bytes) {
        self.recorded_hints.borrow_mut().push(RecordedHint { from, topic, data });
    }
}

/// Length of a storage slot key in bytes (32 bytes for U256).
const SLOT_KEY_LEN: usize = B256::len_bytes();
/// Length of an account address in bytes (20 bytes).
const PLAIN_ACCOUNT_KEY_LEN: usize = Address::len_bytes();
/// Length of a combined address+slot key (52 bytes = 20 + 32).
const PLAIN_STORAGE_KEY_LEN: usize = PLAIN_ACCOUNT_KEY_LEN + SLOT_KEY_LEN;

/// Database wrapper that adds `salt_bucket_capacity` support for testing.
///
/// This wrapper delegates all database operations to the inner database,
/// but adds configurable bucket capacity tracking for testing dynamic gas costs.
#[derive(derive_more::Debug, Clone)]
pub struct TestDatabaseWrapper<DB> {
    /// The inner database implementation.
    #[debug(skip)]
    pub inner: DB,
    /// Bucket capacities. Maps bucket IDs to their capacity values.
    /// Buckets not in this map default to [`MIN_BUCKET_SIZE`](crate::MIN_BUCKET_SIZE).
    #[debug(skip)]
    bucket_capacity: Rc<RefCell<HashMap<BucketId, u64>>>,
}

impl<DB> TestDatabaseWrapper<DB> {
    /// Creates a new test database wrapper.
    pub fn new(db: DB) -> Self {
        Self { inner: db, bucket_capacity: Rc::new(RefCell::new(HashMap::default())) }
    }

    /// Configures a bucket to have a specific capacity.
    ///
    /// This affects the gas multiplier calculation for operations on accounts or storage
    /// slots mapped to this bucket. The multiplier will be `capacity / MIN_BUCKET_SIZE`.
    ///
    /// # Arguments
    ///
    /// * `bucket_id` - The bucket ID to configure
    /// * `capacity` - The bucket capacity in number of slots
    ///
    /// # Returns
    ///
    /// `self` for method chaining
    pub fn with_bucket_capacity(self, bucket_id: BucketId, capacity: u64) -> Self {
        self.with_bucket_capacities([(bucket_id, capacity)])
    }

    /// Configures multiple bucket capacities at once.
    ///
    /// # Returns
    ///
    /// `self` for method chaining
    pub fn with_bucket_capacities(
        self,
        bucket_capacities: impl IntoIterator<Item = (BucketId, u64)>,
    ) -> Self {
        self.bucket_capacity.borrow_mut().extend(bucket_capacities);
        self
    }

    /// Removes all configured bucket capacities.
    ///
    /// After calling this, all buckets will return the default minimum capacity.
    pub fn clear_bucket_capacity(&self) {
        self.bucket_capacity.borrow_mut().clear();
    }

    /// Maps an account address to its bucket ID using SALT hashing.
    pub fn bucket_id_for_account(account: Address) -> BucketId {
        salt::state::hasher::bucket_id(account.as_slice())
    }

    /// Maps a storage slot to its bucket ID using SALT hashing.
    pub fn bucket_id_for_slot(address: Address, key: U256) -> BucketId {
        salt::state::hasher::bucket_id(
            address.concat_const::<SLOT_KEY_LEN, PLAIN_STORAGE_KEY_LEN>(key.into()).as_slice(),
        )
    }

    fn salt_bucket_capacity_value(&self, address: Address, slot: Option<U256>) -> (BucketId, u64) {
        let bucket_id = if let Some(key) = slot {
            Self::bucket_id_for_slot(address, key)
        } else {
            Self::bucket_id_for_account(address)
        };

        let capacity = self
            .bucket_capacity
            .borrow()
            .get(&bucket_id)
            .copied()
            .unwrap_or(salt::constant::MIN_BUCKET_SIZE as u64);

        (bucket_id, capacity)
    }
}

impl<DB: revm::Database> revm::Database for TestDatabaseWrapper<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        self.inner.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<revm::state::Bytecode, Self::Error> {
        self.inner.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }

    fn salt_bucket_capacity(
        &self,
        address: Address,
        slot: Option<U256>,
    ) -> Result<(u32, u64), Self::Error> {
        Ok(self.salt_bucket_capacity_value(address, slot))
    }
}

impl<DB: revm::DatabaseRef> revm::DatabaseRef for TestDatabaseWrapper<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        self.inner.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm::state::Bytecode, Self::Error> {
        self.inner.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.inner.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }

    fn salt_bucket_capacity_ref(
        &self,
        address: Address,
        slot: Option<U256>,
    ) -> Result<(u32, u64), Self::Error> {
        Ok(self.salt_bucket_capacity_value(address, slot))
    }
}

impl<DB: revm::DatabaseCommit> revm::DatabaseCommit for TestDatabaseWrapper<DB> {
    fn commit(&mut self, changes: revm::primitives::HashMap<Address, revm::state::Account>) {
        self.inner.commit(changes);
    }
}
