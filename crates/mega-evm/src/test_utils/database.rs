use core::convert::Infallible;

use alloy_primitives::{Address, Bytes, B256, U256};
use delegate::delegate;
use revm::{
    database::{AccountState, CacheDB, DBErrorMarker, EmptyDB},
    primitives::{StorageKey, StorageValue},
    state::{AccountInfo, Bytecode, EvmState},
};

use crate::system::{
    storage_slots::{
        ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, INITIAL_FROM_BLOCK, INITIAL_SEQUENCER,
        INITIAL_SYSTEM_ADDRESS, MIN_ROTATION_DELAY,
    },
    SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE, SEQUENCER_REGISTRY_CODE_HASH,
};

/// The roles of a `SequencerRegistry` a test or a tool puts in a state, with
/// [`MemoryDatabase::sequencer_registry`] or [`storage`](Self::storage).
///
/// Test and tool support, not a chain parameter: the legacy forks deploy the registry (Rex5) and
/// upgrade it to v2.0.0 (Rex6), and Satin neither creates nor seeds it, so a state Satin runs a
/// block on must already hold it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequencerRegistryRoles {
    /// The live system address, and the initial one.
    pub system_address: Address,
    /// The current sequencer, and the initial one.
    pub sequencer: Address,
    /// The admin.
    pub admin: Address,
    /// The minimum delay between scheduling a rotation and its activation, in blocks.
    pub min_rotation_delay: u64,
}

impl From<Address> for SequencerRegistryRoles {
    /// `address` in every role, and a rotation delay of one block.
    fn from(address: Address) -> Self {
        Self { system_address: address, sequencer: address, admin: address, min_rotation_delay: 1 }
    }
}

impl SequencerRegistryRoles {
    /// The account of a v2.0.0 registry, as the legacy forks leave it: the code, and nonce 1, a
    /// created contract's.
    pub fn account_info() -> AccountInfo {
        AccountInfo {
            nonce: 1,
            code_hash: SEQUENCER_REGISTRY_CODE_HASH,
            code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
            ..Default::default()
        }
    }

    /// The storage of a v2.0.0 registry holding these roles, as the legacy forks leave it on a
    /// chain that never rotated one: the seven slots its bootstrap writes, `_initialFromBlock`
    /// zero.
    pub fn storage(&self) -> [(U256, U256); 7] {
        let word = |address: Address| U256::from_be_bytes(address.into_word().0);
        [
            (CURRENT_SYSTEM_ADDRESS, word(self.system_address)),
            (CURRENT_SEQUENCER, word(self.sequencer)),
            (ADMIN, word(self.admin)),
            (INITIAL_SYSTEM_ADDRESS, word(self.system_address)),
            (INITIAL_SEQUENCER, word(self.sequencer)),
            (INITIAL_FROM_BLOCK, U256::ZERO),
            (MIN_ROTATION_DELAY, U256::from(self.min_rotation_delay)),
        ]
    }
}

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
    ///
    /// The bytecode is also stored under its hash. `code_by_hash` reads that map, and a miss falls
    /// through to empty bytecode.
    pub fn set_account_code(&mut self, address: Address, code: Bytes) {
        let bytecode = Bytecode::new_legacy(code);
        let code_hash = bytecode.hash_slow();
        if !bytecode.is_empty() {
            self.db.cache.contracts.entry(code_hash).or_insert_with(|| bytecode.clone());
        }
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

    /// Installs an account whose code is *lazy*: `basic()` returns `code: None` with `code_hash`
    /// set, so loading the code requires a `code_by_hash` call. Mirrors production reth's
    /// `StateProviderDatabase::basic`, which defers code loading. Lets a code-load failure be
    /// exercised independently of `basic()`.
    pub fn account_lazy_code(mut self, address: Address, code_hash: B256) -> Self {
        let account_info = self.db.load_account(address).unwrap();
        account_info.info.code = None;
        account_info.info.code_hash = code_hash;
        account_info.account_state = AccountState::None;
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

    /// Installs the `SequencerRegistry` v2.0.0 holding `roles` — an address names it in every role
    /// — as the legacy forks leave it: [`SequencerRegistryRoles::account_info`] and
    /// [`SequencerRegistryRoles::storage`]. A legacy call from the system address to a whitelisted
    /// contract is then a system-address transaction, and Satin runs a block on the state.
    pub fn sequencer_registry(mut self, roles: impl Into<SequencerRegistryRoles>) -> Self {
        self.db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            SequencerRegistryRoles::account_info(),
        );
        for (slot, value) in roles.into().storage() {
            self.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, slot, value);
        }
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
            fn commit(&mut self, changes: EvmState);
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
    /// When set, `code_by_hash()` calls for this hash return an error. Lets a code load fail
    /// independently of `basic()` (mirrors a node missing a contract's code), to exercise the
    /// `load_code = true` inspect paths in isolation.
    pub fail_on_code_by_hash: Option<B256>,
    /// When set, `block_hash()` calls for this block number return an error.
    pub fail_on_block_hash: Option<u64>,
}

impl ErrorInjectingDatabase {
    /// Creates a new `ErrorInjectingDatabase` wrapping the given [`MemoryDatabase`].
    pub fn new(inner: MemoryDatabase) -> Self {
        Self {
            inner,
            fail_on_account: None,
            fail_on_storage: None,
            fail_on_code_by_hash: None,
            fail_on_block_hash: None,
        }
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
        if self.fail_on_code_by_hash == Some(code_hash) {
            return Err(InjectedDbError(format!("injected code_by_hash() error for {code_hash}")));
        }
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
        if self.fail_on_block_hash == Some(number) {
            return Err(InjectedDbError(format!("injected block_hash() error for {number}")));
        }
        self.inner.block_hash(number).map_err(|e| match e {})
    }
}

impl revm::DatabaseCommit for ErrorInjectingDatabase {
    fn commit(&mut self, changes: EvmState) {
        self.inner.commit(changes);
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, Bytes};
    use revm::Database;

    use super::*;

    #[test]
    fn test_set_account_code_is_served_by_code_by_hash() {
        let address = address!("00000000000000000000000000000000000000aa");
        let code = Bytes::from_static(&[0x00]);
        let mut db = MemoryDatabase::default().account_balance(address, U256::from(5));
        db.set_account_code(address, code.clone());

        let info = db.basic(address).unwrap().expect("the account is present");
        assert_eq!(info.balance, U256::from(5));
        let hash = info.code_hash;
        assert_eq!(info.code.expect("code is stored on the account").original_bytes(), code);
        assert_eq!(db.code_by_hash(hash).unwrap().original_bytes(), code);
    }

    #[test]
    fn test_block_hash_can_fail_for_one_number() {
        let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
        let served = db.block_hash(1).unwrap();
        db.fail_on_block_hash = Some(7);
        let err = db.block_hash(7).unwrap_err();
        assert!(err.to_string().contains("injected block_hash() error for 7"), "{err}");
        assert_eq!(db.block_hash(1).unwrap(), served);
    }
}
