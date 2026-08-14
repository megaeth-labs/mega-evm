//! Mined-block execution kernel shared by the replay drivers.
//!
//! Replaying a mined transaction always means the same thing: fork the parent
//! block's state, walk the block body in order, and stop once every requested
//! target has committed. This module owns that sequence — and nothing else.
//!
//! Everything that decides *which* block, *whether* the endpoint's answers are
//! coherent, and *how* a result is reported stays with the driver
//! ([`super::batch`], and the single-transaction path once it moves here): block
//! and parent fetches, the coherence guards, the on-chain receipt prefetch, the
//! per-target entry assembly and ordering, the error-to-report adaptation, and
//! the decision to publish a fixture. The kernel takes the pieces those
//! decisions produced, executes, and hands back what it observed.
//!
//! The driver reaches into the middle of the run through [`TargetHook`], which
//! is called for every target after it executed and before it is committed —
//! the only moment at which the pre-commit database state is still observable.
//! Its return value is opaque to the kernel: it is carried through the block's
//! `finish()` and handed back alongside that target's receipt.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use alloy_consensus::Transaction as _;
use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use mega_evm::{
    alloy_evm::{block::BlockExecutor, Evm, EvmEnv},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{result::ExecutionResult, ContextTr},
        database::{states::bundle_state::BundleRetention, StateBuilder},
        state::EvmState,
        DatabaseRef,
    },
    MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaEvmFactory, MegaHaltReason, MegaHardforks,
    MegaSpecId,
};
use op_alloy_rpc_types::Transaction;
use tracing::info;

use crate::{
    common::{op_receipt_to_tx_receipt, EvmeExternalEnvs, OpTxReceipt},
    EvmeState,
};

use super::{verify, ReplayError, Result};

/// Identity of the block being replayed, as stamped onto harvested receipts.
///
/// The kernel addresses state by number (the fork) and reports by hash (the
/// receipt), so both are carried explicitly rather than re-derived.
#[derive(Debug, Clone, Copy)]
pub(super) struct BlockIdentity {
    /// Number of the block whose transactions are executed.
    pub(super) number: u64,
    /// Header timestamp, stamped onto every harvested receipt and its logs.
    pub(super) timestamp: u64,
    /// Hash of the block, stamped onto every harvested receipt and its logs.
    pub(super) hash: B256,
}

/// Everything the kernel needs to fork the parent state and run one mined block.
///
/// The environment pieces (`block_ctx`, `evm_env`, `hardforks`) are built by the
/// driver from the block header it fetched and validated: the kernel does not
/// re-read the header, so a driver that wants a what-if world (a forced spec,
/// for one) only has to hand over a different environment.
pub(super) struct MinedBlockRun<'a, H> {
    /// Hardfork schedule the block executes under.
    pub(super) hardforks: H,
    /// `MegaETH` external environment (SALT buckets, oracle) for the EVM factory.
    pub(super) external_envs: EvmeExternalEnvs,
    /// Block-level execution context: parent hash, beacon root, extra data, limits.
    pub(super) block_ctx: MegaBlockExecutionCtx,
    /// Config and block environment every transaction executes under.
    pub(super) evm_env: EvmEnv<MegaSpecId>,
    /// Number of the parent block the state is forked from.
    pub(super) fork_block: u64,
    /// Identity stamped onto the harvested receipts.
    pub(super) identity: BlockIdentity,
    /// The block body, in body order: every transaction the kernel may execute.
    pub(super) tx_hashes: &'a [B256],
    /// Hashes whose results the driver wants reported. The kernel stops once the
    /// last of them has committed.
    pub(super) targets: &'a HashSet<B256>,
}

/// A failure that stopped the block before any transaction ran.
///
/// The two arms are kept apart because they are not the same kind of failure:
/// forking is an endpoint question, while pre-execution changes are the
/// executor rejecting the block. The driver decides how each is reported.
pub(super) enum SetupError {
    /// Forking the parent block's state failed.
    Fork(ReplayError),
    /// The executor's pre-execution changes failed.
    PreExecution(ReplayError),
}

/// Driver work that runs while the block is mid-flight.
///
/// Called once per target, after it executed and before it is committed — the
/// only point at which the database still reflects the pre-target state while
/// the target's own outcome is already known. Anything the driver wants to carry
/// from there to the end of the block travels as [`Self::Draft`], which the
/// kernel never inspects.
pub(super) trait TargetHook {
    /// Value the driver carries from the pre-commit point to the harvest.
    type Draft;

    /// Observe one target between its execution and its commit.
    fn on_target_executed<DB>(&mut self, target: TargetExecution<'_, DB>) -> Self::Draft
    where
        DB: DatabaseRef,
        DB::Error: core::fmt::Display;
}

/// One target's execution, observed before it is committed.
pub(super) struct TargetExecution<'a, DB> {
    /// Database as of the preceding transactions, with this target uncommitted.
    pub(super) db: &'a DB,
    /// Hash the block body listed this transaction under.
    pub(super) tx_hash: B256,
    /// The transaction that just executed.
    pub(super) tx: &'a Transaction,
    /// How many block hashes this transaction read (the record is cleared before
    /// every transaction, so the count is this target's own).
    pub(super) accessed_block_hash_count: usize,
    /// What the execution produced.
    pub(super) exec_result: &'a ExecutionResult<MegaHaltReason>,
    /// State diff the execution produced, not yet committed.
    pub(super) evm_state: &'a EvmState,
}

/// What one kernel run observed.
pub(super) struct BlockRun<D> {
    /// `Err` when the transaction loop aborted before reaching the last target.
    /// The block is still finished, so targets that already ran keep a receipt.
    pub(super) loop_result: Result<()>,
    /// Hash of the transaction whose iteration was in flight when the loop
    /// ended. It is the attribution ground truth for an abort: some rejections
    /// raised *about* a transaction do not embed its hash in the error.
    /// Meaningful only alongside a failed [`Self::loop_result`].
    pub(super) in_flight: Option<B256>,
    /// What `finish()` produced.
    pub(super) finish: FinishOutcome<D>,
}

/// The block's terminal state.
pub(super) enum FinishOutcome<D> {
    /// The block finished. One harvest per target that committed, in commit
    /// order.
    Harvested(Vec<TargetHarvest<D>>),
    /// `finish()` failed, so no target of the block has a receipt and every
    /// draft is dropped unpublished.
    Failed {
        /// Why the block could not be finished.
        error: ReplayError,
        /// Targets that had executed, in commit order.
        executed: Vec<B256>,
    },
}

/// One target's share of a finished block.
pub(super) enum TargetHarvest<D> {
    /// The target and the receipt the finished block produced for it.
    Receipt(Box<HarvestedTarget<D>>),
    /// The finished block produced no receipt at this target's commit position.
    MissingReceipt {
        /// Hash of the target left without a receipt.
        tx_hash: B256,
        /// Index of the target in the block body.
        tx_index: u64,
    },
}

/// A target that executed, committed, and was paired with its receipt.
pub(super) struct HarvestedTarget<D> {
    /// Hash of the target.
    pub(super) tx_hash: B256,
    /// Index of the target in the block body.
    pub(super) tx_index: u64,
    /// What the execution produced.
    pub(super) exec_result: ExecutionResult<MegaHaltReason>,
    /// Wall-clock time the execution took.
    pub(super) exec_time: Duration,
    /// Address a successful contract creation deployed to.
    pub(super) contract_address: Option<Address>,
    /// Receipt built from the block's own receipt for this target.
    pub(super) receipt: OpTxReceipt,
    /// Whatever the driver's [`TargetHook`] produced for this target.
    pub(super) draft: D,
}

/// A target that executed, awaiting the receipt harvested by `finish()`.
struct PendingTarget<D> {
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
    /// Whatever the driver's [`TargetHook`] produced for this target.
    draft: D,
}

/// Fork the parent state, execute the block body until every target has
/// committed, and harvest the targets' receipts.
///
/// Every transaction of the body runs in order — a target's result is only
/// faithful if the state it starts from is. Each target's result is recorded
/// before its outcome is committed, and the walk stops once the last target has
/// committed: trailing non-targets contribute nothing to this run and requiring
/// them would make an incomplete offline capture fail after a successful target.
///
/// Any failure inside the loop aborts the body — the executor's state no longer
/// matches the chain — but the block is still finished, so targets that already
/// ran keep the receipt they earned. `Err` is reserved for a failure that
/// stopped the block before any transaction ran.
pub(super) async fn execute_until_targets<P, H, K>(
    provider: &P,
    run: MinedBlockRun<'_, H>,
    hook: &mut K,
) -> std::result::Result<BlockRun<K::Draft>, SetupError>
where
    P: Provider<op_alloy_network::Optimism> + Clone + core::fmt::Debug,
    H: MegaHardforks + Clone,
    K: TargetHook,
{
    let MinedBlockRun {
        hardforks,
        external_envs,
        block_ctx,
        evm_env,
        fork_block,
        identity,
        tx_hashes,
        targets,
    } = run;

    info!(block = identity.number, fork_block, "Forking state for block");
    let mut database = EvmeState::new_forked(
        provider.clone(),
        Some(fork_block),
        Default::default(),
        Default::default(),
    )
    .await
    .map_err(SetupError::Fork)?;

    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let block_executor_factory =
        MegaBlockExecutorFactory::new(hardforks, evm_factory, OpAlloyReceiptBuilder::default());
    let mut state = StateBuilder::new().with_database(&mut database).with_bundle_update().build();
    let mut block_executor = block_executor_factory.create_executor(&mut state, block_ctx, evm_env);

    if let Err(e) = block_executor.apply_pre_execution_changes() {
        return Err(SetupError::PreExecution(ReplayError::BlockExecutionError(e)));
    }

    // Highest block index among the requested targets: once that transaction has
    // committed we can stop — later non-targets are not needed for receipts or
    // fixtures, and requiring them would force incomplete offline captures to
    // abort after a successful dump target.
    let last_target_index = tx_hashes
        .iter()
        .enumerate()
        .filter(|(_, hash)| targets.contains(*hash))
        .map(|(i, _)| i)
        .max();
    let mut pending: Vec<PendingTarget<K::Draft>> = Vec::new();
    let mut committed = 0usize;

    // Run the block's transactions in order. Any failure aborts the block: the
    // executor state no longer matches the chain, so the remaining targets
    // cannot be replayed faithfully.
    //
    // `in_flight` names the transaction whose iteration raised the abort. It is
    // the attribution ground truth: some rejections raised *about* a
    // transaction do not embed its hash in the error (the block-gas admission
    // check, for one), and attributing from error introspection alone would
    // sweep the aborter itself as an unanswered peer.
    let mut in_flight: Option<B256> = None;
    let loop_result: Result<()> = async {
        for (tx_index, tx_hash) in tx_hashes.iter().enumerate() {
            in_flight = Some(*tx_hash);
            // Isolate BLOCKHASH reads per transaction so a fixture dump sees only
            // the target's own accesses (mirrors the single-tx clear after
            // preceding transactions).
            block_executor.clear_accessed_block_hashes();

            // Every hash here came from the block body this endpoint already
            // served. `Ok(None)` therefore means the endpoint is inconsistent
            // (reorg or load-balanced divergent views), not that the hash is
            // unknown — that definitive answer only applies to a user-supplied
            // target lookup on the single-transaction path.
            let tx = provider
                .get_transaction_by_hash(*tx_hash)
                .await
                .map_err(|e| ReplayError::BlockBodyTransactionFetch {
                    tx_hash: *tx_hash,
                    message: e.to_string(),
                })?
                .ok_or(ReplayError::BlockBodyTransactionNull(*tx_hash))?;
            // A served object that fails authentication is the same class as a
            // null answer on a body-listed hash: the endpoint failed to deliver
            // a transaction it claimed to include. Executing it instead would
            // advance the block state on the wrong transaction, or report
            // another transaction's outcome under a target hash.
            verify::authenticate_transaction(&tx, *tx_hash).map_err(|message| {
                ReplayError::BlockBodyTransactionFetch { tx_hash: *tx_hash, message }
            })?;

            let is_target = targets.contains(tx_hash);
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
                .map_err(ReplayError::BlockExecutionError)?;

            // The driver's hook runs before commit: the database is the state
            // after the preceding transactions, with this target's own result
            // still uncommitted. Its draft and the target's result are taken
            // together so a target either contributes both or neither.
            let observed = is_target.then(|| {
                let accessed_block_hashes = block_executor.get_accessed_block_hashes();
                let draft = hook.on_target_executed(TargetExecution {
                    db: block_executor.evm().db_ref(),
                    tx_hash: *tx_hash,
                    tx: &tx,
                    accessed_block_hash_count: accessed_block_hashes.len(),
                    exec_result: &outcome.inner.result,
                    evm_state: &outcome.inner.state,
                });
                (draft, outcome.inner.result.clone())
            });

            let gas_used = block_executor
                .commit_transaction_outcome(outcome)
                .map_err(ReplayError::BlockExecutionError)?;
            let commit_index = committed;
            committed += 1;

            if let Some((draft, exec_result)) = observed {
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
                    draft,
                });
            }

            // Stop once every requested target that can run has committed: trailing
            // non-targets are irrelevant to this job's receipts and fixtures.
            if Some(tx_index) == last_target_index {
                break;
            }
        }
        Ok(())
    }
    .await;

    // Finish the block even when it aborted midway: targets that already ran
    // still have a receipt worth reporting.
    let finish = match block_executor.finish() {
        Ok((evm, block_result)) => {
            let (db, _) = evm.finish();
            db.merge_transitions(BundleRetention::Reverts);
            let receipts = block_result.receipts;
            // Receipts are pushed one per committed transaction; index from the
            // end so any receipt produced before the first transaction (now or
            // later) cannot shift the mapping.
            let offset = receipts.len().saturating_sub(committed);
            let mut harvested = Vec::with_capacity(pending.len());
            for target in pending {
                let Some(envelope) = receipts.get(offset + target.commit_index) else {
                    harvested.push(TargetHarvest::MissingReceipt {
                        tx_hash: target.tx_hash,
                        tx_index: target.tx_index,
                    });
                    continue;
                };
                let contract_address = (target.to.is_none() && envelope.is_success())
                    .then(|| target.from.create(target.pre_execution_nonce));
                // Block-global log index: cumulative log count of all committed
                // receipts that precede this target in the block.
                let first_log_index: u64 = receipts[offset..offset + target.commit_index]
                    .iter()
                    .map(|r| r.logs().len() as u64)
                    .sum();
                let receipt = op_receipt_to_tx_receipt(
                    envelope,
                    identity.number,
                    identity.timestamp,
                    target.from,
                    target.to,
                    contract_address,
                    target.effective_gas_price,
                    target.gas_used,
                    Some(target.tx_hash),
                    Some(identity.hash),
                    target.tx_index,
                    first_log_index,
                );
                harvested.push(TargetHarvest::Receipt(Box::new(HarvestedTarget {
                    tx_hash: target.tx_hash,
                    tx_index: target.tx_index,
                    exec_result: target.exec_result,
                    exec_time: target.exec_time,
                    contract_address,
                    receipt,
                    draft: target.draft,
                })));
            }
            FinishOutcome::Harvested(harvested)
        }
        // The block itself failed to finish, so no target of it has a receipt
        // and every draft is dropped without being published.
        Err(e) => FinishOutcome::Failed {
            error: ReplayError::BlockExecutionError(e),
            executed: pending.into_iter().map(|target| target.tx_hash).collect(),
        },
    };

    Ok(BlockRun { loop_result, in_flight, finish })
}
