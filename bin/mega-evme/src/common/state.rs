//! State management for mega-evme with optional RPC forking support

use std::{collections::BTreeMap, path::PathBuf, str::FromStr};

use alloy_network::Network;
use alloy_primitives::{Address, BlockNumber, Bytes, B256, U256};
use alloy_provider::Provider;
use clap::Parser;
use op_alloy_network::Optimism;

use revm::{
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

    /// Load prestate as [`EvmState`] from file if provided.
    ///
    /// Balance, storage, sender and faucet overrides that name an account the file already holds
    /// are applied to that account. Overrides of any other account are not inserted here: a
    /// default account would hide the rest of a forked account. [`Self::create_initial_state`]
    /// applies those as field overlays on top of the backend.
    pub fn load_prestate(&self, sender: &Address) -> Result<EvmState> {
        let (mut accounts, overlays) = self.prepare_prestate(sender)?;
        for (address, overlay) in overlays {
            let account = accounts.entry(address).or_default();
            if let Some(balance) = overlay.balance {
                account.info.balance = balance;
            }
            if !overlay.faucet.is_zero() {
                account.info.balance =
                    checked_faucet(account.info.balance, overlay.faucet, address)?;
            }
            account.storage.extend(overlay.storage);
        }
        Ok(accounts)
    }

    /// The file accounts, plus field overlays for addresses the file does not hold.
    fn prepare_prestate(
        &self,
        sender: &Address,
    ) -> Result<(EvmState, HashMap<Address, AccountOverlay>)> {
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
                let account = account_state.into_account()?;
                prestate.insert(address, account);
            }
            trace!(prestate = ?prestate, "Prestate loaded");
            prestate
        } else {
            debug!("No prestate file provided");
            HashMap::default()
        };
        let mut overlays: HashMap<Address, AccountOverlay> = HashMap::default();

        for (address, balance) in self.parse_balance()? {
            info!(address = %address, balance = %balance, "Overriding balance");
            set_balance_override(&mut prestate, &mut overlays, address, balance);
        }

        for (address, slot, value) in self.parse_storage()? {
            info!(address = %address, slot = %slot, value = %value, "Overriding storage");
            set_storage_override(&mut prestate, &mut overlays, address, slot, value);
        }

        if let Some(sender_balance_str) = &self.sender_balance {
            let sender_balance = parse_ether_value(sender_balance_str)?;
            info!(sender = %sender, sender_balance = %sender_balance, "Overriding sender balance");
            set_balance_override(&mut prestate, &mut overlays, *sender, sender_balance);
        }

        for (address, balance) in self.parse_faucet()? {
            info!(address = %address, balance = %balance, "Faucet: adding balance");
            add_faucet(&mut prestate, &mut overlays, address, balance)?;
        }

        Ok((prestate, overlays))
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
        let (prestate, overlays) = self.prepare_prestate(sender)?;
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
            let mut state =
                EvmeState::new_forked(provider, self.fork_block, prestate, block_hashes).await?;
            state.overlays = overlays;
            Ok((state, cache_store))
        } else {
            debug!("Creating local state");
            let mut state = EvmeState::new_empty(prestate, block_hashes);
            state.overlays = overlays;
            Ok((state, RpcCacheStore::noop()))
        }
    }
}

/// One CLI override of an account the prestate file does not already hold.
///
/// Applied on read, on top of the backend account, so a balance or one storage slot does not
/// replace the rest of a forked account.
#[derive(Debug, Default, Clone)]
struct AccountOverlay {
    /// Replaces the balance when set. Applied before [`Self::faucet`].
    balance: Option<U256>,
    /// Added to the resolved balance.
    faucet: U256,
    /// Slots that replace the underlying value. Other slots stay with the account.
    storage: HashMap<U256, EvmStorageSlot>,
}

fn set_balance_override(
    prestate: &mut EvmState,
    overlays: &mut HashMap<Address, AccountOverlay>,
    address: Address,
    balance: U256,
) {
    if let Some(account) = prestate.get_mut(&address) {
        account.info.balance = balance;
    } else {
        overlays.entry(address).or_default().balance = Some(balance);
    }
}

fn set_storage_override(
    prestate: &mut EvmState,
    overlays: &mut HashMap<Address, AccountOverlay>,
    address: Address,
    slot: U256,
    value: U256,
) {
    let stored = EvmStorageSlot::new(value, TransactionId::ZERO);
    if let Some(account) = prestate.get_mut(&address) {
        account.storage.insert(slot, stored);
    } else {
        overlays.entry(address).or_default().storage.insert(slot, stored);
    }
}

fn add_faucet(
    prestate: &mut EvmState,
    overlays: &mut HashMap<Address, AccountOverlay>,
    address: Address,
    amount: U256,
) -> Result<()> {
    if let Some(account) = prestate.get_mut(&address) {
        account.info.balance = checked_faucet(account.info.balance, amount, address)?;
    } else {
        let overlay = overlays.entry(address).or_default();
        overlay.faucet = checked_faucet(overlay.faucet, amount, address)?;
    }
    Ok(())
}

fn checked_faucet(balance: U256, add: U256, address: Address) -> Result<U256> {
    balance.checked_add(add).ok_or_else(|| {
        EvmeError::InvalidInput(format!("Faucet overflows the balance of {address}"))
    })
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
            .map(|(address, account)| (address, AccountState::from_account(account.clone())))
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
}

impl AccountState {
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
        }
    }

    /// Converts into [`Account`].
    ///
    /// Code present in the dump is checked against `code_hash`.
    /// Code left out stays unloaded: the stored hash is kept and `code` stays `None`, so a lazy
    /// account can be loaded again. Empty bytecode is not substituted for it.
    pub fn into_account(self) -> Result<Account> {
        let info = match self.code {
            Some(code) => {
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
                AccountInfo::new(
                    self.balance.unwrap_or_default(),
                    self.nonce.unwrap_or_default(),
                    computed_hash,
                    bytecode,
                )
            }
            None => AccountInfo {
                balance: self.balance.unwrap_or_default(),
                nonce: self.nonce.unwrap_or_default(),
                code_hash: self.code_hash.unwrap_or(revm::primitives::KECCAK_EMPTY),
                account_id: None,
                code: None,
            },
        };
        let storage = self
            .storage
            .unwrap_or_default()
            .into_iter()
            .map(|(slot, value)| (slot, EvmStorageSlot::new(value, TransactionId::ZERO)));
        Ok(Account::from(info).with_storage(storage))
    }
}

/// Fills bytecode into journal accounts that were loaded without it.
///
/// A dump records `code: null` together with the account's hash when the code was never fetched.
/// Loading it here, while the database is still open, puts the bytes in the dump so a later run
/// can execute them without that database. The bytes come from `basic_ref`. A hash that does not
/// match is left unloaded.
pub(crate) fn materialize_dumped_code<DB: DatabaseRef>(
    db: &DB,
    state: &mut EvmState,
) -> std::result::Result<(), DB::Error> {
    for (address, account) in state.iter_mut() {
        if account.info.code.is_some() || account.info.code_hash == revm::primitives::KECCAK_EMPTY {
            continue;
        }
        let Some(info) = db.basic_ref(*address)? else {
            continue;
        };
        if info.code_hash != account.info.code_hash {
            continue;
        }
        if let Some(code) = info.code {
            account.info.code = Some(code);
        }
    }
    Ok(())
}

/// Whether an account an RPC endpoint reported is one that does not exist.
///
/// JSON-RPC cannot say "no such account": `eth_getBalance`, `eth_getTransactionCount` and
/// `eth_getCode` answer zeros for it. A node's database answers `None`, and the difference is
/// observable (EIP-7702 refunds an authorization whose authority already exists), so an all-zero
/// answer is read as absent. An existing account that is empty cannot occur on a chain that has
/// had EIP-161 from genesis, as every `MegaETH` chain has.
fn rpc_reads_as_absent(info: &AccountInfo) -> bool {
    info.balance.is_zero() && info.nonce == 0 && info.code_hash == revm::primitives::KECCAK_EMPTY
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
    /// Field overrides for accounts the prestate file does not hold. Applied on top of the
    /// backend so one named field does not hide the rest of a forked account.
    overlays: HashMap<Address, AccountOverlay>,
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

        Self {
            backend: EvmeBackend::Empty(EmptyDB::default()),
            prestate,
            overlays: HashMap::default(),
            code_map,
            block_hashes,
        }
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

    /// Installs the code of Satin's predeploys: the six system contracts and the EIP-7997 factory.
    ///
    /// `run` and `tx` execute no block, so nothing deploys them; the code is applied as a raw
    /// state patch, over whatever a forked state holds at those addresses. The factory gets the
    /// nonce its deployment gives it. The `SequencerRegistry`'s storage is not seeded: a local run
    /// has no chain configuration to seed it from, and a forked run reads the chain's.
    pub fn deploy_system_contracts(&mut self) {
        use mega_evm::system::{system_contract_specs, SequencerRegistryConfig};

        for contract in system_contract_specs(&SequencerRegistryConfig::placeholder()) {
            self.set_account_code(contract.address, Bytecode::new_raw(contract.code));
            if contract.nonce != 0 {
                self.set_account_nonce(contract.address, contract.nonce);
            }
        }
    }

    /// Replaces the balance and adds the faucet on top of `info`. A storage-only overlay leaves
    /// the account as the backend reported it.
    fn apply_account_overlay(
        &self,
        address: Address,
        info: Option<AccountInfo>,
    ) -> Result<Option<AccountInfo>> {
        let Some(overlay) = self.overlays.get(&address) else {
            return Ok(info);
        };
        if overlay.balance.is_none() && overlay.faucet.is_zero() {
            return Ok(info);
        }
        let mut info = info.unwrap_or_default();
        if let Some(balance) = overlay.balance {
            info.balance = balance;
        }
        if !overlay.faucet.is_zero() {
            info.balance = checked_faucet(info.balance, overlay.faucet, address)?;
        }
        Ok(Some(info))
    }

    /// The overridden value of one slot, when this address has a field overlay for it.
    fn overlay_slot(&self, address: Address, index: U256) -> Option<U256> {
        self.overlays
            .get(&address)
            .and_then(|overlay| overlay.storage.get(&index))
            .map(|slot| {
                trace!(address = %address, index = %index, slot = %slot.present_value, "Loaded storage from field overlay");
                slot.present_value
            })
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

        Ok(Self {
            backend: EvmeBackend::Forked(Box::new(db)),
            prestate,
            overlays: HashMap::default(),
            code_map,
            block_hashes,
        })
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
        let account = match &mut self.backend {
            EvmeBackend::Empty(db) => {
                let account = db.basic(address).unwrap();
                trace!(address = %address, account = ?account, "Loaded account basic from empty state");
                account
            }
            EvmeBackend::Forked(db) => {
                let account = db.basic(address).map_err(|e| {
                    EvmeError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
                })?;
                trace!(address = %address, account = ?account, "Loaded account basic from forked state");
                account.filter(|info| !rpc_reads_as_absent(info))
            }
        };
        self.apply_account_overlay(address, account)
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
        if let Some(value) = self.overlay_slot(address, index) {
            return Ok(value);
        }
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
        let account = match &self.backend {
            EvmeBackend::Empty(db) => {
                let account = db.basic_ref(address).unwrap();
                trace!(address = %address, account = ?account, "Loaded account basic from empty state");
                account
            }
            EvmeBackend::Forked(db) => {
                let account = db.basic_ref(address).map_err(|e| {
                    EvmeError::RpcError(format!("Failed to fetch account {}: {:?}", address, e))
                })?;
                trace!(address = %address, account = ?account, "Loaded account basic from forked state");
                account.filter(|info| !rpc_reads_as_absent(info))
            }
        };
        self.apply_account_overlay(address, account)
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
        if let Some(value) = self.overlay_slot(address, index) {
            return Ok(value);
        }
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
    use alloy_provider::DynProvider;
    use op_alloy_network::Optimism;

    use super::*;

    fn lazy_account(hash: B256) -> Account {
        Account::from(AccountInfo {
            balance: U256::from(5u64),
            nonce: 3,
            code_hash: hash,
            account_id: None,
            code: None,
        })
    }

    #[test]
    fn test_lazy_code_reload_keeps_the_hash() {
        let hash = B256::repeat_byte(0xab);
        let loaded = AccountState::from_account(lazy_account(hash)).into_account().unwrap();
        assert!(loaded.info.code.is_none());
        assert_eq!(loaded.info.code_hash, hash);
        assert_eq!(loaded.info.balance, U256::from(5u64));
        assert_eq!(loaded.info.nonce, 3);
    }

    #[test]
    fn test_dump_loads_unfetched_code() {
        let bytecode = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00, 0x56]));
        let hash = bytecode.hash_slow();
        let address = Address::repeat_byte(0x11);
        let mut db: EvmeState<Optimism, DynProvider<Optimism>> =
            EvmeState::new_empty(EvmState::default(), HashMap::default());
        db.set_account_code(address, bytecode.clone());

        let mut state = EvmState::default();
        state.insert(address, lazy_account(hash));
        let other = Address::repeat_byte(0x22);
        state.insert(other, lazy_account(B256::repeat_byte(0xcd)));

        materialize_dumped_code(&db, &mut state).unwrap();

        assert_eq!(state.get(&address).unwrap().info.code.as_ref(), Some(&bytecode));
        assert!(state.get(&other).unwrap().info.code.is_none());
    }
}
