use core::{cell::RefCell, convert::Infallible};
use std::rc::Rc;

use alloy_primitives::{Address, BlockNumber, B256, U256};
use revm::primitives::HashMap;

use crate::{BucketId, ExternalEnvFactory, ExternalEnvTypes, ExternalEnvs, OracleEnv, SaltEnv};

/// Default implementation of [`ExternalEnvs`] that provides no-op implementations for all
/// external environments.
///
/// This is useful when the EVM does not need to access any additional information from an
/// external environment.
#[derive(derive_more::Debug, Clone)]
pub struct TestExternalEnvs<Error = Infallible> {
    #[debug(ignore)]
    _phantom: core::marker::PhantomData<Error>,
    /// Oracle storage for testing purposes. Maps storage slots to their values.
    oracle_storage: Rc<RefCell<HashMap<U256, U256>>>,
    /// Bucket capacity storage for testing purposes. Maps (`bucket_id`, `block_number`) to
    /// capacity.
    bucket_capacity: Rc<RefCell<HashMap<BucketId, u64>>>,
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
    /// Creates a new [`DefaultExternalEnvs`].
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
            oracle_storage: Rc::new(RefCell::new(HashMap::default())),
            bucket_capacity: Rc::new(RefCell::new(HashMap::default())),
        }
    }

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
    pub fn with_bucket_capacity(self, bucket_id: BucketId, capacity: u64) -> Self {
        self.bucket_capacity.borrow_mut().insert(bucket_id, capacity);
        self
    }

    /// Clears all bucket capacity values.
    pub fn clear_bucket_capacity(&self) {
        self.bucket_capacity.borrow_mut().clear();
    }

    /// Sets an oracle storage slot to a specific value for testing purposes.
    ///
    /// # Arguments
    ///
    /// * `slot` - The storage slot to set
    /// * `value` - The value to set at the given slot
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_oracle_storage(self, slot: U256, value: U256) -> Self {
        self.oracle_storage.borrow_mut().insert(slot, value);
        self
    }

    /// Clears all oracle storage values.
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

/// data length of Key of Storage Slot
const SLOT_KEY_LEN: usize = B256::len_bytes();
/// data length of Key of Account
const PLAIN_ACCOUNT_KEY_LEN: usize = Address::len_bytes();
/// data length of Key of Storage
const PLAIN_STORAGE_KEY_LEN: usize = PLAIN_ACCOUNT_KEY_LEN + SLOT_KEY_LEN;

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

    fn bucket_id_for_account(account: Address) -> BucketId {
        salt::state::hasher::bucket_id(account.as_slice())
    }

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
}
