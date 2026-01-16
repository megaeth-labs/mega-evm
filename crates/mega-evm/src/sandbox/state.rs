//! Sandbox database for isolated EVM execution.
//!
//! This module uses type erasure to prevent infinite type instantiation
//! (`SandboxDb<SandboxDb<...>>`) that would cause a compiler ICE during monomorphization.

#![allow(unreachable_pub)] // Types are pub for trait impls but module is private

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{boxed::Box, string::String};

use alloy_primitives::{map::HashMap, Address, B256};
use revm::{
    database::DBErrorMarker,
    primitives::{StorageKey, StorageValue, KECCAK_EMPTY},
    state::{AccountInfo, Bytecode},
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

/// A concrete (non-generic) trait for database operations.
///
/// This is used internally by `SandboxDb` to type-erase the underlying database,
/// preventing infinite type instantiation during monomorphization.
trait ErasedDatabase: Send + Sync {
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, SandboxDbError>;
    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, SandboxDbError>;
    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, SandboxDbError>;
    fn block_hash(&mut self, number: u64) -> Result<B256, SandboxDbError>;
}

/// Wrapper that implements `ErasedDatabase` for any `Database`.
struct DatabaseWrapper<DB: Database>(DB);

impl<DB: Database + Send + Sync> ErasedDatabase for DatabaseWrapper<DB>
where
    DB::Error: core::error::Error + Send + Sync + 'static,
{
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, SandboxDbError> {
        self.0.basic(address).map_err(|e| SandboxDbError(e.to_string()))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, SandboxDbError> {
        self.0.code_by_hash(code_hash).map_err(|e| SandboxDbError(e.to_string()))
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, SandboxDbError> {
        self.0.storage(address, index).map_err(|e| SandboxDbError(e.to_string()))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, SandboxDbError> {
        self.0.block_hash(number).map_err(|e| SandboxDbError(e.to_string()))
    }
}

/// A sandbox database for isolated EVM execution.
///
/// Used for keyless deploy sandbox where we need to read from the parent state
/// directly without cloning or requiring `DB: Clone`.
///
/// **Important**: This type uses internal type erasure to prevent infinite type
/// instantiation (`SandboxDb<SandboxDb<...>>`) that would cause a compiler ICE.
/// Unlike a generic approach, `SandboxDb` itself does NOT implement `Database`,
/// which breaks the recursive type chain.
pub struct SandboxDb {
    /// Type-erased database reference.
    db: Box<dyn ErasedDatabase>,
    /// Cache of account states loaded from the parent.
    state_cache: HashMap<Address, Option<AccountInfo>>,
    /// Index from code_hash to address for O(1) bytecode lookup.
    code_index: HashMap<B256, Address>,
}

impl core::fmt::Debug for SandboxDb {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SandboxDb").finish_non_exhaustive()
    }
}

impl SandboxDb {
    /// Creates a new sandbox database from a database reference.
    ///
    /// The database is type-erased to prevent infinite type instantiation.
    #[allow(dead_code)] // Used in tests
    pub fn new<DB>(db: DB) -> Self
    where
        DB: Database + Send + Sync + 'static,
        DB::Error: core::error::Error + Send + Sync + 'static,
    {
        Self {
            db: Box::new(DatabaseWrapper(db)),
            state_cache: HashMap::default(),
            code_index: HashMap::default(),
        }
    }

    /// Creates a new sandbox database with pre-populated state from a journal.
    ///
    /// This captures the current state from the journal. For any state not in the
    /// journal, the sandbox falls back to returning empty/default values.
    ///
    /// **Note**: This does not clone the underlying database. All required accounts
    /// (deploy signer and deploy address) should be loaded into the journal before
    /// calling this function.
    pub fn from_journal<DB>(journal: &revm::Journal<DB>) -> Self
    where
        DB: Database,
    {
        // Capture state from journal
        let mut state_cache = HashMap::default();
        let mut code_index = HashMap::default();

        for (addr, account) in &journal.inner.state {
            state_cache.insert(*addr, Some(account.info.clone()));
            if let Some(code) = &account.info.code {
                if !code.is_empty() {
                    code_index.insert(account.info.code_hash, *addr);
                }
            }
        }

        // Use EmptyDB as fallback - any state not in the cache will return default values
        Self {
            db: Box::new(DatabaseWrapper(revm::database::EmptyDB::default())),
            state_cache,
            code_index,
        }
    }
}

impl Database for SandboxDb {
    type Error = SandboxDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Check cache first
        if let Some(cached) = self.state_cache.get(&address) {
            return Ok(cached.clone());
        }
        // Fall back to underlying database
        let result = self.db.basic(address)?;
        self.state_cache.insert(address, result.clone());
        Ok(result)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if code_hash == KECCAK_EMPTY {
            return Ok(Bytecode::default());
        }

        // Try underlying database first
        let code = self.db.code_by_hash(code_hash)?;
        if !code.is_empty() {
            return Ok(code);
        }

        // Use index for O(1) lookup of newly created contracts
        if let Some(addr) = self.code_index.get(&code_hash) {
            if let Some(Some(account)) = self.state_cache.get(addr) {
                return Ok(account.code.clone().unwrap_or_default());
            }
        }

        Ok(Bytecode::default())
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        // Note: For sandbox, we delegate storage to the underlying database
        // This is a simplified implementation - in production, you might want
        // to cache storage values from the journal as well
        self.db.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, U256};
    use revm::database::EmptyDB;

    #[test]
    fn test_sandbox_db_basic() {
        let empty_db = EmptyDB::default();
        let mut sandbox = SandboxDb::new(empty_db);

        let addr = address!("1111111111111111111111111111111111111111");
        let result = sandbox.basic(addr).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_sandbox_db_caches_results() {
        let empty_db = EmptyDB::default();
        let mut sandbox = SandboxDb::new(empty_db);

        let addr = address!("1111111111111111111111111111111111111111");

        // First call
        let _ = sandbox.basic(addr).unwrap();

        // Should be cached now
        assert!(sandbox.state_cache.contains_key(&addr));
    }

    #[test]
    fn test_sandbox_db_empty_code_hash() {
        let empty_db = EmptyDB::default();
        let mut sandbox = SandboxDb::new(empty_db);

        let code = sandbox.code_by_hash(KECCAK_EMPTY).unwrap();
        assert!(code.is_empty());
    }

    #[test]
    fn test_sandbox_db_storage() {
        let empty_db = EmptyDB::default();
        let mut sandbox = SandboxDb::new(empty_db);

        let addr = address!("1111111111111111111111111111111111111111");
        let value = sandbox.storage(addr, U256::from(1)).unwrap();
        assert_eq!(value, U256::ZERO);
    }

    #[test]
    fn test_sandbox_db_block_hash() {
        let empty_db = EmptyDB::default();
        let mut sandbox = SandboxDb::new(empty_db);

        // EmptyDB returns keccak256(number.to_string()) for block hashes
        let hash = sandbox.block_hash(100).unwrap();
        assert!(!hash.is_zero()); // Just verify it returns something non-zero
    }
}
