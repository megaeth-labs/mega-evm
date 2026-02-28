use core::convert::Infallible;

use alloy_primitives::{Address, Bytes, B256, U256};
use delegate::delegate;
use revm::{
    database::{AccountState, CacheDB, DBErrorMarker, EmptyDB},
    primitives::{HashMap, StorageKey, StorageValue},
    state::{Account, AccountInfo, Bytecode},
};

/// A memory database for testing purposes.
#[derive(Debug, Default, Clone, derive_more::Deref, derive_more::DerefMut)]
pub struct MemoryDatabase {
    #[deref]
    #[deref_mut]
    db: CacheDB<EmptyDB>,
}

impl MemoryDatabase {
    /// Creates a new `MemoryDatabase` from a `CacheDB`.
    pub fn from_cache_db(db: CacheDB<EmptyDB>) -> Self {
        Self { db }
    }

    /// Sets the code for an account in the database.
    pub fn set_account_code(&mut self, address: Address, code: Bytes) {
        let bytecode = Bytecode::new_legacy(code);
        let code_hash = bytecode.hash_slow();
        let account_info = self.db.load_account(address).unwrap();
        account_info.info.code = Some(bytecode);
        account_info.info.code_hash = code_hash;
        account_info.account_state = AccountState::None;
    }

    /// Sets the code for an account in the database.
    pub fn account_code(mut self, address: Address, code: Bytes) -> Self {
        self.set_account_code(address, code);
        self
    }

    /// Sets the balance for an account in the database.
    pub fn set_account_balance(&mut self, address: Address, balance: U256) {
        let account_info = self.db.load_account(address).unwrap();
        account_info.info.balance = balance;
        account_info.account_state = AccountState::None;
    }

    /// Sets the balance for an account in the database.
    pub fn account_balance(mut self, address: Address, balance: U256) -> Self {
        self.set_account_balance(address, balance);
        self
    }

    /// Sets the nonce for an account in the database.
    pub fn set_account_nonce(&mut self, address: Address, nonce: u64) {
        let account_info = self.db.load_account(address).unwrap();
        account_info.info.nonce = nonce;
        account_info.account_state = AccountState::None;
    }

    /// Sets the nonce for an account in the database.
    pub fn account_nonce(mut self, address: Address, nonce: u64) -> Self {
        self.set_account_nonce(address, nonce);
        self
    }

    /// Sets the storage for an account in the database.
    pub fn set_account_storage(
        &mut self,
        address: Address,
        storage_key: StorageKey,
        value: StorageValue,
    ) {
        let account_info = self.db.load_account(address).unwrap();
        account_info.storage.insert(storage_key, value);
        account_info.account_state = AccountState::None;
    }

    /// Sets the storage for an account in the database.
    pub fn account_storage(
        mut self,
        address: Address,
        storage_key: StorageKey,
        value: StorageValue,
    ) -> Self {
        self.set_account_storage(address, storage_key, value);
        self
    }
}

impl revm::Database for MemoryDatabase {
    type Error = Infallible;

    delegate! {
        to self.db {
            fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error>;
            fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error>;
            fn storage(&mut self, address: Address, index: StorageKey) -> Result<StorageValue, Self::Error>;
            fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error>;
        }
    }
}

impl revm::DatabaseCommit for MemoryDatabase {
    delegate! {
        to self.db {
            fn commit(&mut self, changes: revm::primitives::HashMap<Address, revm::state::Account>);
        }
    }
}

/// Error type for [`ErrorInjectingDatabase`].
#[derive(Debug, Clone, PartialEq, Eq, derive_more::Display, derive_more::Error)]
#[display("{_0}")]
pub struct InjectedDbError(#[error(not(source))] pub String);

impl DBErrorMarker for InjectedDbError {}

/// A database wrapper that injects errors on configurable account or storage lookups.
///
/// Used to test DB error handling paths (e.g., `FatalExternalError` when `inspect_*` fails).
/// Wraps a [`MemoryDatabase`] and can be configured to fail on specific `basic()` or `storage()`
/// calls.
#[derive(Debug, Clone, derive_more::Deref, derive_more::DerefMut)]
pub struct ErrorInjectingDatabase {
    #[deref]
    #[deref_mut]
    inner: MemoryDatabase,
    /// When set, `basic()` calls for this address return an error.
    pub fail_on_account: Option<Address>,
    /// When set, `storage()` calls for this (address, key) return an error.
    pub fail_on_storage: Option<(Address, StorageKey)>,
}

impl ErrorInjectingDatabase {
    /// Creates a new `ErrorInjectingDatabase` wrapping the given [`MemoryDatabase`].
    pub fn new(inner: MemoryDatabase) -> Self {
        Self { inner, fail_on_account: None, fail_on_storage: None }
    }
}

impl revm::Database for ErrorInjectingDatabase {
    type Error = InjectedDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if self.fail_on_account == Some(address) {
            return Err(InjectedDbError(format!("injected basic() error for {address}")));
        }
        self.inner.basic(address).map_err(|e| match e {})
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner.code_by_hash(code_hash).map_err(|e| match e {})
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        if self.fail_on_storage == Some((address, index)) {
            return Err(InjectedDbError(format!("injected storage() error for {address}:{index}")));
        }
        self.inner.storage(address, index).map_err(|e| match e {})
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number).map_err(|e| match e {})
    }
}

impl revm::DatabaseCommit for ErrorInjectingDatabase {
    fn commit(&mut self, changes: HashMap<Address, Account>) {
        self.inner.commit(changes);
    }
}
