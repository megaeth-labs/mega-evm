//! Sandbox database for isolated EVM execution.
//!
//! This module uses type erasure to prevent infinite type instantiation
//! (`SandboxDb<SandboxDb<...>>`) that would cause a compiler ICE during monomorphization.

#![allow(unreachable_pub)] // Types are pub for trait impls but module is private

#[cfg(not(feature = "std"))]
use alloc as std;
use std::string::String;

use alloy_primitives::{map::HashMap, Address, B256};
use core::cell::RefCell;
use revm::{
    database::DBErrorMarker,
    primitives::{StorageKey, StorageValue, KECCAK_EMPTY},
    state::{AccountInfo, Bytecode, EvmState},
    Database,
};

/// Error type for sandbox database operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDbError(String);

impl core::fmt::Display for SandboxDbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl core::error::Error for SandboxDbError {}
impl DBErrorMarker for SandboxDbError {}

/// Type-erased database trait for sandbox operations.
///
/// This trait erases the concrete database type to prevent infinite type
/// instantiation when creating nested sandboxes.
///
/// # Why not `Box<dyn Database>`?
///
/// The [`Database`] trait has an associated type `Error`, which makes it not
/// object-safe. You cannot use `Box<dyn Database>` directly because the compiler
/// doesn't know what error type to use at runtime:
///
/// ```ignore
/// pub trait Database {
///     type Error;  // This makes it NOT object-safe
///     fn basic(&mut self, ...) -> Result<..., Self::Error>;
/// }
/// ```
///
/// Even `Box<dyn Database<Error = SandboxDbError>>` won't work because the trait
/// requires `Self::Error` to match exactly - you can't erase a `DB::Error` into
/// a different type through a trait object.
///
/// This `ErasedDatabase` trait solves the problem by:
/// 1. Having no associated types (all methods return our fixed [`SandboxDbError`])
/// 2. Converting errors via `.to_string()` in the [`DatabaseWrapper`] implementation
trait ErasedDatabase {
    fn basic(&self, address: Address) -> Result<Option<AccountInfo>, SandboxDbError>;
    fn storage(&self, address: Address, index: StorageKey) -> Result<StorageValue, SandboxDbError>;
    fn block_hash(&self, number: u64) -> Result<B256, SandboxDbError>;
    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, SandboxDbError>;
}

/// Wrapper that implements [`ErasedDatabase`] for any [`Database`] type.
///
/// Uses `RefCell` for interior mutability since `Database` methods take `&mut self`.
struct DatabaseWrapper<'a, DB> {
    db: RefCell<&'a mut DB>,
}

impl<DB: Database> ErasedDatabase for DatabaseWrapper<'_, DB>
where
    DB::Error: core::fmt::Display,
{
    #[inline]
    fn basic(&self, address: Address) -> Result<Option<AccountInfo>, SandboxDbError> {
        self.db.borrow_mut().basic(address).map_err(|e| SandboxDbError(e.to_string()))
    }

    #[inline]
    fn storage(&self, address: Address, index: StorageKey) -> Result<StorageValue, SandboxDbError> {
        self.db.borrow_mut().storage(address, index).map_err(|e| SandboxDbError(e.to_string()))
    }

    #[inline]
    fn block_hash(&self, number: u64) -> Result<B256, SandboxDbError> {
        self.db.borrow_mut().block_hash(number).map_err(|e| SandboxDbError(e.to_string()))
    }

    #[inline]
    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, SandboxDbError> {
        self.db.borrow_mut().code_by_hash(code_hash).map_err(|e| SandboxDbError(e.to_string()))
    }
}

/// A sandbox database for isolated EVM execution.
///
/// Used for keyless deploy sandbox where we need to read from the parent state
/// directly without cloning the entire state upfront.
///
/// **Important**: This type uses lifetime parameters to hold references to the
/// parent journal's state and database, avoiding expensive cloning. Values are
/// cloned lazily only when accessed.
pub struct SandboxDb<'a> {
    /// Reference to the parent journal's state.
    journal_state: &'a EvmState,
    /// Type-erased reference to the underlying database for cache misses.
    db: Box<dyn ErasedDatabase + 'a>,
    /// Index from `code_hash` to address for O(1) bytecode lookup.
    code_index: HashMap<B256, Address>,
    /// Address whose nonce should be overridden to 0 (for keyless deploy).
    nonce_override_address: Option<Address>,
}

impl<'a> core::fmt::Debug for SandboxDb<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SandboxDb").finish_non_exhaustive()
    }
}

impl<'a> SandboxDb<'a> {
    /// Creates a new sandbox database with references to the journal's state and database.
    ///
    /// This does NOT clone the state or database - it holds references and clones values
    /// lazily when accessed. The `code_index` is built upfront for O(1) bytecode lookup.
    ///
    /// # Arguments
    ///
    /// * `state` - Reference to the journal's cached state
    /// * `db` - Mutable reference to the underlying database for cache misses
    ///
    /// # Split Borrowing
    ///
    /// This constructor takes separate references to state and database to allow
    /// split borrowing at the call site. The caller should do:
    /// ```ignore
    /// let journal = ctx.journal_mut();
    /// let sandbox_db = SandboxDb::new(&journal.inner.state, &mut journal.database);
    /// ```
    pub fn new<DB>(state: &'a EvmState, db: &'a mut DB) -> Self
    where
        DB: Database,
        DB::Error: core::fmt::Display,
    {
        // Build code index for O(1) bytecode lookup
        let mut code_index = HashMap::default();
        for (addr, account) in state {
            if let Some(code) = &account.info.code {
                if !code.is_empty() {
                    code_index.insert(account.info.code_hash, *addr);
                }
            }
        }

        Self {
            journal_state: state,
            db: Box::new(DatabaseWrapper { db: RefCell::new(db) }),
            code_index,
            nonce_override_address: None,
        }
    }

    /// Sets an address whose nonce should be overridden to 0.
    ///
    /// This is used for keyless deploy where the transaction must have nonce=0
    /// regardless of the signer's actual nonce in the database.
    pub fn with_nonce_override(mut self, address: Address) -> Self {
        self.nonce_override_address = Some(address);
        self
    }
}

impl Database for SandboxDb<'_> {
    type Error = SandboxDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Check journal state first - clone only when accessed
        if let Some(account) = self.journal_state.get(&address) {
            let mut info = account.info.clone();
            // Override nonce to 0 for the keyless deploy signer
            if self.nonce_override_address == Some(address) {
                info.nonce = 0;
            }
            return Ok(Some(info));
        }
        // Not found in journal state - query underlying database
        let result = self.db.basic(address)?;
        // Override nonce to 0 for the keyless deploy signer
        if let Some(mut info) = result {
            if self.nonce_override_address == Some(address) {
                info.nonce = 0;
            }
            return Ok(Some(info));
        }
        Ok(None)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if code_hash == KECCAK_EMPTY {
            return Ok(Bytecode::default());
        }

        // Use index for O(1) lookup in journal state
        if let Some(addr) = self.code_index.get(&code_hash) {
            if let Some(account) = self.journal_state.get(addr) {
                return Ok(account.info.code.clone().unwrap_or_default());
            }
        }

        // Not found in journal state - query underlying database
        self.db.code_by_hash(code_hash)
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        // Check journal state for cached storage values
        if let Some(account) = self.journal_state.get(&address) {
            if let Some(slot) = account.storage.get(&index) {
                return Ok(slot.present_value);
            }
        }
        // Not found in journal state - query underlying database
        self.db.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        // Block hashes are not stored in journal state - query database
        self.db.block_hash(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256, Bytes, U256};
    use revm::{
        context::JournalTr,
        database::EmptyDB,
        state::{AccountStatus, EvmStorageSlot},
        Journal,
    };

    const TEST_ADDR_1: Address = address!("1111111111111111111111111111111111111111");
    const TEST_ADDR_2: Address = address!("2222222222222222222222222222222222222222");
    const TEST_ADDR_3: Address = address!("3333333333333333333333333333333333333333");

    fn create_test_journal() -> Journal<EmptyDB> {
        let mut journal = Journal::<EmptyDB>::new(EmptyDB::default());
        // Add a test account to the journal
        let account = revm::state::Account {
            info: AccountInfo {
                balance: U256::from(1000),
                nonce: 1,
                code_hash: KECCAK_EMPTY,
                code: None,
            },
            transaction_id: 0,
            storage: Default::default(),
            status: AccountStatus::empty(),
        };
        journal.inner.state.insert(TEST_ADDR_1, account);
        journal
    }

    fn create_journal_with_contract() -> Journal<EmptyDB> {
        let mut journal = Journal::<EmptyDB>::new(EmptyDB::default());

        // Create bytecode and compute its hash
        let bytecode_bytes = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]); // PUSH0 PUSH0 RETURN
        let bytecode = Bytecode::new_raw(bytecode_bytes.clone());
        let code_hash = keccak256(&bytecode_bytes);

        // Add contract account with bytecode
        let contract_account = revm::state::Account {
            info: AccountInfo { balance: U256::ZERO, nonce: 1, code_hash, code: Some(bytecode) },
            transaction_id: 0,
            storage: Default::default(),
            status: AccountStatus::empty(),
        };
        journal.inner.state.insert(TEST_ADDR_2, contract_account);
        journal
    }

    fn create_journal_with_storage() -> Journal<EmptyDB> {
        let mut journal = Journal::<EmptyDB>::new(EmptyDB::default());

        // Add account with storage
        let mut storage = HashMap::default();
        storage.insert(U256::from(1), EvmStorageSlot::new_changed(U256::ZERO, U256::from(42), 0));
        storage
            .insert(U256::from(100), EvmStorageSlot::new_changed(U256::ZERO, U256::from(999), 0));

        let account = revm::state::Account {
            info: AccountInfo {
                balance: U256::from(5000),
                nonce: 10,
                code_hash: KECCAK_EMPTY,
                code: None,
            },
            transaction_id: 0,
            storage,
            status: AccountStatus::empty(),
        };
        journal.inner.state.insert(TEST_ADDR_1, account);
        journal
    }

    // ==================== basic() tests ====================

    #[test]
    fn test_basic_returns_account_from_journal() {
        let mut journal = create_test_journal();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        let result = sandbox.basic(TEST_ADDR_1).unwrap();
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.balance, U256::from(1000));
        assert_eq!(info.nonce, 1);
    }

    #[test]
    fn test_basic_queries_database_on_cache_miss() {
        let mut journal = create_test_journal();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // TEST_ADDR_3 is not in journal, so it queries EmptyDB which returns None
        let result = sandbox.basic(TEST_ADDR_3).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_basic_returns_contract_account_info() {
        let mut journal = create_journal_with_contract();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        let result = sandbox.basic(TEST_ADDR_2).unwrap();
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.nonce, 1);
        assert_ne!(info.code_hash, KECCAK_EMPTY);
    }

    // ==================== storage() tests ====================

    #[test]
    fn test_storage_returns_value_from_journal() {
        let mut journal = create_journal_with_storage();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        let value = sandbox.storage(TEST_ADDR_1, U256::from(1)).unwrap();
        assert_eq!(value, U256::from(42));

        let value = sandbox.storage(TEST_ADDR_1, U256::from(100)).unwrap();
        assert_eq!(value, U256::from(999));
    }

    #[test]
    fn test_storage_returns_zero_for_unset_slot_in_journal() {
        let mut journal = create_journal_with_storage();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // Slot 50 is not set in journal storage, queries database (EmptyDB returns zero)
        let value = sandbox.storage(TEST_ADDR_1, U256::from(50)).unwrap();
        assert_eq!(value, U256::ZERO);
    }

    #[test]
    fn test_storage_queries_database_for_unknown_account() {
        let mut journal = create_test_journal();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // TEST_ADDR_3 not in journal, queries EmptyDB which returns zero
        let value = sandbox.storage(TEST_ADDR_3, U256::from(1)).unwrap();
        assert_eq!(value, U256::ZERO);
    }

    // ==================== code_by_hash() tests ====================

    #[test]
    fn test_code_by_hash_returns_empty_for_keccak_empty() {
        let mut journal = create_test_journal();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        let code = sandbox.code_by_hash(KECCAK_EMPTY).unwrap();
        assert!(code.is_empty());
    }

    #[test]
    fn test_code_by_hash_returns_bytecode_from_journal_via_index() {
        let mut journal = create_journal_with_contract();

        // Get the code hash from the contract account
        let code_hash = journal.inner.state.get(&TEST_ADDR_2).unwrap().info.code_hash;

        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        let code = sandbox.code_by_hash(code_hash).unwrap();
        assert!(!code.is_empty());
        // Bytecode may have padding, so check it starts with our expected bytes
        assert!(code.bytes_slice().starts_with(&[0x60, 0x00, 0x60, 0x00, 0xf3]));
    }

    #[test]
    fn test_code_by_hash_queries_database_for_unknown_hash() {
        let mut journal = create_test_journal();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // Random hash not in journal - queries EmptyDB which returns empty bytecode
        let unknown_hash = keccak256(b"unknown code");
        let code = sandbox.code_by_hash(unknown_hash).unwrap();
        assert!(code.is_empty());
    }

    #[test]
    fn test_code_index_built_correctly() {
        let mut journal = create_journal_with_contract();
        let code_hash = journal.inner.state.get(&TEST_ADDR_2).unwrap().info.code_hash;

        let sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // Verify code_index contains the mapping
        assert!(sandbox.code_index.contains_key(&code_hash));
        assert_eq!(sandbox.code_index.get(&code_hash), Some(&TEST_ADDR_2));
    }

    #[test]
    fn test_code_index_excludes_empty_code() {
        let mut journal = create_test_journal();
        let sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // Account with no code should not be in code_index
        assert!(!sandbox.code_index.contains_key(&KECCAK_EMPTY));
        assert!(sandbox.code_index.is_empty());
    }

    // ==================== block_hash() tests ====================

    #[test]
    fn test_block_hash_queries_database() {
        let mut journal = create_test_journal();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // Block hash is always queried from database
        // EmptyDB returns keccak256(block_number)
        let hash = sandbox.block_hash(100).unwrap();
        assert_ne!(hash, B256::ZERO); // EmptyDB returns non-zero hash
    }

    #[test]
    fn test_block_hash_different_blocks_different_hashes() {
        let mut journal = create_test_journal();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        let hash_100 = sandbox.block_hash(100).unwrap();
        let hash_200 = sandbox.block_hash(200).unwrap();

        assert_ne!(hash_100, hash_200);
    }

    // ==================== Multiple accounts tests ====================

    #[test]
    fn test_multiple_accounts_in_journal() {
        let mut journal = Journal::<EmptyDB>::new(EmptyDB::default());

        // Add multiple accounts
        for (i, addr) in [TEST_ADDR_1, TEST_ADDR_2, TEST_ADDR_3].iter().enumerate() {
            let account = revm::state::Account {
                info: AccountInfo {
                    balance: U256::from((i + 1) * 1000),
                    nonce: i as u64,
                    code_hash: KECCAK_EMPTY,
                    code: None,
                },
                transaction_id: 0,
                storage: Default::default(),
                status: AccountStatus::empty(),
            };
            journal.inner.state.insert(*addr, account);
        }

        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // Verify all accounts are accessible
        let info1 = sandbox.basic(TEST_ADDR_1).unwrap().unwrap();
        assert_eq!(info1.balance, U256::from(1000));

        let info2 = sandbox.basic(TEST_ADDR_2).unwrap().unwrap();
        assert_eq!(info2.balance, U256::from(2000));

        let info3 = sandbox.basic(TEST_ADDR_3).unwrap().unwrap();
        assert_eq!(info3.balance, U256::from(3000));
    }

    // ==================== Journal priority tests ====================

    #[test]
    fn test_journal_state_takes_priority_over_database() {
        let mut journal = create_journal_with_storage();
        let mut sandbox = SandboxDb::new(&journal.inner.state, &mut journal.database);

        // Storage slot 1 has value 42 in journal
        // Even if database had a different value, journal takes priority
        let value = sandbox.storage(TEST_ADDR_1, U256::from(1)).unwrap();
        assert_eq!(value, U256::from(42));
    }
}
