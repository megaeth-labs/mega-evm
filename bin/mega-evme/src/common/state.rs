//! State management for mega-evme with optional RPC forking support

use std::{path::PathBuf, str::FromStr};

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
use tracing::{debug, info, trace};

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

    /// JSON file with prestate (genesis) config. This overrides the state in the
    /// forked remote state (if applicable).
    #[arg(long = "prestate", visible_aliases = ["pre-state"])]
    pub prestate: Option<PathBuf>,

    /// History block hashes to serve `BLOCKHASH` opcode. This overrides the block hashes in the
    /// forked remote state (if applicable). Each entry should be in the format
    /// `block_number:block_hash` (can be repeated).
    #[arg(long = "block-hash", visible_aliases = ["blockhash", "block-hashes", "blockhashes"])]
    pub block_hashes: Vec<String>,

    /// Balance to allocate to the sender account
    /// If not specified, sender balance is not set (fallback to `prestate` if specified,
    /// otherwise 0)
    #[arg(long = "sender.balance", visible_aliases = ["from.balance"])]
    pub sender_balance: Option<U256>,

    /// Add ether to specified addresses. Each entry format: `ADDRESS+=VALUE`
    /// VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei).
    /// Examples: `--faucet 0x1234+=100ether`, `--faucet 0x5678+=1000000gwei`
    /// Can be repeated for multiple addresses.
    #[arg(long = "faucet")]
    pub faucet: Vec<String>,

    /// Override balance for specified addresses. Each entry format: `ADDRESS=VALUE`
    /// VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei).
    /// Examples: `--balance 0x1234=100ether`
    #[arg(long = "balance")]
    pub balance: Vec<String>,

    /// Override storage slots. Each entry format: `ADDRESS:SLOT=VALUE`
    /// SLOT and VALUE are U256 (hex or decimal).
    /// Examples: `--storage 0x1234:0x0=0x1`
    #[arg(long = "storage")]
    pub storage: Vec<String>,
}

/// Parse ether value string into wei (U256).
/// Supports: plain number (wei), or number with suffix (ether, gwei, wei, etc).
/// Examples: "1000000000000000000", "1ether", "100gwei", "1000wei"
fn parse_ether_value(s: &str) -> Result<U256> {
    use alloy_primitives::utils::parse_units;

    let s = s.trim();

    // Find where digits/decimal end and unit begins
    let split_pos = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());

    let (num_str, unit) = s.split_at(split_pos);
    let unit = if unit.is_empty() { "wei" } else { unit };

    let parsed = parse_units(num_str, unit)
        .map_err(|e| EvmeError::InvalidInput(format!("Invalid ether value '{}': {}", s, e)))?;

    Ok(parsed.into())
}

impl PreStateArgs {
    /// Parse block hashes from CLI arguments.
    ///
    /// Each entry should be in the format `block_number:block_hash`.
    pub fn parse_block_hashes(&self) -> Result<HashMap<u64, B256>> {
        debug!("Parsing block hashes");
        let mut map = HashMap::default();
        for entry in &self.block_hashes {
            let (num_str, hash_str) = entry.split_once(':').ok_or_else(|| {
                EvmeError::InvalidInput(format!(
                    "Invalid block hash entry '{}': expected format 'block_number:block_hash'",
                    entry
                ))
            })?;
            let block_num: u64 = num_str.trim().parse().map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Invalid block number '{}' in entry '{}': {}",
                    num_str, entry, e
                ))
            })?;
            let block_hash = B256::from_str(hash_str.trim()).map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Invalid block hash '{}' in entry '{}': {}",
                    hash_str, entry, e
                ))
            })?;
            map.insert(block_num, block_hash);
        }
        trace!(block_hashes = ?map, "Block hashes parsed");
        Ok(map)
    }

    /// Parse faucet entries from CLI arguments.
    ///
    /// Each entry should be in the format `ADDRESS+=VALUE`.
    /// VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei).
    pub fn parse_faucet(&self) -> Result<Vec<(Address, U256)>> {
        let mut entries = Vec::new();
        for entry in &self.faucet {
            let (addr_str, value_str) = entry.split_once("+=").ok_or_else(|| {
                EvmeError::InvalidInput(format!(
                    "Invalid faucet entry '{}': expected format 'ADDRESS+=VALUE'",
                    entry
                ))
            })?;
            let address = Address::from_str(addr_str.trim()).map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Invalid address '{}' in faucet entry '{}': {}",
                    addr_str, entry, e
                ))
            })?;
            let wei = parse_ether_value(value_str)?;
            entries.push((address, wei));
        }
        Ok(entries)
    }

    /// Parse balance override entries from CLI arguments.
    ///
    /// Each entry should be in the format `ADDRESS=VALUE`.
    /// VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei).
    pub fn parse_balance(&self) -> Result<Vec<(Address, U256)>> {
        let mut entries = Vec::new();
        for entry in &self.balance {
            let (addr_str, value_str) = entry.split_once('=').ok_or_else(|| {
                EvmeError::InvalidInput(format!(
                    "Invalid balance entry '{}': expected format 'ADDRESS=VALUE'",
                    entry
                ))
            })?;
            let address = Address::from_str(addr_str.trim()).map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Invalid address '{}' in balance entry '{}': {}",
                    addr_str, entry, e
                ))
            })?;
            let wei = parse_ether_value(value_str)?;
            entries.push((address, wei));
        }
        Ok(entries)
    }

    /// Parse storage override entries from CLI arguments.
    ///
    /// Each entry should be in the format `ADDRESS:SLOT=VALUE`.
    /// SLOT and VALUE are U256 (hex or decimal).
    pub fn parse_storage(&self) -> Result<Vec<(Address, U256, U256)>> {
        let mut entries = Vec::new();
        for entry in &self.storage {
            let (addr_str, rest) = entry.split_once(':').ok_or_else(|| {
                EvmeError::InvalidInput(format!(
                    "Invalid storage entry '{}': expected format 'ADDRESS:SLOT=VALUE'",
                    entry
                ))
            })?;
            let (slot_str, value_str) = rest.split_once('=').ok_or_else(|| {
                EvmeError::InvalidInput(format!(
                    "Invalid storage entry '{}': expected format 'ADDRESS:SLOT=VALUE'",
                    entry
                ))
            })?;
            let address = Address::from_str(addr_str.trim()).map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Invalid address '{}' in storage entry '{}': {}",
                    addr_str, entry, e
                ))
            })?;
            let slot = U256::from_str(slot_str.trim()).map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Invalid slot '{}' in storage entry '{}': {}",
                    slot_str, entry, e
                ))
            })?;
            let value = U256::from_str(value_str.trim()).map_err(|e| {
                EvmeError::InvalidInput(format!(
                    "Invalid value '{}' in storage entry '{}': {}",
                    value_str, entry, e
                ))
            })?;
            entries.push((address, slot, value));
        }
        Ok(entries)
    }

    /// Load prestate as [`EvmState`] from file if provided
    pub fn load_prestate(&self, sender: &Address) -> Result<EvmState> {
        let mut prestate = if let Some(pre_state_path) = &self.prestate {
            info!(prestate_path = ?pre_state_path, "Loading prestate from file");
            let prestate_content = std::fs::read_to_string(pre_state_path)?;
            let loaded_prestate: HashMap<Address, AccountState> =
                serde_json::from_str(&prestate_content).map_err(|e| {
                    EvmeError::InvalidInput(format!("Failed to parse prestate JSON: {}", e))
                })?;
            trace!(loaded_prestate = ?loaded_prestate, "Prestate loaded from file");
            let mut prestate = EvmState::with_capacity_and_hasher(
                loaded_prestate.len(),
                DefaultHashBuilder::default(),
            );
            for (address, account_state) in loaded_prestate {
                let account = account_state.into_account()?;
                prestate.insert(address, account);
            }
            trace!(prestate = ?prestate, "Prestate loaded");
            prestate
        } else {
            debug!("No prestate file provided");
            HashMap::default()
        };

        // Apply balance overrides
        for (address, balance) in self.parse_balance()? {
            info!(address = %address, balance = %balance, "Overriding balance");
            prestate.entry(address).or_default().info.balance = balance;
        }

        // Apply storage overrides
        for (address, slot, value) in self.parse_storage()? {
            info!(address = %address, slot = %slot, value = %value, "Overriding storage");
            prestate
                .entry(address)
                .or_default()
                .storage
                .insert(slot, EvmStorageSlot::new(value, 0));
        }

        // Set balance for the sender if specified (overrides prestate)
        if let Some(sender_balance) = &self.sender_balance {
            info!(sender = %sender, sender_balance = %sender_balance, "Overriding sender balance");
            prestate.entry(*sender).or_default().info.set_balance(*sender_balance);
        }

        // Apply faucet balances
        for (address, balance) in self.parse_faucet()? {
            info!(address = %address, balance = %balance, "Faucet: adding balance");
            prestate.entry(address).or_default().info.balance += balance;
        }

        Ok(prestate)
    }

    /// Create a new provider for the network if forking is enabled
    pub fn create_provider<N>(&self) -> Result<Option<DynProvider<N>>>
    where
        N: alloy_network::Network + Sized,
    {
        if self.fork {
            debug!(rpc_url = %self.fork_rpc, "Forking state from RPC");
            let url = self.fork_rpc.parse().map_err(|e| {
                EvmeError::Other(format!("Invalid RPC URL '{}': {}", self.fork_rpc, e))
            })?;
            let provider = alloy_provider::ProviderBuilder::new()
                .disable_recommended_fillers()
                .network::<N>()
                .connect_http(url);
            Ok(Some(DynProvider::new(provider)))
        } else {
            debug!("No forking state specified");
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

        // Parse block hashes
        let block_hashes = self.parse_block_hashes()?;

        // Create the appropriate state based on whether provider is provided
        if let Some(provider) = provider {
            debug!("Creating forked state");
            EvmeState::new_forked(provider, self.fork_block, prestate, block_hashes).await
        } else {
            debug!("Creating local state");
            Ok(EvmeState::new_empty(prestate, block_hashes))
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
        trace!(evm_state = ?evm_state, "Serializing EVM state");
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
        debug!("Dumping EVM state");
        let state_json = self.serialize_evm_state(evm_state)?;

        // Output to file or console
        println!();
        println!("=== State Dump ===");
        if let Some(ref output_file) = self.dump_output_file {
            debug!(output_file = ?output_file, "Writing dumped state to file");
            // Write state to file
            std::fs::write(output_file, state_json).map_err(|e| {
                EvmeError::ExecutionError(format!("Failed to write state to file: {}", e))
            })?;
            println!("State dump written to: {}", output_file.display());
        } else {
            debug!("Printing dumped state to console");
            // Print state to console
            println!("{}", state_json);
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
    pub balance: Option<U256>,
    /// Account nonce (uses `alloy_serde::quantity` for standard Ethereum format)
    #[serde(default, with = "alloy_serde::quantity::opt")]
    pub nonce: Option<u64>,
    /// Account code (hex string with 0x prefix)
    pub code: Option<Bytes>,
    /// Code hash
    /// B256 already uses hex format with 0x prefix (always 32 bytes)
    pub code_hash: Option<B256>,
    /// Storage slots (uses quantity format for keys and values)
    pub storage: Option<HashMap<U256, U256>>,
}

impl AccountState {
    /// Creates a new [`AccountState`] from [`Account`].
    pub fn from_account(account: Account) -> Self {
        let AccountInfo { balance, nonce, code_hash, code } = account.info;
        let code = code.map(|c| c.bytecode().to_vec()).unwrap_or_default().into();
        let storage =
            account.storage.into_iter().map(|(slot, value)| (slot, value.present_value)).collect();
        Self {
            balance: Some(balance),
            nonce: Some(nonce),
            code: Some(code),
            code_hash: Some(code_hash),
            storage: Some(storage),
        }
    }

    /// Converts into [`Account`].
    pub fn into_account(self) -> Result<Account> {
        let code = self.code.unwrap_or_default();
        let bytecode = if code.is_empty() {
            Bytecode::default()
        } else {
            Bytecode::new_raw_checked(code).map_err(EvmeError::InvalidBytecode)?
        };
        let computed_hash = bytecode.hash_slow();
        if let Some(code_hash) = self.code_hash {
            if computed_hash != code_hash {
                return Err(EvmeError::CodeHashMismatch {
                    expected: code_hash,
                    computed: computed_hash,
                });
            }
        }

        let info = AccountInfo::new(
            self.balance.unwrap_or_default(),
            self.nonce.unwrap_or_default(),
            computed_hash,
            bytecode,
        );
        let storage = self
            .storage
            .unwrap_or_default()
            .into_iter()
            .map(|(slot, value)| (slot, EvmStorageSlot::new(value, 0)));
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
    /// Block hash overrides (block number -> block hash)
    block_hashes: HashMap<u64, B256>,
}

impl<N, P> EvmeState<N, P>
where
    N: Network,
    P: Provider<N>,
{
    /// Creates a new empty state with optional prestate overrides and block hash overrides
    pub fn new_empty(prestate: EvmState, block_hashes: HashMap<u64, B256>) -> Self {
        // Extract code hash → bytecode mappings from prestate
        let code_map: HashMap<_, _> = prestate
            .values()
            .filter_map(|account| {
                account.info.code.clone().map(|code| (account.info.code_hash, code))
            })
            .collect();

        Self { backend: EvmeBackend::Empty(EmptyDB::default()), prestate, code_map, block_hashes }
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

    /// Deploys system contracts based on the given spec.
    pub fn deploy_system_contracts(&mut self, spec: mega_evm::MegaSpecId) {
        use mega_evm::{
            MegaSpecId, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
            HIGH_PRECISION_TIMESTAMP_ORACLE_CODE, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE,
            ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, ORACLE_CONTRACT_CODE_REX2,
        };

        // MiniRex+: Oracle Contract (v1.0.0 or v1.1.0 based on Rex2)
        if spec >= MegaSpecId::MINI_REX {
            let code = if spec >= MegaSpecId::REX2 {
                ORACLE_CONTRACT_CODE_REX2
            } else {
                ORACLE_CONTRACT_CODE
            };
            self.set_account_code(ORACLE_CONTRACT_ADDRESS, Bytecode::new_raw(code));
        }

        // MiniRex+: High Precision Timestamp Oracle
        if spec >= MegaSpecId::MINI_REX {
            self.set_account_code(
                HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
                Bytecode::new_raw(HIGH_PRECISION_TIMESTAMP_ORACLE_CODE),
            );
        }

        // Rex2+: Keyless Deploy Contract
        if spec >= MegaSpecId::REX2 {
            self.set_account_code(KEYLESS_DEPLOY_ADDRESS, Bytecode::new_raw(KEYLESS_DEPLOY_CODE));
        }
    }
}

// Impl block for methods that accept a generic provider
impl<N, P> EvmeState<N, P>
where
    N: Network,
    P: Provider<N>,
{
    /// Create a new forked state from a provider with optional prestate overrides and block hash
    /// overrides
    pub async fn new_forked(
        provider: P,
        fork_block: Option<u64>,
        prestate: EvmState,
        block_hashes: HashMap<u64, B256>,
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

        Ok(Self { backend: EvmeBackend::Forked(Box::new(db)), prestate, code_map, block_hashes })
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
            trace!(address = %address, account = ?account, "Loaded account basic from prestate");
            return Ok(Some(account.info.clone()));
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => {
                let account = db.basic(address).unwrap();
                trace!(address = %address, account = ?account, "Loaded account basic from empty state");
                Ok(account)
            }
            EvmeBackend::Forked(db) => {
                let account = db.basic(address).map_err(|e| {
                    EvmeError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
                })?;
                trace!(address = %address, account = ?account, "Loaded account basic from forked state");
                Ok(account)
            }
        }
    }

    fn code_by_hash(
        &mut self,
        code_hash: alloy_primitives::B256,
    ) -> std::result::Result<Bytecode, Self::Error> {
        // Check code_map first (for prestate accounts)
        if let Some(code) = self.code_map.get(&code_hash) {
            trace!(code_hash = %code_hash, code = ?code, "Loaded code by hash from prestate");
            return Ok(code.clone());
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => {
                let code = db.code_by_hash(code_hash).unwrap();
                trace!(code_hash = %code_hash, code = ?code, "Loaded code by hash from empty state");
                Ok(code)
            }
            EvmeBackend::Forked(db) => {
                let code = db.code_by_hash(code_hash).map_err(|e| {
                    EvmeError::RpcError(format!(
                        "Failed to fetch code by hash {}: {:?}",
                        code_hash, e
                    ))
                })?;
                trace!(code_hash = %code_hash, code = ?code, "Loaded code by hash from forked state");
                Ok(code)
            }
        }
    }

    fn storage(&mut self, address: Address, index: U256) -> std::result::Result<U256, Self::Error> {
        // Check storage overrides first
        if let Some(account) = self.prestate.get(&address) {
            if let Some(slot) = account.storage.get(&index) {
                trace!(address = %address, index = %index, slot = %slot.present_value, "Loaded storage from prestate");
                return Ok(slot.present_value);
            }
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => {
                let storage = db.storage(address, index).unwrap();
                trace!(address = %address, index = %index, storage = %storage, "Loaded storage from empty state");
                Ok(storage)
            }
            EvmeBackend::Forked(db) => {
                let storage = db.storage(address, index).map_err(|e| {
                    EvmeError::RpcError(format!(
                        "Failed to fetch storage for {} at slot {}: {:?}",
                        address, index, e
                    ))
                })?;
                trace!(address = %address, index = %index, storage = %storage, "Loaded storage from forked state");
                Ok(storage)
            }
        }
    }

    fn block_hash(
        &mut self,
        number: u64,
    ) -> std::result::Result<alloy_primitives::B256, Self::Error> {
        // Check block hash overrides first
        if let Some(hash) = self.block_hashes.get(&number) {
            trace!(number = %number, hash = %hash, "Loaded block hash from provided overrides");
            return Ok(*hash);
        }

        // Query backend database
        match &mut self.backend {
            EvmeBackend::Empty(db) => {
                let hash = db.block_hash(number).unwrap();
                trace!(number = %number, hash = %hash, "Loaded block hash from empty state");
                Ok(hash)
            }
            EvmeBackend::Forked(db) => {
                let hash = db.block_hash(number).map_err(|e| {
                    EvmeError::RpcError(format!(
                        "Failed to fetch block hash for block {}: {:?}",
                        number, e
                    ))
                })?;
                trace!(number = %number, hash = %hash, "Loaded block hash from forked state");
                Ok(hash)
            }
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
            trace!(address = %address, account = ?account, "Loaded account basic from prestate");
            return Ok(Some(account.info.clone()));
        }

        // Query backend database
        match &self.backend {
            EvmeBackend::Empty(db) => {
                let account = db.basic_ref(address).unwrap();
                trace!(address = %address, account = ?account, "Loaded account basic from empty state");
                Ok(account)
            }
            EvmeBackend::Forked(db) => {
                let account = db.basic_ref(address).map_err(|e| {
                    EvmeError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
                })?;
                trace!(address = %address, account = ?account, "Loaded account basic from forked state");
                Ok(account)
            }
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
            EvmeBackend::Empty(db) => {
                let code = db.code_by_hash_ref(code_hash).unwrap();
                trace!(code_hash = %code_hash, code = ?code, "Loaded code by hash from empty state");
                Ok(code)
            }
            EvmeBackend::Forked(db) => {
                let code = db.code_by_hash_ref(code_hash).map_err(|e| {
                    EvmeError::RpcError(format!(
                        "Failed to fetch code by hash {}: {:?}",
                        code_hash, e
                    ))
                })?;
                trace!(code_hash = %code_hash, code = ?code, "Loaded code by hash from forked state");
                Ok(code)
            }
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
            EvmeBackend::Empty(db) => {
                let storage = db.storage_ref(address, index).unwrap();
                trace!(address = %address, index = %index, storage = %storage, "Loaded storage from empty state");
                Ok(storage)
            }
            EvmeBackend::Forked(db) => {
                let storage = db.storage_ref(address, index).map_err(|e| {
                    EvmeError::RpcError(format!(
                        "Failed to fetch storage for {} at slot {}: {:?}",
                        address, index, e
                    ))
                })?;
                trace!(address = %address, index = %index, storage = %storage, "Loaded storage from forked state");
                Ok(storage)
            }
        }
    }

    fn block_hash_ref(
        &self,
        number: u64,
    ) -> std::result::Result<alloy_primitives::B256, Self::Error> {
        // Check block hash overrides first
        if let Some(hash) = self.block_hashes.get(&number) {
            trace!(number = %number, hash = %hash, "Loaded block hash from provided overrides");
            return Ok(*hash);
        }

        // Query backend database
        match &self.backend {
            EvmeBackend::Empty(db) => {
                let hash = db.block_hash_ref(number).unwrap();
                trace!(number = %number, hash = %hash, "Loaded block hash from empty state");
                Ok(hash)
            }
            EvmeBackend::Forked(db) => {
                let hash = db.block_hash_ref(number).map_err(|e| {
                    EvmeError::RpcError(format!(
                        "Failed to fetch block hash for block {}: {:?}",
                        number, e
                    ))
                })?;
                trace!(number = %number, hash = %hash, "Loaded block hash from forked state");
                Ok(hash)
            }
        }
    }
}
