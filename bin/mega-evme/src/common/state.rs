//! State management for mega-evme with optional RPC forking support

use std::{collections::BTreeMap, path::PathBuf, str::FromStr};

use alloy_network::Network;
use alloy_primitives::{Address, BlockNumber, Bytes, B256, U256};
use alloy_provider::Provider;
use clap::Parser;
use op_alloy_network::Optimism;

use mega_evm::revm::{
    database::{AlloyDB, CacheDB, EmptyDB, WrapDatabaseAsync},
    primitives::HashMap,
    state::{Account, AccountInfo, Bytecode, EvmState, EvmStorageSlot, TransactionId},
    Database, DatabaseRef,
};
use tracing::{debug, info, trace};

use super::{EvmeError, Result, RpcCacheStore};

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

    /// JSON file with prestate (genesis) config. This overrides the state in the
    /// forked remote state (if applicable).
    #[arg(long = "prestate", visible_aliases = ["pre-state"])]
    pub prestate: Option<PathBuf>,

    /// History block hashes to serve `BLOCKHASH` opcode. This overrides the block hashes in the
    /// forked remote state (if applicable). Each entry should be in the format
    /// `block_number:block_hash` (can be repeated).
    #[arg(long = "block-hash", visible_aliases = ["blockhash", "block-hashes", "blockhashes"])]
    pub block_hashes: Vec<String>,

    /// Balance to allocate to the sender account.
    /// VALUE can be: plain number (wei), or number with suffix (ether, gwei, wei).
    /// Examples: `--sender.balance 1ether`, `--sender.balance 1000000000000000000`
    /// If not specified, sender balance is not set (fallback to `prestate` if specified,
    /// otherwise 0)
    #[arg(long = "sender.balance", visible_aliases = ["from.balance"])]
    pub sender_balance: Option<String>,

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
pub fn parse_ether_value(s: &str) -> Result<U256> {
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
            let mut prestate =
                EvmState::with_capacity_and_hasher(loaded_prestate.len(), Default::default());
            for (address, account_state) in loaded_prestate {
                // An entry marked as self-destructed describes an address the dumping run
                // erased from the state. Loading it as an account would resurrect balance,
                // nonce, code, and storage that the commit deleted, so the address is
                // treated as absent from the file.
                if account_state.is_selfdestructed() {
                    debug!(address = %address, "Skipping self-destructed account in prestate");
                    continue;
                }
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
                .insert(slot, EvmStorageSlot::new(value, TransactionId::ZERO));
        }

        // Set balance for the sender if specified (overrides prestate)
        if let Some(sender_balance_str) = &self.sender_balance {
            let sender_balance = parse_ether_value(sender_balance_str)?;
            info!(sender = %sender, sender_balance = %sender_balance, "Overriding sender balance");
            prestate.entry(*sender).or_default().info.set_balance(sender_balance);
        }

        // Apply faucet balances
        for (address, balance) in self.parse_faucet()? {
            info!(address = %address, balance = %balance, "Faucet: adding balance");
            prestate.entry(address).or_default().info.balance += balance;
        }

        Ok(prestate)
    }

    /// Build the initial execution state and the clean-exit RPC cache store.
    ///
    /// Fork mode (`self.fork == true`) builds a forked state at `self.fork_block` via
    /// `rpc_args.build_provider()` and returns the caller-owned [`RpcCacheStore`] for
    /// persist-on-exit. Non-fork mode builds an empty local state, ignores `rpc_args`,
    /// and returns a no-op store. Either way the call site persists unconditionally.
    pub async fn create_initial_state(
        &self,
        sender: &Address,
        rpc_args: &super::RpcArgs,
    ) -> Result<(EvmeState<Optimism, super::OpProvider>, RpcCacheStore)> {
        let prestate = self.load_prestate(sender)?;
        let block_hashes = self.parse_block_hashes()?;

        if self.fork {
            debug!("Creating forked state");
            if rpc_args.rpc_url.is_none() {
                return Err(EvmeError::InvalidInput("'--fork' requires '--rpc <URL>'".to_string()));
            }
            if rpc_args.capture_file.is_some() || rpc_args.replay_file.is_some() {
                return Err(EvmeError::InvalidInput(
                    "'--rpc.capture-file' and '--rpc.replay-file' are not supported with '--fork' \
                     in this version"
                        .to_string(),
                ));
            }
            let super::BuildProviderOutput { provider, cache_store, .. } =
                rpc_args.build_provider().await?;
            let state =
                EvmeState::new_forked(provider, self.fork_block, prestate, block_hashes).await?;
            Ok((state, cache_store))
        } else {
            debug!("Creating local state");
            Ok((EvmeState::new_empty(prestate, block_hashes), RpcCacheStore::noop()))
        }
    }
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
    /// Serializes [`EvmState`] as JSON string with deterministic key ordering.
    pub fn serialize_evm_state(&self, evm_state: &EvmState) -> Result<String> {
        trace!(evm_state = ?evm_state, "Serializing EVM state");
        let account_states: BTreeMap<_, _> = evm_state
            .iter()
            .filter_map(|(address, account)| {
                DumpedAccount::from_account(account.clone()).map(|dumped| (address, dumped))
            })
            .collect();
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
            std::fs::write(output_file, state_json).map_err(|e| {
                EvmeError::ExecutionError(format!("Failed to write state to file: {}", e))
            })?;
            println!("State dump written to: {}", output_file.display());
        } else {
            debug!("Printing dumped state to console");
            println!("{}", state_json);
        }

        Ok(())
    }
}

/// One account entry of a serialized state dump.
///
/// An account that survives the transaction is written as its full [`AccountState`].
/// An account destroyed by `SELFDESTRUCT` no longer exists once the state is committed —
/// its balance, nonce, code, and storage are all gone — so it is written as the bare
/// marker `{"selfdestructed": true}` instead of the account it used to be.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum DumpedAccount {
    /// An account present in the post-execution state.
    Live(AccountState),
    /// An address erased from the state by `SELFDESTRUCT` during the transaction.
    SelfDestructed {
        /// Always `true`, marking the address as erased by `SELFDESTRUCT`.
        selfdestructed: bool,
    },
}

impl DumpedAccount {
    /// Classifies a post-execution account for the state dump.
    ///
    /// `None` omits the address entirely: the run only ever observed it as
    /// nonexistent and it still holds no balance, nonce, or code, so there is no
    /// account to describe on either side of the commit — printing it as an
    /// existing empty account would let a round-tripped prestate answer
    /// `EXTCODEHASH` with the empty-code hash where the chain answers zero.
    /// The self-destruct check runs first: an account created and destroyed in
    /// one transaction also started as nonexistent, and it is reported as its
    /// marker, not omitted.
    pub fn from_account(account: Account) -> Option<Self> {
        if account.is_selfdestructed() {
            return Some(Self::SelfDestructed { selfdestructed: true });
        }
        if account.is_loaded_as_not_existing() && account.info.is_empty() {
            return None;
        }
        Some(Self::Live(AccountState::from_account(account)))
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
    /// Storage slots (sorted by key for deterministic output)
    pub storage: Option<BTreeMap<U256, U256>>,
    /// Marks an address that `SELFDESTRUCT` erased from the state.
    ///
    /// Never written for a live account, and read back as "this address does not exist":
    /// a prestate load skips such an entry instead of reconstructing an account from the
    /// remaining fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selfdestructed: Option<bool>,
}

impl AccountState {
    /// Whether this entry marks an address erased by `SELFDESTRUCT`.
    pub fn is_selfdestructed(&self) -> bool {
        self.selfdestructed.unwrap_or(false)
    }

    /// Creates a new [`AccountState`] from [`Account`].
    ///
    /// When `AccountInfo.code` is `Some`, `code_hash` is recomputed from the actual
    /// bytes (guards against stale hashes from direct `info.code` assignment).
    /// When `code` is `None` (lazy-loaded, e.g. forked accounts whose bytecode hasn't
    /// been fetched), the original `code_hash` is preserved so the account is not
    /// silently downgraded to an EOA.
    pub fn from_account(account: Account) -> Self {
        let (code, code_hash) = match &account.info.code {
            Some(bytecode) => {
                let bytes: Bytes = bytecode.original_byte_slice().to_vec().into();
                let hash = if bytes.is_empty() {
                    B256::from(alloy_primitives::KECCAK256_EMPTY)
                } else {
                    alloy_primitives::keccak256(&bytes)
                };
                (Some(bytes), hash)
            }
            // Code not materialized (lazy loading) — preserve original hash.
            None => (None, account.info.code_hash),
        };
        let storage: BTreeMap<U256, U256> =
            account.storage.into_iter().map(|(slot, value)| (slot, value.present_value)).collect();
        Self {
            balance: Some(account.info.balance),
            nonce: Some(account.info.nonce),
            code,
            code_hash: Some(code_hash),
            storage: Some(storage),
            selfdestructed: None,
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
            .map(|(slot, value)| (slot, EvmStorageSlot::new(value, TransactionId::ZERO)));
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

/// Normalizes an account fetched over RPC, mapping the all-zero answer to "does not exist".
///
/// JSON-RPC cannot express account non-existence: `eth_getBalance`, `eth_getTransactionCount`,
/// and `eth_getCode` all answer `0`/`0`/empty for an account that was never created, so the RPC
/// backend materializes it as an *existing* empty account. That flips every consumer of
/// existence, most visibly the EIP-7702 per-authorization refund: a brand-new authority is
/// judged "already in the trie" and the replay refunds 12,500 gas per authorization that the
/// chain did not.
///
/// Mapping all-zero back to `None` is safe because an existing-but-empty account cannot occur
/// on `MegaETH`: EIP-161 (Spurious Dragon) removes empty accounts on touch and forbids creating
/// them, every chain this tool replays activated it from genesis, and the genesis allocs carry
/// balance or code. The one shape this cannot distinguish — an account with zero
/// balance/nonce/code that still holds storage — also cannot exist post-EIP-161, since storage
/// is only reachable through code and contracts have a non-empty code hash or nonce.
fn normalize_rpc_account(account: Option<AccountInfo>) -> Option<AccountInfo> {
    account.filter(|info| !info.is_empty())
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
        self.prestate.entry(address).or_default().info.set_code(code);
    }

    /// Set the storage for an account.
    pub fn set_account_storage(&mut self, address: Address, storage: HashMap<U256, U256>) {
        self.prestate.entry(address).or_default().storage.extend(
            storage
                .into_iter()
                .map(|(slot, value)| (slot, EvmStorageSlot::new(value, TransactionId::ZERO))),
        );
    }

    /// Deploys system contracts based on the given spec.
    pub fn deploy_system_contracts(&mut self, spec: mega_evm::MegaSpecId) {
        use mega_evm::{
            flat_system_contract_specs, MegaSpecId, SEQUENCER_REGISTRY_ADDRESS,
            SEQUENCER_REGISTRY_CODE, SEQUENCER_REGISTRY_CODE_REX6,
        };

        // Flat predeploys (Oracle, high-precision timestamp Oracle, KeylessDeploy,
        // MegaAccessControl, MegaLimitControl) come from the canonical registry shared
        // with the block executor. mega-evme runs with a fixed spec, so activations are
        // resolved via a `FixedHardfork` at timestamp 0, and the bytecode is applied as a
        // raw state patch (no witness / storage seeding needed for local execution).
        for contract in flat_system_contract_specs(super::FixedHardfork::new(spec), 0) {
            self.set_account_code(contract.address, Bytecode::new_raw(contract.code));
        }

        // Rex5+: SequencerRegistry (v1.0.0 pre-Rex6, v2.0.0 from Rex6). Only the bytecode
        // is installed here — a local run has no chain-config sequencer/admin to seed (the
        // registry's storage is otherwise read from forked state).
        if spec.reaches(MegaSpecId::REX5) {
            let code = if spec.reaches(MegaSpecId::REX6) {
                SEQUENCER_REGISTRY_CODE_REX6
            } else {
                SEQUENCER_REGISTRY_CODE
            };
            self.set_account_code(SEQUENCER_REGISTRY_ADDRESS, Bytecode::new_raw(code));
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
                let account = normalize_rpc_account(db.basic(address).map_err(|e| {
                    EvmeError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
                })?);
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
                let account = normalize_rpc_account(db.basic_ref(address).map_err(|e| {
                    EvmeError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
                })?);
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
            trace!(code_hash = %code_hash, code = ?code, "Loaded code by hash from prestate");
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
                trace!(address = %address, index = %index, slot = %slot.present_value, "Loaded storage from prestate");
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const LIVE: Address = address!("0x00000000000000000000000000000000000000aa");
    const DESTROYED: Address = address!("0x00000000000000000000000000000000000000bb");

    /// A contract account holding `code` and a single storage slot.
    fn contract_account(balance: u64, nonce: u64, code: &[u8]) -> Account {
        let bytecode = Bytecode::new_raw(Bytes::from(code.to_vec()));
        let info = AccountInfo::new(U256::from(balance), nonce, bytecode.hash_slow(), bytecode);
        Account::from(info).with_storage(std::iter::once((
            U256::from(1),
            EvmStorageSlot::new(U256::from(0x42), TransactionId::ZERO),
        )))
    }

    fn dump_args() -> StateDumpArgs {
        StateDumpArgs { dump: true, dump_output_file: None }
    }

    /// A live account is serialized in full, byte for byte as before the
    /// self-destruct marker existed: every field present, none skipped.
    #[test]
    fn test_serialize_evm_state_live_account_is_written_in_full() {
        let mut state = EvmState::default();
        state.insert(LIVE, contract_account(7, 3, &[0x60, 0x00]));

        let json = dump_args().serialize_evm_state(&state).expect("serialize");

        assert_eq!(
            json,
            r#"{
  "0x00000000000000000000000000000000000000aa": {
    "balance": "0x7",
    "nonce": "0x3",
    "code": "0x6000",
    "codeHash": "0x07ad118d6cc8642c86c03827f276d8b791a65e5c99a3845faf186be720a1455d",
    "storage": {
      "0x1": "0x42"
    }
  }
}"#
        );
    }

    /// An address the run only observed as nonexistent — and that still holds
    /// no balance, nonce, or code — is omitted: no account exists on either
    /// side of the commit, so there is nothing to describe.
    #[test]
    fn test_serialize_evm_state_omits_a_never_existing_empty_account() {
        let mut state = EvmState::default();
        let mut ghost = Account::new_not_existing(TransactionId::ZERO);
        ghost.mark_touch();
        state.insert(DESTROYED, ghost);
        state.insert(LIVE, contract_account(7, 3, &[0x60, 0x00]));

        let json = dump_args().serialize_evm_state(&state).expect("serialize");

        assert!(
            !json.contains("0x00000000000000000000000000000000000000bb"),
            "ghost printed: {json}"
        );
        assert!(
            json.contains("0x00000000000000000000000000000000000000aa"),
            "live dropped: {json}"
        );
    }

    /// An account that started as nonexistent but gained substance during the
    /// transaction (a funded fresh address) exists after the commit and is
    /// written in full.
    #[test]
    fn test_serialize_evm_state_keeps_a_funded_fresh_account() {
        let mut state = EvmState::default();
        let mut funded = Account::new_not_existing(TransactionId::ZERO);
        funded.mark_touch();
        funded.info.balance = U256::from(5);
        state.insert(LIVE, funded);

        let json = dump_args().serialize_evm_state(&state).expect("serialize");

        assert!(
            json.contains("0x00000000000000000000000000000000000000aa") &&
                json.contains("\"balance\": \"0x5\""),
            "funded fresh account must be written in full: {json}"
        );
    }

    /// The self-destruct marker outranks the omission rule: an account created
    /// and destroyed in one transaction also started as nonexistent, and it is
    /// reported as its marker rather than silently dropped.
    #[test]
    fn test_serialize_evm_state_selfdestruct_marker_outranks_omission() {
        let mut state = EvmState::default();
        let mut account = Account::new_not_existing(TransactionId::ZERO);
        account.mark_touch();
        account.mark_selfdestruct();
        state.insert(DESTROYED, account);

        let json = dump_args().serialize_evm_state(&state).expect("serialize");

        assert!(
            json.contains("\"selfdestructed\": true"),
            "the marker must win over omission: {json}"
        );
    }

    /// An account destroyed by `SELFDESTRUCT` is reduced to the marker: none of
    /// the state the commit erases (code, storage, balance, nonce) is printed.
    #[test]
    fn test_serialize_evm_state_selfdestructed_account_is_marker_only() {
        let mut state = EvmState::default();
        state.insert(LIVE, contract_account(7, 3, &[0x60, 0x00]));
        state.insert(DESTROYED, contract_account(0, 1, &[0x33, 0xff]).with_selfdestruct_mark());

        let json = dump_args().serialize_evm_state(&state).expect("serialize");
        let parsed: BTreeMap<Address, serde_json::Value> =
            serde_json::from_str(&json).expect("valid JSON");

        assert_eq!(
            parsed[&DESTROYED],
            serde_json::json!({ "selfdestructed": true }),
            "destroyed account must carry the marker and nothing else",
        );
        let live = &parsed[&LIVE];
        assert_eq!(live["code"], "0x6000", "live account keeps its code");
        assert_eq!(live["storage"]["0x1"], "0x42", "live account keeps its storage");
        assert!(live.get("selfdestructed").is_none(), "live account carries no marker");
    }

    /// The marker round-trips: an entry a dump marked as destroyed loads as if
    /// the address were absent from the file, while its neighbours load normally.
    #[test]
    fn test_load_prestate_skips_selfdestructed_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prestate.json");
        std::fs::write(
            &path,
            format!(
                r#"{{
                  "{LIVE}": {{
                    "balance": "0x7",
                    "nonce": "0x3",
                    "code": "0x6000",
                    "storage": {{ "0x1": "0x42" }}
                  }},
                  "{DESTROYED}": {{ "selfdestructed": true }}
                }}"#
            ),
        )
        .expect("write prestate");

        let args = PreStateArgs::parse_from(["mega-evme", "--prestate", path.to_str().unwrap()]);
        let prestate = args.load_prestate(&Address::ZERO).expect("load prestate");

        assert!(!prestate.contains_key(&DESTROYED), "destroyed address must not be loaded");
        let live = prestate.get(&LIVE).expect("live account loaded");
        assert_eq!(live.info.nonce, 3);
        assert_eq!(
            live.storage.get(&U256::from(1)).map(|s| s.present_value),
            Some(U256::from(0x42))
        );
    }

    /// Only the marker's `true` value means "erased". An entry that spells the
    /// field out as `false` is an ordinary account and is loaded as written.
    #[test]
    fn test_load_prestate_keeps_entry_marked_not_selfdestructed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prestate.json");
        std::fs::write(
            &path,
            format!(r#"{{ "{LIVE}": {{ "nonce": "0x3", "selfdestructed": false }} }}"#),
        )
        .expect("write prestate");

        let args = PreStateArgs::parse_from(["mega-evme", "--prestate", path.to_str().unwrap()]);
        let prestate = args.load_prestate(&Address::ZERO).expect("load prestate");

        assert_eq!(prestate.get(&LIVE).expect("account loaded").info.nonce, 3);
    }
}
