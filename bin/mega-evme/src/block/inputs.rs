//! Everything replaying a block needs from the chain, fetched over RPC or read from a block cache.
//!
//! Three requests describe a block: the block with its transactions, its receipts, and a
//! `prestateTracer` trace from which its pre-state is built. The block cache keeps them per block
//! as one zstd-compressed JSON file, `blocks/<n / 10000>/<n>.json.zst`, with contract code split
//! out into a shared store, `codes/<hh>/<hash>.bin`.

use std::path::{Path, PathBuf};

use alloy_consensus::BlockHeader as _;
use alloy_eips::Encodable2718;
use alloy_primitives::{keccak256, Address, Bytes, Log, B256, U256, U64};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Block;
use op_alloy_rpc_types::Transaction;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::state::{write_atomically, BlockPrestate, CodeStore};
use crate::common::{EvmeError, OpProvider, Result};

/// Version of the cached block format.
const CACHE_VERSION: u32 = 1;

/// zstd level cached blocks are written at.
const CACHE_ZSTD_LEVEL: i32 = 3;

/// A block's inputs as cached: the RPC responses for the block and its receipts, and the
/// pre-state built from its trace.
#[derive(Debug, Serialize, Deserialize)]
pub struct CachedBlock {
    /// Format version.
    pub version: u32,
    /// Chain the block belongs to.
    pub chain_id: u64,
    /// `eth_getBlockByNumber` result, with full transactions.
    pub block: Value,
    /// `eth_getBlockReceipts` result.
    pub receipts: Value,
    /// Pre-state; codes live in the code store.
    pub prestate: BlockPrestate,
}

/// The header fields block execution reads, in plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderFields {
    /// The block's hash.
    pub hash: B256,
    /// The block's number.
    pub number: u64,
    /// The block's timestamp.
    pub timestamp: u64,
    /// The block's beneficiary.
    pub beneficiary: Address,
    /// The block's gas limit.
    pub gas_limit: u64,
    /// The block's base fee.
    pub base_fee: u64,
    /// The block's difficulty.
    pub difficulty: U256,
    /// The block's `mixHash`, which is its `PREVRANDAO`.
    pub mix_hash: B256,
    /// The block's excess blob gas.
    pub excess_blob_gas: u64,
    /// The parent block's hash.
    pub parent_hash: B256,
    /// The parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// The block's extra data.
    pub extra_data: Bytes,
    /// The block's receipts root.
    pub receipts_root: B256,
}

/// A transaction of the block, as either engine takes it: its EIP-2718 envelope and its sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxInput {
    /// The transaction's hash.
    pub hash: B256,
    /// The EIP-2718 encoding of the transaction.
    pub envelope: Bytes,
    /// The recovered sender.
    pub sender: Address,
}

/// A log as the chain recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChainLog {
    /// The emitting contract.
    pub address: Address,
    /// The topics.
    pub topics: Vec<B256>,
    /// The data.
    pub data: Bytes,
}

/// The parts of a receipt a replay is compared with.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainReceipt {
    /// Hash of the transaction.
    pub transaction_hash: B256,
    /// Gas used as the chain recorded it.
    pub gas_used: U64,
    /// Cumulative gas used as the chain recorded it.
    pub cumulative_gas_used: U64,
    /// `0x1` for success.
    #[serde(default)]
    pub status: Option<U64>,
    /// Logs as the chain recorded them.
    #[serde(default)]
    pub logs: Vec<ChainLog>,
}

impl ChainReceipt {
    /// Whether the transaction succeeded.
    pub fn success(&self) -> bool {
        self.status.is_some_and(|status| status == U64::from(1))
    }

    /// Whether `logs` are the chain's logs, in order.
    pub fn logs_equal(&self, logs: &[Log]) -> bool {
        self.logs.len() == logs.len() &&
            self.logs.iter().zip(logs).all(|(chain, log)| {
                chain.address == log.address &&
                    chain.topics == log.data.topics() &&
                    chain.data == log.data.data
            })
    }
}

/// A block ready to replay.
#[derive(Debug)]
pub struct BlockInputs {
    /// Chain the block belongs to.
    pub chain_id: u64,
    /// The header fields execution reads.
    pub header: HeaderFields,
    /// The transactions, in block order.
    pub transactions: Vec<TxInput>,
    /// Receipts, in transaction order.
    pub receipts: Vec<ChainReceipt>,
    /// The part of the parent block's state the block reads.
    pub prestate: BlockPrestate,
    /// Whether the inputs came from the block cache.
    pub from_cache: bool,
    /// The block and receipts responses as received, for writing the cache entry.
    raw: (Value, Value),
}

impl BlockInputs {
    /// Parses the RPC responses of a block into its inputs.
    pub fn from_responses(
        chain_id: u64,
        block: Value,
        receipts: Value,
        prestate: BlockPrestate,
        from_cache: bool,
    ) -> Result<Self> {
        let parsed: Block<Transaction> = serde_json::from_value(block.clone())
            .map_err(|e| EvmeError::RpcError(format!("Malformed block: {e}")))?;
        let chain_receipts: Vec<ChainReceipt> = serde_json::from_value(receipts.clone())
            .map_err(|e| EvmeError::RpcError(format!("Malformed receipts: {e}")))?;
        let header = &parsed.header;
        let number = header.number();
        let fields = HeaderFields {
            hash: header.hash,
            number,
            timestamp: header.timestamp(),
            beneficiary: header.beneficiary(),
            gas_limit: header.gas_limit(),
            base_fee: header.base_fee_per_gas().unwrap_or_default(),
            difficulty: header.difficulty(),
            mix_hash: header.mix_hash().unwrap_or_default(),
            excess_blob_gas: header.excess_blob_gas().ok_or_else(|| {
                EvmeError::RpcError(format!("block {number} has no excess_blob_gas"))
            })?,
            parent_hash: header.parent_hash(),
            parent_beacon_block_root: header.parent_beacon_block_root(),
            extra_data: header.extra_data().clone(),
            receipts_root: header.receipts_root(),
        };
        let raw_txs = block["transactions"].as_array().cloned().unwrap_or_default();
        let transactions = parsed
            .transactions
            .into_transactions()
            .zip(raw_txs)
            .map(|(tx, raw)| tx_input(tx, &raw))
            .collect::<Result<Vec<_>>>()?;
        if transactions.is_empty() &&
            block["transactions"].as_array().is_some_and(|t| !t.is_empty())
        {
            return Err(EvmeError::RpcError(format!(
                "block {number} lists transaction hashes, not transactions"
            )));
        }
        if chain_receipts.len() != transactions.len() {
            return Err(EvmeError::RpcError(format!(
                "block {number} has {} transactions but {} receipts",
                transactions.len(),
                chain_receipts.len()
            )));
        }
        for (tx, receipt) in transactions.iter().zip(&chain_receipts) {
            if tx.hash != receipt.transaction_hash {
                return Err(EvmeError::RpcError(format!(
                    "block {number}: receipt of {} listed at the position of {}",
                    receipt.transaction_hash, tx.hash
                )));
            }
        }
        Ok(Self {
            chain_id,
            header: fields,
            transactions,
            receipts: chain_receipts,
            prestate,
            from_cache,
            raw: (block, receipts),
        })
    }

    /// The cache entry of these inputs, with `prestate` as the block's pre-state.
    fn to_cached(&self, prestate: &BlockPrestate) -> CachedBlock {
        CachedBlock {
            version: CACHE_VERSION,
            chain_id: self.chain_id,
            block: self.raw.0.clone(),
            receipts: self.raw.1.clone(),
            prestate: prestate.clone(),
        }
    }
}

/// The EIP-2718 envelope and sender of `tx`, whose re-encoding must hash to the hash the chain
/// reports for it: the envelope is rebuilt from the RPC object, and a field the object dropped
/// would otherwise replay a different transaction.
fn tx_input(tx: Transaction, raw: &Value) -> Result<TxInput> {
    let reported: B256 = serde_json::from_value(raw["hash"].clone())
        .map_err(|e| EvmeError::RpcError(format!("Malformed transaction hash: {e}")))?;
    let recovered = tx.inner.inner;
    let sender = recovered.signer();
    let envelope = Bytes::from(recovered.inner().encoded_2718());
    let hash = keccak256(&envelope);
    if hash != reported {
        return Err(EvmeError::RpcError(format!(
            "transaction {reported} re-encodes to a different hash, {hash}"
        )));
    }
    Ok(TxInput { hash, envelope, sender })
}

/// Where a block's inputs come from: a block cache, an RPC provider, or both, in that order.
#[derive(Debug)]
pub struct BlockSource {
    cache_dir: Option<PathBuf>,
    provider: Option<OpProvider>,
    chain_id: Option<u64>,
}

impl BlockSource {
    /// A source reading `cache_dir` first and `provider` (on chain `chain_id`) for what the
    /// cache lacks.
    pub fn new(
        cache_dir: Option<PathBuf>,
        provider: Option<OpProvider>,
        chain_id: Option<u64>,
    ) -> Self {
        Self { cache_dir, provider, chain_id }
    }

    /// The code store the cache keeps codes in.
    pub fn code_store(&self) -> CodeStore {
        CodeStore::new(self.cache_dir.as_ref().map(|dir| dir.join("codes")))
    }

    /// The provider, when there is one.
    pub fn provider(&self) -> Option<&OpProvider> {
        self.provider.as_ref()
    }

    fn block_path(dir: &Path, number: u64) -> PathBuf {
        dir.join("blocks").join((number / 10_000).to_string()).join(format!("{number}.json.zst"))
    }

    /// The inputs of block `number`, adding the codes its trace carries to `codes`.
    pub async fn load(&self, number: u64, codes: &mut CodeStore) -> Result<BlockInputs> {
        if let Some(dir) = &self.cache_dir {
            let path = Self::block_path(dir, number);
            if path.exists() {
                let compressed = std::fs::read(&path)?;
                let json = zstd::decode_all(compressed.as_slice())?;
                let cached: CachedBlock = serde_json::from_slice(&json)
                    .map_err(|e| EvmeError::FixtureError(format!("{}: {e}", path.display())))?;
                if cached.version != CACHE_VERSION {
                    return Err(EvmeError::FixtureError(format!(
                        "{}: cache version {} is not {CACHE_VERSION}",
                        path.display(),
                        cached.version
                    )));
                }
                if let Some(chain_id) = self.chain_id {
                    if cached.chain_id != chain_id {
                        return Err(EvmeError::FixtureError(format!(
                            "{}: cached for chain {}, the RPC serves chain {chain_id}",
                            path.display(),
                            cached.chain_id
                        )));
                    }
                }
                return BlockInputs::from_responses(
                    cached.chain_id,
                    cached.block,
                    cached.receipts,
                    cached.prestate,
                    true,
                );
            }
        }
        let (Some(provider), Some(chain_id)) = (&self.provider, self.chain_id) else {
            return Err(EvmeError::Other(format!(
                "block {number} is not in the block cache, and there is no '--rpc' to fetch it"
            )));
        };
        let tag = format!("0x{number:x}");
        let request = |method: &'static str, params: Value| async move {
            provider
                .raw_request::<_, Value>(method.into(), params)
                .await
                .map_err(|e| EvmeError::RpcError(format!("{method} of block {number}: {e}")))
        };
        let block = request("eth_getBlockByNumber", json!([tag, true])).await?;
        if block.is_null() {
            return Err(EvmeError::BlockNotFound(number));
        }
        let receipts = request("eth_getBlockReceipts", json!([tag])).await?;
        let trace =
            request("debug_traceBlockByNumber", json!([tag, { "tracer": "prestateTracer" }]))
                .await?;
        let prestate = BlockPrestate::from_block_trace(trace, codes)?;
        BlockInputs::from_responses(chain_id, block, receipts, prestate, false)
    }

    /// Writes `inputs` to the cache with `prestate` as their pre-state, and the codes it holds.
    /// A no-op without a cache directory.
    pub fn store(
        &self,
        inputs: &BlockInputs,
        prestate: &BlockPrestate,
        codes: &mut CodeStore,
    ) -> Result<()> {
        let Some(dir) = &self.cache_dir else { return Ok(()) };
        codes.persist()?;
        let json = serde_json::to_vec(&inputs.to_cached(prestate))
            .map_err(|e| EvmeError::Other(format!("serializing block cache entry: {e}")))?;
        let compressed = zstd::encode_all(json.as_slice(), CACHE_ZSTD_LEVEL)?;
        write_atomically(&Self::block_path(dir, inputs.header.number), &compressed)
    }
}
