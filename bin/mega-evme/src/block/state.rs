//! The state a replayed block starts from, kept in plain data so that either engine can read it.
//!
//! A block's pre-state comes from a `prestateTracer` trace of the block. The trace reports, for
//! every transaction, the accounts and storage slots it touched as they stood just before it
//! ran; keeping the value from the first transaction that touched each account and slot gives
//! the state the parent block left, for everything the block's transactions read or wrote.
//!
//! What the trace does not cover (what only a pre-block system call reads, or a path a
//! counterfactual execution takes that the chain did not) is read from the parent block over RPC
//! when a provider is available, and folded into the pre-state, so a cached block written
//! afterwards replays offline along the same path.
//!
//! [`BlockState`] answers in `alloy-primitives` types only. Each engine reads it through a
//! `Database` of its own revm line: [`SatinDb`] here, and the legacy leg's in its module.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use alloy_primitives::{keccak256, Address, Bytes, B256, U256, U64};
use alloy_provider::Provider;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::debug;

use crate::common::{EvmeError, OpProvider, Result};

/// The hash of empty code.
pub const EMPTY_CODE_HASH: B256 = alloy_primitives::KECCAK256_EMPTY;

/// An account as the parent block left it, with the storage slots known so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreAccount {
    /// Balance in wei.
    pub balance: U256,
    /// Nonce.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nonce: u64,
    /// Hash of the account's code, absent for an account without code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_hash: Option<B256>,
    /// Storage slots known so far, by key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub storage: BTreeMap<B256, U256>,
}

const fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl PreAccount {
    /// Whether the account does not exist.
    ///
    /// Neither a trace nor an RPC endpoint can say "no such account": they report zeros for it.
    /// A node's database answers `None`, and the difference is observable (EIP-7702 refunds an
    /// authorization whose authority already exists), so an all-zero account is absent. An
    /// existing empty account cannot occur on a chain that has had EIP-161 from genesis.
    pub fn is_absent(&self) -> bool {
        self.balance.is_zero() && self.nonce == 0 && self.code_hash.is_none()
    }
}

/// The part of the parent block's state a block's execution reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockPrestate {
    /// Accounts by address.
    pub accounts: BTreeMap<Address, PreAccount>,
    /// Hashes of earlier blocks, by number, for `BLOCKHASH`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub block_hashes: BTreeMap<u64, B256>,
}

/// One transaction's entry in a `prestateTracer` block trace.
#[derive(Debug, Deserialize)]
struct TracedTx {
    #[serde(rename = "txHash", default)]
    tx_hash: Option<B256>,
    #[serde(default)]
    result: Option<BTreeMap<Address, TracedAccount>>,
    #[serde(default)]
    error: Option<String>,
}

/// An account in a `prestateTracer` result.
#[derive(Debug, Deserialize)]
struct TracedAccount {
    #[serde(default)]
    balance: Option<U256>,
    #[serde(default)]
    nonce: Option<u64>,
    #[serde(default)]
    code: Option<Bytes>,
    #[serde(default)]
    storage: BTreeMap<B256, B256>,
}

impl BlockPrestate {
    /// Builds the pre-state from a `prestateTracer` trace of a whole block, adding every code it
    /// carries to `codes`.
    ///
    /// Transactions are taken in block order and the first value seen for each account and slot
    /// wins: a later transaction sees what the earlier ones left, not what the parent block did.
    pub fn from_block_trace(trace: serde_json::Value, codes: &mut CodeStore) -> Result<Self> {
        let traced: Vec<TracedTx> = serde_json::from_value(trace)
            .map_err(|e| EvmeError::RpcError(format!("Malformed prestate trace: {e}")))?;
        let mut prestate = Self::default();
        for (index, tx) in traced.into_iter().enumerate() {
            let Some(accounts) = tx.result else {
                return Err(EvmeError::RpcError(format!(
                    "Prestate trace of transaction {index} ({}) failed: {}",
                    tx.tx_hash.map(|h| h.to_string()).unwrap_or_default(),
                    tx.error.unwrap_or_else(|| "no result".to_string()),
                )));
            };
            for (address, traced) in accounts {
                let account = prestate.accounts.entry(address).or_insert_with(|| PreAccount {
                    balance: traced.balance.unwrap_or_default(),
                    nonce: traced.nonce.unwrap_or_default(),
                    code_hash: traced
                        .code
                        .filter(|code| !code.is_empty())
                        .map(|code| codes.insert(code)),
                    storage: BTreeMap::new(),
                });
                for (slot, value) in traced.storage {
                    account.storage.entry(slot).or_insert_with(|| value.into());
                }
            }
        }
        Ok(prestate)
    }
}

/// Contract code by hash, kept in memory and, when a directory is given, on disk as one file per
/// code so that blocks sharing a contract share its bytes.
#[derive(Debug, Default)]
pub struct CodeStore {
    dir: Option<PathBuf>,
    codes: HashMap<B256, Bytes>,
    unsaved: Vec<B256>,
}

impl CodeStore {
    /// A store persisted under `dir`, or kept in memory only.
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir, ..Default::default() }
    }

    fn path(&self, hash: &B256) -> Option<PathBuf> {
        let hex = hash.to_string();
        self.dir.as_ref().map(|dir| dir.join(&hex[2..4]).join(format!("{}.bin", &hex[2..])))
    }

    /// Adds `code`, returning its hash.
    pub fn insert(&mut self, code: Bytes) -> B256 {
        let hash = keccak256(&code);
        if let std::collections::hash_map::Entry::Vacant(entry) = self.codes.entry(hash) {
            entry.insert(code);
            self.unsaved.push(hash);
        }
        hash
    }

    /// The code with `hash`, loading it from disk if it is not in memory.
    pub fn get(&mut self, hash: &B256) -> Result<Option<Bytes>> {
        if *hash == EMPTY_CODE_HASH {
            return Ok(Some(Bytes::new()));
        }
        if let Some(code) = self.codes.get(hash) {
            return Ok(Some(code.clone()));
        }
        let Some(path) = self.path(hash) else { return Ok(None) };
        match fs::read(&path) {
            Ok(bytes) => {
                let code = Bytes::from(bytes);
                if keccak256(&code) != *hash {
                    return Err(EvmeError::FixtureError(format!(
                        "code store entry {} does not hash to its name",
                        path.display()
                    )));
                }
                self.codes.insert(*hash, code.clone());
                Ok(Some(code))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Writes the codes added since the last call to disk. Files are written whole and renamed
    /// into place, so processes sharing a store never see a partial one.
    pub fn persist(&mut self) -> Result<()> {
        if self.dir.is_none() {
            self.unsaved.clear();
            return Ok(());
        }
        for hash in std::mem::take(&mut self.unsaved) {
            let path = self.path(&hash).expect("a store with a directory has paths");
            if path.exists() {
                continue;
            }
            let code = self.codes.get(&hash).expect("an unsaved code is in memory");
            write_atomically(&path, code)?;
        }
        Ok(())
    }
}

/// Writes `bytes` to `path` through a temporary file in the same directory.
pub(super) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().expect("a file path has a parent");
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.persist(path).map_err(|e| EvmeError::FileRead(e.error))?;
    // A temporary file is private to its owner; a cache entry is readable like any other file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

/// Reads the state after block `parent` over RPC, one request at a time.
#[derive(Debug)]
pub struct ParentStateRpc {
    provider: OpProvider,
    parent: u64,
    runtime: tokio::runtime::Handle,
}

impl ParentStateRpc {
    /// Reads the state after block `parent`. `None` outside a tokio runtime.
    pub fn new(provider: OpProvider, parent: u64) -> Option<Self> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        Some(Self { provider, parent, runtime })
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        tokio::task::block_in_place(|| self.runtime.block_on(future))
    }

    fn call<R: alloy_json_rpc::RpcRecv>(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<R> {
        self.block_on(self.provider.raw_request::<_, R>(method.into(), params))
            .map_err(|e| EvmeError::RpcError(format!("{method}: {e}")))
    }

    fn account(&self, address: Address) -> Result<(PreAccount, Bytes)> {
        let block = format!("0x{:x}", self.parent);
        let balance: U256 = self.call("eth_getBalance", json!([address, block]))?;
        let nonce: U64 = self.call("eth_getTransactionCount", json!([address, block]))?;
        let code: Bytes = self.call("eth_getCode", json!([address, block]))?;
        let code_hash = (!code.is_empty()).then(|| keccak256(&code));
        Ok((PreAccount { balance, nonce: nonce.to(), code_hash, storage: BTreeMap::new() }, code))
    }

    fn storage(&self, address: Address, slot: B256) -> Result<U256> {
        self.call("eth_getStorageAt", json!([address, slot, format!("0x{:x}", self.parent)]))
    }

    fn block_hash(&self, number: u64) -> Result<Option<B256>> {
        let block: Option<serde_json::Value> =
            self.call("eth_getBlockByNumber", json!([format!("0x{number:x}"), false]))?;
        block
            .map(|block| {
                serde_json::from_value::<B256>(block["hash"].clone())
                    .map_err(|e| EvmeError::RpcError(format!("Malformed block {number}: {e}")))
            })
            .transpose()
    }
}

/// An account as an engine reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountData {
    /// Balance in wei.
    pub balance: U256,
    /// Nonce.
    pub nonce: u64,
    /// Hash of the code.
    pub code_hash: B256,
    /// The code.
    pub code: Bytes,
}

/// The first read a block's state could not serve, which fails the block.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StateReadError(pub String);

impl revm::database::DBErrorMarker for StateReadError {}

/// The state a block executes against: its pre-state, falling back to the parent block over RPC
/// for what the pre-state does not hold.
#[derive(Debug)]
pub struct BlockState {
    /// The pre-state; grows with every read the fallback serves.
    pub prestate: BlockPrestate,
    /// Contract code.
    pub codes: CodeStore,
    fallback: Option<ParentStateRpc>,
    /// Number of reads the fallback served.
    pub fallback_reads: usize,
    /// The first read neither the pre-state nor a fallback could serve.
    pub miss: Option<String>,
}

impl BlockState {
    /// A state over `prestate` and `codes`, reading what they lack through `fallback`.
    pub fn new(
        prestate: BlockPrestate,
        codes: CodeStore,
        fallback: Option<ParentStateRpc>,
    ) -> Self {
        Self { prestate, codes, fallback, fallback_reads: 0, miss: None }
    }

    fn fail(&mut self, message: String) -> StateReadError {
        self.miss.get_or_insert_with(|| message.clone());
        StateReadError(message)
    }

    fn missing(&mut self, what: String) -> StateReadError {
        self.fail(format!("state not in the block's pre-state and no RPC to read it: {what}"))
    }

    /// The account at `address`, or `None` when it does not exist.
    pub fn account(
        &mut self,
        address: Address,
    ) -> std::result::Result<Option<AccountData>, StateReadError> {
        if !self.prestate.accounts.contains_key(&address) {
            let Some(fallback) = &self.fallback else {
                return Err(self.missing(format!("account {address}")));
            };
            let (account, code) = match fallback.account(address) {
                Ok(read) => read,
                Err(e) => return Err(self.fail(format!("reading account {address}: {e}"))),
            };
            self.fallback_reads += 1;
            debug!(%address, "Read an account the pre-state lacks from the parent block");
            if !code.is_empty() {
                self.codes.insert(code);
            }
            self.prestate.accounts.insert(address, account);
        }
        let account = self.prestate.accounts[&address].clone();
        if account.is_absent() {
            return Ok(None);
        }
        let code_hash = account.code_hash.unwrap_or(EMPTY_CODE_HASH);
        let code = self.code(code_hash)?;
        Ok(Some(AccountData { balance: account.balance, nonce: account.nonce, code_hash, code }))
    }

    /// The code with `code_hash`.
    pub fn code(&mut self, code_hash: B256) -> std::result::Result<Bytes, StateReadError> {
        match self.codes.get(&code_hash) {
            Ok(Some(code)) => Ok(code),
            Ok(None) => Err(self.missing(format!("code {code_hash}"))),
            Err(e) => Err(self.fail(format!("reading code {code_hash}: {e}"))),
        }
    }

    /// The value of `slot` in `address`'s storage.
    pub fn storage(
        &mut self,
        address: Address,
        slot: U256,
    ) -> std::result::Result<U256, StateReadError> {
        let key = B256::from(slot);
        if let Some(value) =
            self.prestate.accounts.get(&address).and_then(|account| account.storage.get(&key))
        {
            return Ok(*value);
        }
        if self.fallback.is_none() {
            // A slot of an account the trace saw is a slot the block did not touch before it
            // was read here; only an account the pre-state holds whole could answer zero, and
            // none does.
            return Err(self.missing(format!("storage {address}[{key}]")));
        }
        // The slot is recorded under its account, so the account must be in the pre-state
        // first; otherwise an empty entry would later pass for the account.
        if !self.prestate.accounts.contains_key(&address) {
            self.account(address)?;
        }
        let fallback = self.fallback.as_ref().expect("checked above");
        let value = match fallback.storage(address, key) {
            Ok(value) => value,
            Err(e) => return Err(self.fail(format!("reading storage {address}[{key}]: {e}"))),
        };
        self.fallback_reads += 1;
        debug!(%address, %key, "Read a storage slot the pre-state lacks from the parent block");
        self.prestate.accounts.get_mut(&address).expect("loaded above").storage.insert(key, value);
        Ok(value)
    }

    /// The hash of block `number`.
    pub fn block_hash(&mut self, number: u64) -> std::result::Result<B256, StateReadError> {
        if let Some(hash) = self.prestate.block_hashes.get(&number) {
            return Ok(*hash);
        }
        let Some(fallback) = &self.fallback else {
            return Err(self.missing(format!("hash of block {number}")));
        };
        let hash = match fallback.block_hash(number) {
            Ok(Some(hash)) => hash,
            Ok(None) => return Err(self.fail(format!("block {number} does not exist"))),
            Err(e) => return Err(self.fail(format!("reading the hash of block {number}: {e}"))),
        };
        self.fallback_reads += 1;
        self.prestate.block_hashes.insert(number, hash);
        Ok(hash)
    }

    /// The keys of the pre-state, to tell what a replay read beyond them.
    pub fn keys(&self) -> BTreeSet<(Address, Option<B256>)> {
        self.prestate
            .accounts
            .iter()
            .flat_map(|(address, account)| {
                std::iter::once((*address, None))
                    .chain(account.storage.keys().map(|slot| (*address, Some(*slot))))
            })
            .collect()
    }
}

/// The Satin engine's view of a [`BlockState`].
#[derive(Debug)]
pub struct SatinDb<'a>(pub &'a mut BlockState);

impl revm::Database for SatinDb<'_> {
    type Error = StateReadError;

    fn basic(
        &mut self,
        address: Address,
    ) -> std::result::Result<Option<revm::state::AccountInfo>, Self::Error> {
        Ok(self.0.account(address)?.map(|account| {
            revm::state::AccountInfo::new(
                account.balance,
                account.nonce,
                account.code_hash,
                revm::state::Bytecode::new_raw(account.code),
            )
        }))
    }

    fn code_by_hash(
        &mut self,
        code_hash: B256,
    ) -> std::result::Result<revm::state::Bytecode, Self::Error> {
        Ok(revm::state::Bytecode::new_raw(self.0.code(code_hash)?))
    }

    fn storage(&mut self, address: Address, index: U256) -> std::result::Result<U256, Self::Error> {
        self.0.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> std::result::Result<B256, Self::Error> {
        self.0.block_hash(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, bytes};

    const ALICE: Address = address!("0x00000000000000000000000000000000000a11ce");
    const TOKEN: Address = address!("0x0000000000000000000000000000000000007070");

    fn slot(n: u8) -> B256 {
        B256::with_last_byte(n)
    }

    fn word(n: u64) -> String {
        B256::from(U256::from(n)).to_string()
    }

    /// Two transactions touch the same account and slot; the second one saw what the first
    /// left. The pre-state keeps the first sighting of each, and the second transaction's
    /// sighting of a slot the first did not touch.
    #[test]
    fn test_the_first_sighting_of_each_account_and_slot_is_the_parent_state() {
        let trace = json!([
            {
                "txHash": B256::with_last_byte(1),
                "result": {
                    ALICE.to_string(): { "balance": "0x64", "nonce": 7 },
                    TOKEN.to_string(): {
                        "balance": "0x0",
                        "code": "0x6001600055",
                        "storage": { slot(1).to_string(): word(10) }
                    }
                }
            },
            {
                "txHash": B256::with_last_byte(2),
                "result": {
                    ALICE.to_string(): { "balance": "0x32", "nonce": 8 },
                    TOKEN.to_string(): {
                        "balance": "0x0",
                        "code": "0x6001600055",
                        "storage": { slot(1).to_string(): word(11), slot(2).to_string(): word(20) }
                    }
                }
            }
        ]);
        let mut codes = CodeStore::new(None);
        let prestate = BlockPrestate::from_block_trace(trace, &mut codes).unwrap();

        let alice = &prestate.accounts[&ALICE];
        assert_eq!((alice.balance, alice.nonce, alice.code_hash), (U256::from(100), 7, None));
        let token = &prestate.accounts[&TOKEN];
        assert_eq!(token.storage[&slot(1)], U256::from(10), "the first transaction's view");
        assert_eq!(token.storage[&slot(2)], U256::from(20), "first seen by the second transaction");
        let code = bytes!("6001600055");
        assert_eq!(token.code_hash, Some(keccak256(&code)));
        assert_eq!(codes.get(&keccak256(&code)).unwrap().unwrap(), code, "kept once, by hash");
    }

    #[test]
    fn test_a_failed_transaction_trace_fails_the_block() {
        let trace = json!([{ "txHash": B256::ZERO, "error": "execution timeout" }]);
        let err = BlockPrestate::from_block_trace(trace, &mut CodeStore::new(None)).unwrap_err();
        assert!(err.to_string().contains("execution timeout"), "{err}");
    }

    /// An all-zero account is absent; offline, anything the pre-state lacks fails the read and
    /// is remembered as the block's miss.
    #[test]
    fn test_reads_of_the_block_state() {
        let mut prestate = BlockPrestate::default();
        prestate.accounts.insert(ALICE, PreAccount::default());
        prestate.accounts.insert(
            TOKEN,
            PreAccount {
                balance: U256::from(5),
                storage: BTreeMap::from([(slot(1), U256::from(9))]),
                ..Default::default()
            },
        );
        let mut state = BlockState::new(prestate, CodeStore::new(None), None);

        assert_eq!(state.account(ALICE).unwrap(), None, "all zeros is no account");
        let token = state.account(TOKEN).unwrap().unwrap();
        assert_eq!((token.balance, token.code_hash), (U256::from(5), EMPTY_CODE_HASH));
        assert_eq!(state.storage(TOKEN, U256::from(1)).unwrap(), U256::from(9));
        assert!(state.miss.is_none());

        let err = state.storage(TOKEN, U256::from(2)).unwrap_err();
        assert!(err.0.contains("no RPC to read it"), "{err}");
        assert!(state.account(Address::ZERO).is_err());
        assert_eq!(state.miss.as_deref(), Some(err.0.as_str()), "the first miss is kept");
    }
}
