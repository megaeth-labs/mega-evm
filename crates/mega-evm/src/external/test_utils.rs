//! Test utilities for external environment implementations.
//!
//! Provides [`TestExternalEnvs`], a configurable mock implementation of SALT and Oracle
//! environments for use in tests. Unlike [`EmptyExternalEnv`](crate::EmptyExternalEnv),
//! this implementation allows setting specific bucket capacities and oracle storage values.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{cell::RefCell, convert::Infallible};
use std::{rc::Rc, vec::Vec};

use alloy_primitives::{Address, BlockNumber, Bytes, B256, U256};
use revm::primitives::HashMap;

use crate::{BucketId, ExternalEnvFactory, ExternalEnvTypes, ExternalEnvs, OracleEnv, SaltEnv};

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
/// This struct provides mutable state for bucket capacities, oracle storage, and recorded hints,
/// allowing tests to set up specific scenarios and verify hint mechanism behavior. Bucket IDs are
/// calculated using the real SALT hashing logic from the `salt` crate.
///
/// # Example
/// ```ignore
/// let env = TestExternalEnvs::new()
///     .with_bucket_capacity(123, 512)  // Set bucket 123 to 512 capacity
///     .with_oracle_storage(U256::ZERO, U256::from(42));  // Set oracle slot 0 to 42
/// ```
#[derive(derive_more::Debug, Clone)]
pub struct TestExternalEnvs<Error = Infallible> {
    #[debug(ignore)]
    _phantom: core::marker::PhantomData<Error>,
    /// Oracle contract storage values. Maps storage slot keys to their values.
    oracle_storage: Rc<RefCell<HashMap<U256, U256>>>,
    /// Bucket capacities. Maps bucket IDs to their capacity values.
    /// Buckets not in this map default to [`MIN_BUCKET_SIZE`](crate::MIN_BUCKET_SIZE).
    bucket_capacity: Rc<RefCell<HashMap<BucketId, u64>>>,
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
        Self { salt_env: value.clone(), oracle_env: value }
    }
}

impl<'a> From<&'a TestExternalEnvs> for ExternalEnvs<&'a TestExternalEnvs> {
    fn from(value: &'a TestExternalEnvs) -> Self {
        ExternalEnvs { salt_env: value.clone(), oracle_env: value.clone() }
    }
}

impl<Error: Unpin + Clone + 'static> TestExternalEnvs<Error> {
    /// Creates a new test environment with empty bucket capacity and oracle storage.
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
            oracle_storage: Rc::new(RefCell::new(HashMap::default())),
            bucket_capacity: Rc::new(RefCell::new(HashMap::default())),
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
        self.bucket_capacity.borrow_mut().insert(bucket_id, capacity);
        self
    }

    /// Removes all configured bucket capacities.
    ///
    /// After calling this, all buckets will return the default minimum capacity.
    pub fn clear_bucket_capacity(&self) {
        self.bucket_capacity.borrow_mut().clear();
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

impl<Error: Unpin + Clone> ExternalEnvFactory for TestExternalEnvs<Error> {
    type EnvTypes = Self;

    fn external_envs(&self, _block: BlockNumber) -> ExternalEnvs<Self::EnvTypes> {
        ExternalEnvs { salt_env: self.clone(), oracle_env: self.clone() }
    }
}

impl<Error: Unpin> ExternalEnvTypes for TestExternalEnvs<Error> {
    type SaltEnv = Self;

    type OracleEnv = Self;
}

/// Length of a storage slot key in bytes (32 bytes for U256).
const SLOT_KEY_LEN: usize = B256::len_bytes();
/// Length of an account address in bytes (20 bytes).
const PLAIN_ACCOUNT_KEY_LEN: usize = Address::len_bytes();
/// Length of a combined address+slot key (52 bytes = 20 + 32).
const PLAIN_STORAGE_KEY_LEN: usize = PLAIN_ACCOUNT_KEY_LEN + SLOT_KEY_LEN;

/// SALT environment implementation using real bucket ID hashing.
///
/// Bucket IDs are calculated using the SALT hasher from the `salt` crate, which provides
/// deterministic mapping of accounts and storage slots to buckets.
impl<Error: Unpin> SaltEnv for TestExternalEnvs<Error> {
    type Error = Error;

    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        Ok(self
            .bucket_capacity
            .borrow()
            .get(&bucket_id)
            .copied()
            .unwrap_or(salt::constant::MIN_BUCKET_SIZE as u64))
    }

    /// Maps accounts to buckets by hashing the address.
    fn bucket_id_for_account(account: Address) -> BucketId {
        salt::state::hasher::bucket_id(account.as_slice())
    }

    /// Maps storage slots to buckets by hashing the concatenation of address and slot key.
    fn bucket_id_for_slot(address: Address, key: U256) -> BucketId {
        salt::state::hasher::bucket_id(
            address.concat_const::<SLOT_KEY_LEN, PLAIN_STORAGE_KEY_LEN>(key.into()).as_slice(),
        )
    }
}

impl<Error: Unpin> OracleEnv for TestExternalEnvs<Error> {
    fn get_oracle_storage(&self, slot: U256) -> Option<U256> {
        self.oracle_storage.borrow().get(&slot).copied()
    }

    fn on_hint(&self, from: Address, topic: B256, data: Bytes) {
        self.recorded_hints.borrow_mut().push(RecordedHint { from, topic, data });
    }
}
