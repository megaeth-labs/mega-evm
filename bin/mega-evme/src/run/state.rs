//! State management for mega-evme with optional RPC forking support

use alloy_network::Network;
use alloy_primitives::{Address, BlockNumber, U256};
use alloy_provider::Provider;
use mega_evm::revm::{
    database::{AlloyDB, CacheDB, EmptyDB, WrapDatabaseAsync},
    primitives::HashMap,
    state::{Account, AccountInfo, Bytecode},
    Database, DatabaseCommit, DatabaseRef,
};

use super::{Result, RunError};

/// Backend database type with generic provider and network
#[derive(Debug)]
enum EvmeBackend<N, P>
where
    N: Network,
    P: Provider<N> + std::fmt::Debug,
{
    /// Local state with no RPC backend
    Empty(EmptyDB),
    /// Forked state from RPC
    Forked(CacheDB<WrapDatabaseAsync<AlloyDB<N, P>>>),
}

/// State database that can be backed by either EmptyDB or AlloyDB (forked from RPC)
#[derive(Debug)]
pub struct EvmeState<N, P>
where
    N: Network,
    P: Provider<N> + std::fmt::Debug,
{
    /// The backend database
    backend: EvmeBackend<N, P>,
    /// Prestate overrides (accounts that override the database)
    prestate: HashMap<Address, AccountInfo>,
    /// Storage overrides
    storage: HashMap<Address, HashMap<U256, U256>>,
    /// Code hash to bytecode map (extracted from prestate accounts)
    code_map: HashMap<alloy_primitives::B256, Bytecode>,
}

impl<N, P> EvmeState<N, P>
where
    N: Network,
    P: Provider<N> + std::fmt::Debug,
{
    /// Create a new empty state with optional prestate overrides
    pub fn new_empty(
        prestate: HashMap<Address, AccountInfo>,
        storage: HashMap<Address, HashMap<U256, U256>>,
    ) -> Self {
        // Extract code hash → bytecode mappings from prestate
        let code_map: HashMap<_, _> = prestate
            .values()
            .filter_map(|info| info.code.clone().map(|code| (info.code_hash, code)))
            .collect();

        Self { backend: EvmeBackend::Empty(EmptyDB::default()), prestate, storage, code_map }
    }

    /// Insert an account override
    pub fn insert_account(&mut self, address: Address, info: AccountInfo) {
        // Add code to code_map if present
        if let Some(ref code) = info.code {
            self.code_map.insert(info.code_hash, code.clone());
        }
        self.prestate.insert(address, info);
    }

    /// Insert storage overrides for an account
    pub fn insert_storage(&mut self, address: Address, account_storage: HashMap<U256, U256>) {
        self.storage.insert(address, account_storage);
    }

    /// Insert an account with storage
    pub fn insert_account_with_storage(
        &mut self,
        address: Address,
        info: AccountInfo,
        account_storage: HashMap<U256, U256>,
    ) {
        self.insert_account(address, info);
        if !account_storage.is_empty() {
            self.insert_storage(address, account_storage);
        }
    }
}

// Impl block for methods that accept a generic provider
impl<N, P> EvmeState<N, P>
where
    N: Network,
    P: Provider<N> + Clone + std::fmt::Debug,
{
    /// Create a new forked state from a provider with optional prestate overrides
    pub async fn new_forked(
        provider: P,
        fork_block: Option<u64>,
        prestate: HashMap<Address, AccountInfo>,
        storage: HashMap<Address, HashMap<U256, U256>>,
    ) -> Result<Self> {
        // Determine block number
        let block_num = if let Some(block_num) = fork_block {
            BlockNumber::from(block_num)
        } else {
            // Fetch latest block number
            let latest_block = provider
                .get_block_number()
                .await
                .map_err(|e| RunError::RpcError(format!("Failed to fetch latest block: {}", e)))?;
            BlockNumber::from(latest_block)
        };

        // Create AlloyDB with the provider and block number
        let alloy_db = AlloyDB::new(provider, block_num.into());

        // Wrap the AlloyDB for synchronous access with the runtime
        let wrapped_db =
            WrapDatabaseAsync::new(alloy_db).expect("Failed to create wrapped database");

        // Wrap with CacheDB to enable mutable Database trait
        let db = CacheDB::new(wrapped_db);

        // Extract code hash → bytecode mappings from prestate
        let code_map: HashMap<_, _> = prestate
            .values()
            .filter_map(|info| info.code.clone().map(|code| (info.code_hash, code)))
            .collect();

        Ok(Self { backend: EvmeBackend::Forked(db), prestate, storage, code_map })
    }
}

impl<N, P> Database for EvmeState<N, P>
where
    N: Network,
    P: Provider<N> + std::fmt::Debug,
{
    type Error = RunError;

    fn basic(&mut self, address: Address) -> std::result::Result<Option<AccountInfo>, Self::Error> {
        // Check prestate overrides first
        if let Some(info) = self.prestate.get(&address) {
            return Ok(Some(info.clone()));
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => Ok(db.basic(address).unwrap()),
            EvmeBackend::Forked(db) => db.basic(address).map_err(|e| {
                RunError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
            }),
        }
    }

    fn code_by_hash(
        &mut self,
        code_hash: alloy_primitives::B256,
    ) -> std::result::Result<Bytecode, Self::Error> {
        // Check code_map first (for prestate accounts)
        if let Some(code) = self.code_map.get(&code_hash) {
            return Ok(code.clone());
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => Ok(db.code_by_hash(code_hash).unwrap()),
            EvmeBackend::Forked(db) => db.code_by_hash(code_hash).map_err(|e| {
                RunError::RpcError(format!("Failed to fetch code by hash {}: {:?}", code_hash, e))
            }),
        }
    }

    fn storage(&mut self, address: Address, index: U256) -> std::result::Result<U256, Self::Error> {
        // Check storage overrides first
        if let Some(account_storage) = self.storage.get(&address) {
            if let Some(value) = account_storage.get(&index) {
                return Ok(*value);
            }
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => Ok(db.storage(address, index).unwrap()),
            EvmeBackend::Forked(db) => db.storage(address, index).map_err(|e| {
                RunError::RpcError(format!(
                    "Failed to fetch storage for {} at slot {}: {:?}",
                    address, index, e
                ))
            }),
        }
    }

    fn block_hash(
        &mut self,
        number: u64,
    ) -> std::result::Result<alloy_primitives::B256, Self::Error> {
        match &mut self.backend {
            EvmeBackend::Empty(db) => Ok(db.block_hash(number).unwrap()),
            EvmeBackend::Forked(db) => db.block_hash(number).map_err(|e| {
                RunError::RpcError(format!(
                    "Failed to fetch block hash for block {}: {:?}",
                    number, e
                ))
            }),
        }
    }
}

impl<N, P> DatabaseRef for EvmeState<N, P>
where
    N: Network,
    P: Provider<N> + std::fmt::Debug,
{
    type Error = RunError;

    fn basic_ref(&self, address: Address) -> std::result::Result<Option<AccountInfo>, Self::Error> {
        // Check prestate overrides first
        if let Some(info) = self.prestate.get(&address) {
            return Ok(Some(info.clone()));
        }

        // Query backend database
        match &self.backend {
            EvmeBackend::Empty(db) => Ok(db.basic_ref(address).unwrap()),
            EvmeBackend::Forked(db) => db.basic_ref(address).map_err(|e| {
                RunError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
            }),
        }
    }

    fn code_by_hash_ref(
        &self,
        code_hash: alloy_primitives::B256,
    ) -> std::result::Result<Bytecode, Self::Error> {
        // Check code_map first (for prestate accounts)
        if let Some(code) = self.code_map.get(&code_hash) {
            return Ok(code.clone());
        }

        // Query backend database
        match &self.backend {
            EvmeBackend::Empty(db) => Ok(db.code_by_hash_ref(code_hash).unwrap()),
            EvmeBackend::Forked(db) => db.code_by_hash_ref(code_hash).map_err(|e| {
                RunError::RpcError(format!("Failed to fetch code by hash {}: {:?}", code_hash, e))
            }),
        }
    }

    fn storage_ref(&self, address: Address, index: U256) -> std::result::Result<U256, Self::Error> {
        // Check storage overrides first
        if let Some(account_storage) = self.storage.get(&address) {
            if let Some(value) = account_storage.get(&index) {
                return Ok(*value);
            }
        }

        // Query backend database
        match &self.backend {
            EvmeBackend::Empty(db) => Ok(db.storage_ref(address, index).unwrap()),
            EvmeBackend::Forked(db) => db.storage_ref(address, index).map_err(|e| {
                RunError::RpcError(format!(
                    "Failed to fetch storage for {} at slot {}: {:?}",
                    address, index, e
                ))
            }),
        }
    }

    fn block_hash_ref(
        &self,
        number: u64,
    ) -> std::result::Result<alloy_primitives::B256, Self::Error> {
        match &self.backend {
            EvmeBackend::Empty(db) => Ok(db.block_hash_ref(number).unwrap()),
            EvmeBackend::Forked(db) => db.block_hash_ref(number).map_err(|e| {
                RunError::RpcError(format!(
                    "Failed to fetch block hash for block {}: {:?}",
                    number, e
                ))
            }),
        }
    }
}

impl<N, P> DatabaseCommit for EvmeState<N, P>
where
    N: Network,
    P: Provider<N> + std::fmt::Debug,
{
    fn commit(&mut self, changes: HashMap<Address, Account>) {
        // Delegate to the backend database
        match &mut self.backend {
            EvmeBackend::Empty(_) => {
                // EmptyDB doesn't support commit, but we can update our prestate and storage
                for (address, account) in changes {
                    if account.is_touched() {
                        // Update account info in prestate
                        let info = account.info.clone();

                        // Add code to code_map if present
                        if let Some(ref code) = info.code {
                            self.code_map.insert(info.code_hash, code.clone());
                        }

                        self.prestate.insert(address, info);

                        // Update storage
                        let account_storage = self.storage.entry(address).or_default();
                        for (key, value) in account.storage {
                            if value.present_value != U256::ZERO {
                                account_storage.insert(key, value.present_value);
                            } else {
                                account_storage.remove(&key);
                            }
                        }
                    }
                }
            }
            EvmeBackend::Forked(db) => {
                // CacheDB supports commit
                db.commit(changes);
            }
        }
    }
}
