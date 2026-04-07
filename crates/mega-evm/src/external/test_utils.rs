//! In-memory external environment implementations.
//!
//! Provides [`TestExternalEnvs`], a configurable in-memory implementation of SALT and Oracle
//! environments backed by `HashMap`s.
//! Unlike [`EmptyExternalEnv`](crate::EmptyExternalEnv) which returns hardcoded defaults,
//! this implementation allows setting specific bucket capacities and oracle storage values.
//!
//! # Use Cases
//!
//! - **Unit and integration tests**: Configure bucket capacities and oracle storage to exercise
//!   specific gas pricing and oracle access paths without a real database.
//! - **CLI tools** (e.g., `mega-evme`): Simulate EVM execution with user-specified bucket
//!   capacities and oracle state, useful for offline transaction analysis and debugging.
//! - **Standalone EVM runners**: Any context where a full node database is unavailable but
//!   controllable external environment state is needed.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{
    cell::RefCell,
    convert::Infallible,
    fmt::{Debug, Display},
};
use std::{rc::Rc, vec::Vec};

use alloy_primitives::{Address, BlockNumber, Bytes, B256, U256};
use revm::primitives::HashMap;

use crate::{BucketId, ExternalEnvFactory, ExternalEnvTypes, ExternalEnvs, OracleEnv, SaltEnv};

/// Strategy trait for computing bucket IDs from raw key bytes.
///
/// Implementations determine how account addresses and storage slot keys are mapped to
/// SALT bucket IDs. The trait only requires a single static method, making it zero-cost
/// to parameterize [`TestExternalEnvs`] over different hashing strategies.
pub trait BucketHasher: Debug + Clone + Unpin + 'static {
    /// Computes a bucket ID from the given key bytes.
    fn bucket_id(key: &[u8]) -> BucketId;
}

/// Simple deterministic hasher for tests.
///
/// Uses FNV-1a to produce consistent bucket IDs. This is NOT the real SALT hash algorithm;
/// tests only need consistency (same input produces same output), not compatibility with
/// the production SALT trie.
#[derive(Debug, Clone, Copy)]
pub struct SimpleBucketHasher;

impl BucketHasher for SimpleBucketHasher {
    fn bucket_id(key: &[u8]) -> BucketId {
        let mut hash: u64 = 0xcbf29ce484222325;
        for &byte in key {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        // Map to valid bucket range [NUM_META_BUCKETS, NUM_BUCKETS)
        const NUM_BUCKETS: u64 = 1 << 24; // 16,777,216
        const NUM_META_BUCKETS: u64 = NUM_BUCKETS / 256; // 65,536
        const NUM_KV_BUCKETS: u64 = NUM_BUCKETS - NUM_META_BUCKETS;
        (hash % NUM_KV_BUCKETS + NUM_META_BUCKETS) as BucketId
    }
}

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

/// In-memory external environment implementation backed by `HashMap`s.
///
/// Provides configurable SALT bucket capacities, oracle storage values, and oracle hint
/// recording, all stored in memory.
/// Suitable for unit tests, integration tests, CLI tools, and any standalone EVM execution
/// context where a real node database is unavailable.
///
/// # Bucket Hashing
///
/// The `Hasher` type parameter controls how account addresses and storage slot keys are
/// mapped to bucket IDs.
/// The default [`SimpleBucketHasher`] uses FNV-1a — sufficient for tests where only
/// consistency matters, not production compatibility.
/// For production-compatible bucket IDs (matching the `salt` crate), supply a hasher that
/// implements the real SALT hashing algorithm.
///
/// # Example
/// ```ignore
/// let env = TestExternalEnvs::new()
///     .with_bucket_capacity(123, 512)  // Set bucket 123 to 512 capacity
///     .with_oracle_storage(U256::ZERO, U256::from(42));  // Set oracle slot 0 to 42
/// ```
#[derive(derive_more::Debug, Clone)]
pub struct TestExternalEnvs<Error = Infallible, Hasher = SimpleBucketHasher> {
    #[debug(ignore)]
    _phantom: core::marker::PhantomData<(Error, Hasher)>,
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

impl<H: BucketHasher> From<TestExternalEnvs<Infallible, H>>
    for ExternalEnvs<TestExternalEnvs<Infallible, H>>
{
    fn from(value: TestExternalEnvs<Infallible, H>) -> Self {
        Self { salt_env: value.clone(), oracle_env: value }
    }
}

impl<'a, H: BucketHasher> From<&'a TestExternalEnvs<Infallible, H>>
    for ExternalEnvs<&'a TestExternalEnvs<Infallible, H>>
{
    fn from(value: &'a TestExternalEnvs<Infallible, H>) -> Self {
        ExternalEnvs { salt_env: value.clone(), oracle_env: value.clone() }
    }
}

impl<Error: Unpin + Clone + Display + 'static, Hasher: BucketHasher>
    TestExternalEnvs<Error, Hasher>
{
    /// Creates a new environment with empty bucket capacity and oracle storage.
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

impl<Error: Unpin + Clone + Display, Hasher: BucketHasher> ExternalEnvFactory
    for TestExternalEnvs<Error, Hasher>
{
    type EnvTypes = Self;

    fn external_envs(&self, _block: BlockNumber) -> ExternalEnvs<Self::EnvTypes> {
        ExternalEnvs { salt_env: self.clone(), oracle_env: self.clone() }
    }
}

impl<Error: Unpin + Display, Hasher: BucketHasher> ExternalEnvTypes
    for TestExternalEnvs<Error, Hasher>
{
    type SaltEnv = Self;

    type OracleEnv = Self;
}

/// Length of a storage slot key in bytes (32 bytes for U256).
const SLOT_KEY_LEN: usize = B256::len_bytes();
/// Length of an account address in bytes (20 bytes).
const PLAIN_ACCOUNT_KEY_LEN: usize = Address::len_bytes();
/// Length of a combined address+slot key (52 bytes = 20 + 32).
const PLAIN_STORAGE_KEY_LEN: usize = PLAIN_ACCOUNT_KEY_LEN + SLOT_KEY_LEN;

/// SALT environment implementation with configurable bucket ID hashing.
impl<Error: Unpin + Display, Hasher: BucketHasher> SaltEnv
    for TestExternalEnvs<Error, Hasher>
{
    type Error = Error;

    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        Ok(self
            .bucket_capacity
            .borrow()
            .get(&bucket_id)
            .copied()
            .unwrap_or(crate::MIN_BUCKET_SIZE as u64))
    }

    fn bucket_id_for_account(account: Address) -> BucketId {
        Hasher::bucket_id(account.as_slice())
    }

    fn bucket_id_for_slot(address: Address, key: U256) -> BucketId {
        Hasher::bucket_id(
            address.concat_const::<SLOT_KEY_LEN, PLAIN_STORAGE_KEY_LEN>(key.into()).as_slice(),
        )
    }
}

impl<Error: Unpin + Display, Hasher: BucketHasher> OracleEnv
    for TestExternalEnvs<Error, Hasher>
{
    fn get_oracle_storage(&self, slot: U256) -> Option<U256> {
        self.oracle_storage.borrow().get(&slot).copied()
    }

    fn on_hint(&self, from: Address, topic: B256, data: Bytes) {
        self.recorded_hints.borrow_mut().push(RecordedHint { from, topic, data });
    }
}
