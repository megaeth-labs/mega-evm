//! Batch replay driver: replay many transactions inside a single process.
//!
//! The single-transaction path ([`super::cmd`]) builds a provider, forks state at
//! the parent block, and executes one block per process. Verifying a large corpus
//! that way pays the provider/cache setup once per transaction, which dominates
//! the actual EVM work. This module reuses one provider and one RPC cache for the
//! whole run, groups the requested transactions by their containing block, and
//! executes each block exactly once while recording the result of every target it
//! passes through.
//!
//! Every RPC call issued here has the same shape as the single-transaction path
//! (`eth_getTransactionByHash`, `eth_getBlockByNumber` with hash-only bodies, and
//! the state reads behind [`EvmeState::new_forked`]), so an offline envelope
//! captured by single-transaction replays serves batch runs without a miss.

use std::{
    collections::{BTreeMap, HashSet},
    str::FromStr,
    time::{Duration, Instant},
};

use alloy_consensus::{BlockHeader, Transaction as _};
use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::Block;
use mega_evm::{
    alloy_evm::{block::BlockExecutor, Evm, EvmEnv},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{result::ExecutionResult, ContextTr},
        database::{states::bundle_state::BundleRetention, StateBuilder},
        DatabaseRef,
    },
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaHaltReason,
    MegaHardforks,
};
use op_alloy_rpc_types::Transaction;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::{
    common::{
        op_receipt_to_tx_receipt, print_execution_summary, print_receipt, EvmeExternalEnvs,
        ExecutionSummary, OpTxReceipt,
    },
    replay::get_hardfork_config,
    ChainArgs, EvmeState,
};

use super::{cmd::retrieve_block_env, ReplayError, Result};

/// What a batch run was asked to replay.
#[derive(Debug)]
pub(super) enum BatchMode {
    /// Transaction hashes read from `--tx-file`, in file order.
    TxList(Vec<B256>),
    /// Every transaction of the block given by `--block`.
    Block(u64),
}

/// Why a target transaction produced no execution result.
///
/// Execution outcomes (success, revert, halt) are normal results and never map
/// to one of these kinds.
#[derive(Debug, Clone, Copy)]
enum BatchErrorKind {
    /// The transaction hash is unknown to the endpoint.
    NotFound,
    /// The transaction exists but is not mined yet (no block number).
    Pending,
    /// An RPC call failed or returned nothing.
    Rpc,
    /// The block executor rejected the transaction or the block setup failed.
    Execution,
}

impl BatchErrorKind {
    /// Wire name used in the NDJSON error line and the human-readable output.
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Pending => "pending",
            Self::Rpc => "rpc",
            Self::Execution => "execution",
        }
    }
}

/// Outcome of a single target transaction.
enum BatchEntry {
    /// The transaction executed and produced a result (success, revert, or halt).
    Executed(Box<ExecutedTx>),
    /// The transaction could not be executed.
    Failed(FailedTx),
}

impl BatchEntry {
    /// Hash of the target this entry reports on.
    const fn tx_hash(&self) -> B256 {
        match self {
            Self::Executed(tx) => tx.tx_hash,
            Self::Failed(tx) => tx.tx_hash,
        }
    }
}

/// A target transaction that ran to completion.
struct ExecutedTx {
    tx_hash: B256,
    block_number: u64,
    tx_index: u64,
    exec_result: ExecutionResult<MegaHaltReason>,
    contract_address: Option<Address>,
    exec_time: Duration,
    receipt: OpTxReceipt,
}

/// A target transaction that hit an infrastructure failure.
struct FailedTx {
    tx_hash: B256,
    kind: BatchErrorKind,
    message: String,
}

/// One block's worth of work.
struct BlockJob {
    /// Number of the block holding the targets.
    number: u64,
    /// Block body, present when planning already fetched it (`--block`).
    block: Option<Block<Transaction>>,
    /// Hashes of the transactions whose results are reported.
    targets: Vec<B256>,
}

/// A target that executed, awaiting the receipt harvested by `finish()`.
struct PendingTarget {
    tx_hash: B256,
    tx_index: u64,
    /// Position of this transaction among the block's committed transactions.
    commit_index: usize,
    exec_result: ExecutionResult<MegaHaltReason>,
    exec_time: Duration,
    gas_used: u64,
    pre_execution_nonce: u64,
    from: Address,
    to: Option<Address>,
    effective_gas_price: u128,
}

/// NDJSON line for a target that produced an execution result.
#[derive(Serialize)]
struct BatchResultLine<'a> {
    tx_hash: B256,
    block_number: u64,
    tx_index: u64,
    #[serde(flatten)]
    summary: &'a ExecutionSummary,
}

/// NDJSON line for a target that produced an infrastructure error.
#[derive(Serialize)]
struct BatchErrorLine<'a> {
    tx_hash: B256,
    error: BatchErrorBody<'a>,
}

/// Error payload of a [`BatchErrorLine`].
#[derive(Serialize)]
struct BatchErrorBody<'a> {
    kind: &'static str,
    message: &'a str,
}

/// Replay every requested transaction, reporting one entry per target.
///
/// Returns an error when at least one target produced an infrastructure error
/// entry, so the process exits non-zero; execution outcomes never fail the run.
pub(super) async fn run<P>(
    provider: &P,
    chain_id: u64,
    mode: &BatchMode,
    external_envs: EvmeExternalEnvs,
    json: bool,
) -> Result<()>
where
    P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug,
{
    let start = Instant::now();
    let mut replayed = 0usize;
    let mut failed = 0usize;

    let jobs = match mode {
        BatchMode::Block(number) => {
            let block = fetch_block(provider, *number).await?;
            let targets: Vec<B256> = block.transactions.hashes().collect();
            info!(block = number, tx_count = targets.len(), "Batch replay of a whole block");
            vec![BlockJob { number: *number, block: Some(block), targets }]
        }
        BatchMode::TxList(hashes) => {
            let (jobs, failures) = resolve_targets(provider, hashes).await;
            info!(
                requested = hashes.len(),
                blocks = jobs.len(),
                unresolved = failures.len(),
                "Batch replay of a transaction list",
            );
            for failure in failures {
                failed += 1;
                emit(&BatchEntry::Failed(failure), json);
            }
            jobs
        }
    };

    for job in jobs {
        for entry in replay_block(provider, chain_id, job, external_envs.clone()).await {
            match entry {
                BatchEntry::Executed(_) => replayed += 1,
                BatchEntry::Failed(_) => failed += 1,
            }
            emit(&entry, json);
        }
    }

    info!(replayed, failed, elapsed = ?start.elapsed(), "Batch replay finished");

    if failed > 0 {
        return Err(ReplayError::Other(format!(
            "{failed} of {} target transaction(s) failed to replay",
            replayed + failed
        )));
    }
    Ok(())
}

/// Resolve each requested hash to its containing block.
///
/// Returns the per-block jobs in ascending block order, plus the failures for
/// hashes that could not be resolved (in the order they were requested).
async fn resolve_targets<P>(provider: &P, hashes: &[B256]) -> (Vec<BlockJob>, Vec<FailedTx>)
where
    P: Provider<op_alloy_network::Optimism>,
{
    let mut grouped: BTreeMap<u64, Vec<B256>> = BTreeMap::new();
    let mut failures = Vec::new();

    for hash in hashes {
        match provider.get_transaction_by_hash(*hash).await {
            Err(e) => failures.push(FailedTx {
                tx_hash: *hash,
                kind: BatchErrorKind::Rpc,
                message: format!("Failed to fetch transaction: {e}"),
            }),
            Ok(None) => failures.push(FailedTx {
                tx_hash: *hash,
                kind: BatchErrorKind::NotFound,
                message: "Transaction not found".to_string(),
            }),
            Ok(Some(tx)) => match tx.block_number {
                Some(number) => grouped.entry(number).or_default().push(*hash),
                None => failures.push(FailedTx {
                    tx_hash: *hash,
                    kind: BatchErrorKind::Pending,
                    message: "Transaction is pending (no block number)".to_string(),
                }),
            },
        }
    }

    let jobs = grouped
        .into_iter()
        .map(|(number, targets)| BlockJob { number, block: None, targets })
        .collect();
    (jobs, failures)
}

/// Replay one block, reporting an entry for every target it was asked about.
///
/// The block is executed exactly once: every transaction runs in order, and each
/// target's result is recorded before the transaction is committed. Receipts are
/// harvested from the finished block, which is why the block's entries are only
/// produced once the block is done.
async fn replay_block<P>(
    provider: &P,
    chain_id: u64,
    job: BlockJob,
    external_envs: EvmeExternalEnvs,
) -> Vec<BatchEntry>
where
    P: Provider<op_alloy_network::Optimism> + Clone + std::fmt::Debug,
{
    let BlockJob { number, block, targets } = job;

    if number == 0 {
        return fail_all(&targets, BatchErrorKind::Rpc, "Block 0 has no parent block to fork from");
    }

    let block = match block {
        Some(block) => block,
        None => match fetch_block(provider, number).await {
            Ok(block) => block,
            Err(e) => return fail_all(&targets, BatchErrorKind::Rpc, &e.to_string()),
        },
    };
    let parent_block = match fetch_block(provider, number - 1).await {
        Ok(block) => block,
        Err(e) => return fail_all(&targets, BatchErrorKind::Rpc, &e.to_string()),
    };

    let hardforks = get_hardfork_config(chain_id);
    let timestamp = block.header.timestamp();
    let spec = hardforks.spec_id(timestamp);
    let chain_args = ChainArgs { chain_id, spec: spec.to_string() };
    debug!(block = number, chain_id, spec = %spec, "Block configuration");

    let cfg_env = match chain_args.create_cfg_env() {
        Ok(cfg) => cfg,
        Err(e) => return fail_all(&targets, BatchErrorKind::Execution, &e.to_string()),
    };
    let block_env = match retrieve_block_env(&block) {
        Ok(env) => env,
        Err(e) => return fail_all(&targets, BatchErrorKind::Execution, &e.to_string()),
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    let Some(hardfork) = hardforks.hardfork(timestamp) else {
        let message = format!("No `MegaHardfork` active at block timestamp: {timestamp}");
        return fail_all(&targets, BatchErrorKind::Execution, &message);
    };
    let block_limits =
        BlockLimits::from_hardfork_and_block_gas_limit(hardfork, block.header.gas_limit());
    let block_ctx = MegaBlockExecutionCtx::new(
        parent_block.hash(),
        block.header.parent_beacon_block_root(),
        block.header.extra_data().clone(),
        block_limits,
    );

    info!(block = number, fork_block = parent_block.header.number(), "Forking state for block");
    let mut database = match EvmeState::new_forked(
        provider.clone(),
        Some(parent_block.header.number()),
        Default::default(),
        Default::default(),
    )
    .await
    {
        Ok(database) => database,
        Err(e) => return fail_all(&targets, BatchErrorKind::Rpc, &e.to_string()),
    };

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let block_executor_factory =
        MegaBlockExecutorFactory::new(&hardforks, evm_factory, OpAlloyReceiptBuilder::default());
    let mut state = StateBuilder::new().with_database(&mut database).with_bundle_update().build();
    let mut block_executor = block_executor_factory.create_executor(&mut state, block_ctx, evm_env);

    if let Err(e) = block_executor.apply_pre_execution_changes() {
        let message = format!("Block execution error: {e}");
        return fail_all(&targets, BatchErrorKind::Execution, &message);
    }

    let target_set: HashSet<B256> = targets.iter().copied().collect();
    let tx_hashes: Vec<B256> = block.transactions.hashes().collect();
    let mut pending: Vec<PendingTarget> = Vec::new();
    let mut committed = 0usize;

    // Run the block's transactions in order. Any failure aborts the block: the
    // executor state no longer matches the chain, so the remaining targets
    // cannot be replayed faithfully.
    let loop_result: Result<()> = async {
        for (tx_index, tx_hash) in tx_hashes.iter().enumerate() {
            let tx = provider
                .get_transaction_by_hash(*tx_hash)
                .await
                .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
                .ok_or(ReplayError::TransactionNotFound(*tx_hash))?;

            let is_target = target_set.contains(tx_hash);
            let start = Instant::now();
            let pre_execution_nonce = if is_target {
                block_executor
                    .evm()
                    .db_ref()
                    .basic_ref(tx.inner.inner.signer())?
                    .map(|acc| acc.nonce)
                    .unwrap_or(0)
            } else {
                0
            };

            let outcome = block_executor
                .run_transaction(tx.as_recovered())
                .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
            // Record the target's result before committing, mirroring the
            // single-transaction path.
            let exec_result = is_target.then(|| outcome.inner.result.clone());
            let gas_used = block_executor
                .commit_transaction_outcome(outcome)
                .map_err(|e| ReplayError::Other(format!("Block execution error: {e}")))?;
            let commit_index = committed;
            committed += 1;

            if let Some(exec_result) = exec_result {
                pending.push(PendingTarget {
                    tx_hash: *tx_hash,
                    tx_index: tx_index as u64,
                    commit_index,
                    exec_result,
                    exec_time: start.elapsed(),
                    gas_used,
                    pre_execution_nonce,
                    from: tx.inner.inner.signer(),
                    to: tx.inner.inner.to(),
                    effective_gas_price: tx.inner.effective_gas_price.unwrap_or(0),
                });
            }
        }
        Ok(())
    }
    .await;

    // Finish the block even when it aborted midway: targets that already ran
    // still have a receipt worth reporting.
    let mut entries = Vec::with_capacity(targets.len());
    match block_executor.finish() {
        Ok((evm, block_result)) => {
            let (db, _) = evm.finish();
            db.merge_transitions(BundleRetention::Reverts);
            let receipts = block_result.receipts;
            // Receipts are pushed one per committed transaction; index from the
            // end so any receipt produced before the first transaction (now or
            // later) cannot shift the mapping.
            let offset = receipts.len().saturating_sub(committed);
            let block_hash = block.hash();
            for target in pending {
                let Some(envelope) = receipts.get(offset + target.commit_index) else {
                    entries.push(failure(
                        target.tx_hash,
                        BatchErrorKind::Execution,
                        format!("No receipt produced for transaction index {}", target.tx_index),
                    ));
                    continue;
                };
                let contract_address = (target.to.is_none() && envelope.is_success())
                    .then(|| target.from.create(target.pre_execution_nonce));
                let receipt = op_receipt_to_tx_receipt(
                    envelope,
                    number,
                    timestamp,
                    target.from,
                    target.to,
                    contract_address,
                    target.effective_gas_price,
                    target.gas_used,
                    Some(target.tx_hash),
                    Some(block_hash),
                    target.tx_index,
                );
                entries.push(BatchEntry::Executed(Box::new(ExecutedTx {
                    tx_hash: target.tx_hash,
                    block_number: number,
                    tx_index: target.tx_index,
                    exec_result: target.exec_result,
                    contract_address,
                    exec_time: target.exec_time,
                    receipt,
                })));
            }
        }
        Err(e) => {
            let message = format!("Block execution error: {e}");
            for target in pending {
                entries.push(failure(target.tx_hash, BatchErrorKind::Execution, message.clone()));
            }
        }
    }

    // Any target that produced no entry either sat behind the abort or is not
    // part of this block at all.
    let (kind, message) = match loop_result {
        Ok(()) => (BatchErrorKind::NotFound, format!("Transaction is not part of block {number}")),
        Err(e) => {
            warn!(block = number, error = %e, "Aborted block replay; skipping its remaining targets");
            (classify(&e), e.to_string())
        }
    };
    let reported: HashSet<B256> = entries.iter().map(BatchEntry::tx_hash).collect();
    for tx_hash in &targets {
        if !reported.contains(tx_hash) {
            entries.push(failure(*tx_hash, kind, message.clone()));
        }
    }

    entries
}

/// Fetch a block by number, using the same call shape as the single-transaction path.
async fn fetch_block<P>(provider: &P, number: u64) -> Result<Block<Transaction>>
where
    P: Provider<op_alloy_network::Optimism>,
{
    provider
        .get_block_by_number(number.into())
        .await
        .map_err(|e| ReplayError::RpcError(format!("RPC transport error: {e}")))?
        .ok_or(ReplayError::BlockNotFound(number))
}

/// Map an error raised while replaying a block onto a reported error kind.
const fn classify(err: &ReplayError) -> BatchErrorKind {
    match err {
        ReplayError::TransactionNotFound(_) => BatchErrorKind::NotFound,
        ReplayError::RpcError(_) | ReplayError::RpcTransportError(_) => BatchErrorKind::Rpc,
        _ => BatchErrorKind::Execution,
    }
}

/// Build a failure entry.
fn failure(tx_hash: B256, kind: BatchErrorKind, message: String) -> BatchEntry {
    BatchEntry::Failed(FailedTx { tx_hash, kind, message })
}

/// Report the same failure for every target of a block that never started.
fn fail_all(targets: &[B256], kind: BatchErrorKind, message: &str) -> Vec<BatchEntry> {
    targets.iter().map(|hash| failure(*hash, kind, message.to_string())).collect()
}

/// Write one entry to stdout: a compact NDJSON line, or the human-readable
/// summary used by the single-transaction path.
fn emit(entry: &BatchEntry, json: bool) {
    if json {
        let line = match entry {
            BatchEntry::Executed(tx) => {
                let mut summary =
                    ExecutionSummary::from_result(&tx.exec_result, tx.contract_address);
                summary.receipt =
                    Some(serde_json::to_value(&tx.receipt).expect("failed to serialize receipt"));
                serde_json::to_string(&BatchResultLine {
                    tx_hash: tx.tx_hash,
                    block_number: tx.block_number,
                    tx_index: tx.tx_index,
                    summary: &summary,
                })
            }
            BatchEntry::Failed(tx) => serde_json::to_string(&BatchErrorLine {
                tx_hash: tx.tx_hash,
                error: BatchErrorBody { kind: tx.kind.as_str(), message: &tx.message },
            }),
        };
        println!("{}", line.expect("failed to serialize output"));
        return;
    }

    match entry {
        BatchEntry::Executed(tx) => {
            println!();
            println!(
                "=== Transaction {} (block {}, index {}) ===",
                tx.tx_hash, tx.block_number, tx.tx_index
            );
            print_execution_summary(&tx.exec_result, tx.contract_address, tx.exec_time);
            print_receipt(&tx.receipt);
        }
        BatchEntry::Failed(tx) => {
            println!();
            println!("=== Transaction {} ===", tx.tx_hash);
            println!("Error ({}): {}", tx.kind.as_str(), tx.message);
        }
    }
}

/// Parse the newline-separated transaction hash list behind `--tx-file`.
///
/// Blank lines and `#`-prefixed comment lines are ignored. Duplicates are
/// dropped, keeping the first occurrence.
pub(super) fn parse_tx_hash_list(contents: &str) -> Result<Vec<B256>> {
    let mut hashes = Vec::new();
    let mut seen = HashSet::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_number = index + 1;
        let hash = B256::from_str(line).map_err(|e| {
            ReplayError::InvalidInput(format!(
                "invalid transaction hash on line {line_number}: '{line}' ({e})"
            ))
        })?;
        if seen.insert(hash) {
            hashes.push(hash);
        } else {
            warn!(
                tx_hash = %hash,
                line = line_number,
                "Duplicate transaction hash in --tx-file; replaying it once",
            );
        }
    }

    Ok(hashes)
}

/// `clap` value parser for `--block`, accepting decimal or `0x`-prefixed hex.
pub(super) fn parse_block_number(value: &str) -> std::result::Result<u64, String> {
    let trimmed = value.trim();
    match trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16)
            .map_err(|e| format!("invalid hex block number '{value}': {e}")),
        None => trimmed.parse::<u64>().map_err(|e| format!("invalid block number '{value}': {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "0xde3d56dc739484166b8af1bea757bf7e3e9a4b9a0fb62d722703345570dfc1d6";
    const HASH_B: &str = "0x323ddc8e67dfc134284d78c65f3c1dc7ff45ba1db02eeaf62e211ae3253478ef";

    #[test]
    fn test_parse_tx_hash_list_skips_blanks_and_comments() {
        let contents =
            format!("# leading comment\n\n{HASH_A}\n   \n  # indented comment\n\t{HASH_B}  \n\n");

        let hashes = parse_tx_hash_list(&contents).expect("should parse");

        assert_eq!(hashes, vec![B256::from_str(HASH_A).unwrap(), B256::from_str(HASH_B).unwrap()]);
    }

    #[test]
    fn test_parse_tx_hash_list_deduplicates_preserving_order() {
        let contents = format!("{HASH_B}\n{HASH_A}\n{HASH_B}\n");

        let hashes = parse_tx_hash_list(&contents).expect("should parse");

        assert_eq!(hashes, vec![B256::from_str(HASH_B).unwrap(), B256::from_str(HASH_A).unwrap()]);
    }

    #[test]
    fn test_parse_tx_hash_list_reports_offending_line_number() {
        let contents = format!("# comment\n\n{HASH_A}\nnot-a-hash\n");

        let err = parse_tx_hash_list(&contents).expect_err("should reject the invalid hash");

        let message = err.to_string();
        assert!(message.contains("line 4"), "error should name the line, got: {message}");
        assert!(message.contains("not-a-hash"), "error should quote the line, got: {message}");
    }

    #[test]
    fn test_parse_tx_hash_list_accepts_empty_input() {
        let hashes = parse_tx_hash_list("# only a comment\n\n").expect("should parse");
        assert!(hashes.is_empty());
    }

    #[test]
    fn test_parse_block_number_decimal() {
        assert_eq!(parse_block_number("22945844"), Ok(22_945_844));
        assert_eq!(parse_block_number("  22945844 "), Ok(22_945_844));
        assert_eq!(parse_block_number("0"), Ok(0));
    }

    #[test]
    fn test_parse_block_number_hex() {
        assert_eq!(parse_block_number("0x15e2034"), Ok(22_945_844));
        assert_eq!(parse_block_number("0X15E2034"), Ok(22_945_844));
    }

    #[test]
    fn test_parse_block_number_rejects_garbage() {
        assert!(parse_block_number("").is_err());
        assert!(parse_block_number("0x").is_err());
        assert!(parse_block_number("0xzz").is_err());
        assert!(parse_block_number("-1").is_err());
        assert!(parse_block_number("12.5").is_err());
    }
}
