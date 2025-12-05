//! State management for mega-evme with optional RPC forking support

use std::path::PathBuf;

use alloy_network::Network;
use alloy_primitives::{map::DefaultHashBuilder, Address, BlockNumber, Bytes, B256, U256};
use alloy_provider::{DynProvider, Provider};
use clap::Parser;
use mega_evm::revm::{
    database::{AlloyDB, CacheDB, EmptyDB, WrapDatabaseAsync},
    primitives::HashMap,
    state::{Account, AccountInfo, Bytecode, EvmState, EvmStorageSlot},
    Database, DatabaseRef,
};
use tracing::debug;

use super::{EvmeError, Result};

/// Pre-execution state configuration arguments
#[derive(Parser, Debug, Clone)]
#[command(next_help_heading = "State Options")]
pub struct PreStateArgs {
    /// Fork state from a remote RPC endpoint.
    #[arg(long = "fork")]
    pub fork: bool,

    /// Block number of the state (post-block state) to fork from. If not specified, the latest
    /// block is used. Only used if `fork` is true.
    #[arg(long = "fork.block")]
    pub fork_block: Option<u64>,

    /// RPC URL to use for the fork. Only used if `fork` is true.
    #[arg(long = "fork.rpc", default_value = "http://localhost:8545", env = "RPC_URL")]
    pub fork_rpc: String,

    /// JSON file with prestate (genesis) config
    #[arg(long = "prestate")]
    pub prestate: Option<PathBuf>,

    /// Balance to allocate to the sender account
    /// If not specified, sender balance is not set (fallback to `prestate` if specified,
    /// otherwise 0)
    #[arg(long = "sender.balance", visible_aliases = ["from.balance"])]
    pub sender_balance: Option<U256>,
}

impl PreStateArgs {
    /// Load prestate as [`EvmState`] from file if provided
    pub fn load_prestate(&self, sender: &Address) -> Result<EvmState> {
        let mut prestate = if let Some(pre_state_path) = &self.prestate {
            let prestate_content = std::fs::read_to_string(pre_state_path)?;
            let loaded_prestate: HashMap<Address, AccountState> =
                serde_json::from_str(&prestate_content).map_err(|e| {
                    EvmeError::InvalidInput(format!("Failed to parse prestate JSON: {}", e))
                })?;
            let mut prestate = EvmState::with_capacity_and_hasher(
                loaded_prestate.len(),
                DefaultHashBuilder::default(),
            );
            for (address, account_state) in loaded_prestate {
                let account = account_state.into_account()?;
                prestate.insert(address, account);
            }
            prestate
        } else {
            HashMap::default()
        };

        // Set balance for the sender if specified (overrides prestate)
        if let Some(sender_balance) = &self.sender_balance {
            prestate.entry(*sender).or_default().info.set_balance(*sender_balance);
        }

        Ok(prestate)
    }

    /// Create a new provider for the network if forking is enabled
    pub fn create_provider<N>(&self) -> Result<Option<DynProvider<N>>>
    where
        N: alloy_network::Network + Sized,
    {
        if self.fork {
            let url = self.fork_rpc.parse().map_err(|e| {
                EvmeError::Other(format!("Invalid RPC URL '{}': {}", self.fork_rpc, e))
            })?;
            debug!("Forking from RPC {}", self.fork_rpc);
            let provider = alloy_provider::ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<N>()
                .connect_http(url);
            Ok(Some(DynProvider::new(provider)))
        } else {
            Ok(None)
        }
    }

    /// Creates the initial state for execution. This provides a Evm database based on the prestate
    /// and remote forked chain.
    pub async fn create_initial_state<N>(
        &self,
        sender: &Address,
    ) -> Result<EvmeState<N, DynProvider<N>>>
    where
        N: alloy_network::Network,
    {
        let provider = self.create_provider()?;

        // Load prestate
        let prestate = self.load_prestate(sender)?;

        // Create the appropriate state based on whether provider is provided
        if let Some(provider) = provider {
            EvmeState::new_forked(provider, self.fork_block, prestate).await
        } else {
            Ok(EvmeState::new_empty(prestate))
        }
    }
}

/// Dumps [`EvmState`] as JSON string.
pub fn convert_evm_state_to_json(evm_state: &EvmState) -> Result<String> {
    let account_states = evm_state
        .iter()
        .map(|(address, account)| (address, AccountState::from_account(account.clone())))
        .collect::<HashMap<_, _>>();
    let state_json = serde_json::to_string_pretty(&account_states)
        .map_err(|e| EvmeError::ExecutionError(format!("Failed to serialize state: {}", e)))?;
    Ok(state_json)
}

/// State dump configuration arguments
#[derive(Parser, Debug, Clone)]
#[command(next_help_heading = "State Dump Options")]
pub struct StateDumpArgs {
    /// Dumps the state after the run
    #[arg(long = "dump")]
    pub dump: bool,

    /// Output file for state dump (if not specified, prints to console)
    #[arg(long = "dump.output")]
    pub dump_output_file: Option<PathBuf>,
}

impl StateDumpArgs {
    /// Serializes [`EvmState`] as JSON string.
    pub fn serialize_evm_state(&self, evm_state: &EvmState) -> Result<String> {
        let account_states = evm_state
            .iter()
            .map(|(address, account)| (address, AccountState::from_account(account.clone())))
            .collect::<HashMap<_, _>>();
        let state_json = serde_json::to_string_pretty(&account_states)
            .map_err(|e| EvmeError::ExecutionError(format!("Failed to serialize state: {}", e)))?;
        Ok(state_json)
    }

    /// Dumps [`EvmState`] as JSON string to file or console.
    pub fn dump_evm_state(&self, evm_state: &EvmState) -> Result<()> {
        let state_json = self.serialize_evm_state(evm_state)?;

        // Output to file or console
        if let Some(ref output_file) = self.dump_output_file {
            // Write state to file
            std::fs::write(output_file, state_json).map_err(|e| {
                EvmeError::ExecutionError(format!("Failed to write state to file: {}", e))
            })?;
            eprintln!();
            eprintln!("State dump written to: {}", output_file.display());
        } else {
            // Print state to console
            eprintln!();
            eprintln!("=== State Dump ===");
            eprintln!("{}", state_json);
        }

        Ok(())
    }
}

/// Account state information
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    /// Account balance
    /// U256 from ruint already uses quantity format (0x-prefixed hex without leading zeros)
    pub balance: U256,
    /// Account nonce (uses `alloy_serde::quantity` for standard Ethereum format)
    #[serde(with = "alloy_serde::quantity")]
    pub nonce: u64,
    /// Account code (hex string with 0x prefix)
    pub code: Bytes,
    /// Code hash
    /// B256 already uses hex format with 0x prefix (always 32 bytes)
    pub code_hash: B256,
    /// Storage slots (uses quantity format for keys and values)
    pub storage: HashMap<U256, U256>,
}

impl AccountState {
    /// Creates a new [`AccountState`] from [`Account`].
    pub fn from_account(account: Account) -> Self {
        let AccountInfo { balance, nonce, code_hash, code } = account.info;
        let code = code.map(|c| c.bytecode().to_vec()).unwrap_or_default().into();
        let storage =
            account.storage.into_iter().map(|(slot, value)| (slot, value.present_value)).collect();
        Self { balance, nonce, code, code_hash, storage }
    }

    /// Converts into [`Account`].
    pub fn into_account(self) -> Result<Account> {
        let bytecode = if self.code.is_empty() {
            Bytecode::default()
        } else {
            Bytecode::new_raw_checked(self.code).map_err(EvmeError::InvalidBytecode)?
        };
        let computed_hash = bytecode.hash_slow();
        if computed_hash != self.code_hash {
            return Err(EvmeError::CodeHashMismatch {
                expected: self.code_hash,
                computed: computed_hash,
            });
        }

        let info = AccountInfo::new(self.balance, self.nonce, self.code_hash, bytecode);
        let storage =
            self.storage.into_iter().map(|(slot, value)| (slot, EvmStorageSlot::new(value, 0)));
        Ok(Account::from(info).with_storage(storage))
    }
}

/// Backend database type with generic provider and network
#[derive(Debug)]
enum EvmeBackend<N, P>
where
    N: Network,
    P: Provider<N>,
{
    /// Local state with no RPC backend
    Empty(EmptyDB),
    /// Forked state from RPC
    Forked(Box<CacheDB<WrapDatabaseAsync<AlloyDB<N, P>>>>),
}

/// State database that can be backed by either [`EmptyDB`] or [`AlloyDB`] (forked from RPC)
#[derive(Debug)]
pub struct EvmeState<N, P>
where
    N: Network,
    P: Provider<N>,
{
    /// The backend database
    backend: EvmeBackend<N, P>,
    /// Prestate overrides (accounts that override the database)
    prestate: EvmState,
    /// Code hash to bytecode map (extracted from prestate accounts)
    code_map: HashMap<alloy_primitives::B256, Bytecode>,
}

impl<N, P> EvmeState<N, P>
where
    N: Network,
    P: Provider<N>,
{
    /// Creates a new empty state with optional prestate overrides
    pub fn new_empty(prestate: EvmState) -> Self {
        // Extract code hash → bytecode mappings from prestate
        let code_map: HashMap<_, _> = prestate
            .values()
            .filter_map(|account| {
                account.info.code.clone().map(|code| (account.info.code_hash, code))
            })
            .collect();

        Self { backend: EvmeBackend::Empty(EmptyDB::default()), prestate, code_map }
    }

    /// Inserts an account override
    /// This will override the existing account if it exists.
    pub fn insert_account(&mut self, address: Address, account: Account) {
        // Add code to code_map if present
        if let Some(ref code) = account.info.code {
            self.code_map.insert(account.info.code_hash, code.clone());
        }
        self.prestate.insert(address, account);
    }

    /// Inserts storage overrides for an account
    pub fn insert_storage(&mut self, address: Address, storage: HashMap<U256, EvmStorageSlot>) {
        self.prestate.entry(address).or_default().storage.extend(storage);
    }

    /// Inserts an account with storage.
    /// This will override the existing account if it exists.
    pub fn insert_account_with_storage(
        &mut self,
        address: Address,
        info: AccountInfo,
        storage: HashMap<U256, EvmStorageSlot>,
    ) {
        // Add code to code_map if present
        if let Some(ref code) = info.code {
            self.code_map.insert(info.code_hash, code.clone());
        }
        let account = Account::from(info).with_storage(storage.into_iter());
        self.prestate.insert(address, account);
    }

    /// Set the balance for an account.
    pub fn set_account_balance(&mut self, address: Address, balance: U256) {
        self.prestate.entry(address).or_default().info.balance = balance;
    }

    /// Set the nonce for an account.
    pub fn set_account_nonce(&mut self, address: Address, nonce: u64) {
        self.prestate.entry(address).or_default().info.nonce = nonce;
    }

    /// Set the code for an account.
    pub fn set_account_code(&mut self, address: Address, code: Bytecode) {
        self.code_map.insert(code.hash_slow(), code.clone());
        self.prestate.entry(address).or_default().info.code = Some(code);
    }

    /// Set the storage for an account.
    pub fn set_account_storage(&mut self, address: Address, storage: HashMap<U256, U256>) {
        self.prestate
            .entry(address)
            .or_default()
            .storage
            .extend(storage.into_iter().map(|(slot, value)| (slot, EvmStorageSlot::new(value, 0))));
    }
}

// Impl block for methods that accept a generic provider
impl<N, P> EvmeState<N, P>
where
    N: Network,
    P: Provider<N>,
{
    /// Create a new forked state from a provider with optional prestate overrides
    pub async fn new_forked(
        provider: P,
        fork_block: Option<u64>,
        prestate: EvmState,
    ) -> Result<Self> {
        // Determine block number
        let block_num = if let Some(block_num) = fork_block {
            BlockNumber::from(block_num)
        } else {
            // Fetch latest block number
            let latest_block = provider
                .get_block_number()
                .await
                .map_err(|e| EvmeError::RpcError(format!("Failed to fetch latest block: {}", e)))?;
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
            .filter_map(|account| {
                account.info.code.clone().map(|code| (account.info.code_hash, code))
            })
            .collect();

        Ok(Self { backend: EvmeBackend::Forked(Box::new(db)), prestate, code_map })
    }
}

impl<N, P> Database for EvmeState<N, P>
where
    N: Network,
    P: Provider<N> + std::fmt::Debug,
{
    type Error = EvmeError;

    fn basic(&mut self, address: Address) -> std::result::Result<Option<AccountInfo>, Self::Error> {
        // Check prestate overrides first
        if let Some(account) = self.prestate.get(&address) {
            return Ok(Some(account.info.clone()));
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => Ok(db.basic(address).unwrap()),
            EvmeBackend::Forked(db) => db.basic(address).map_err(|e| {
                EvmeError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
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
                EvmeError::RpcError(format!("Failed to fetch code by hash {}: {:?}", code_hash, e))
            }),
        }
    }

    fn storage(&mut self, address: Address, index: U256) -> std::result::Result<U256, Self::Error> {
        // Check storage overrides first
        if let Some(account) = self.prestate.get(&address) {
            if let Some(slot) = account.storage.get(&index) {
                return Ok(slot.present_value);
            }
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => Ok(db.storage(address, index).unwrap()),
            EvmeBackend::Forked(db) => db.storage(address, index).map_err(|e| {
                EvmeError::RpcError(format!(
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
                EvmeError::RpcError(format!(
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
    type Error = EvmeError;

    fn basic_ref(&self, address: Address) -> std::result::Result<Option<AccountInfo>, Self::Error> {
        // Check prestate overrides first
        if let Some(account) = self.prestate.get(&address) {
            return Ok(Some(account.info.clone()));
        }

        // Query backend database
        match &self.backend {
            EvmeBackend::Empty(db) => Ok(db.basic_ref(address).unwrap()),
            EvmeBackend::Forked(db) => db.basic_ref(address).map_err(|e| {
                EvmeError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
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
                EvmeError::RpcError(format!("Failed to fetch code by hash {}: {:?}", code_hash, e))
            }),
        }
    }

    fn storage_ref(&self, address: Address, index: U256) -> std::result::Result<U256, Self::Error> {
        // Check storage overrides first
        if let Some(account) = self.prestate.get(&address) {
            if let Some(slot) = account.storage.get(&index) {
                return Ok(slot.present_value);
            }
        }

        // Query backend database
        match &self.backend {
            EvmeBackend::Empty(db) => Ok(db.storage_ref(address, index).unwrap()),
            EvmeBackend::Forked(db) => db.storage_ref(address, index).map_err(|e| {
                EvmeError::RpcError(format!(
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
                EvmeError::RpcError(format!(
                    "Failed to fetch block hash for block {}: {:?}",
                    number, e
                ))
            }),
        }
    }
}
