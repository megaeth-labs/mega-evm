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
//! # Lifecycle
//!
//! A driver reaches into the run through [`TargetLifecycle`], which the kernel
//! calls at exactly three moments, plus one it cannot call at all:
//!
//! 1. **Construction** — [`TargetLifecycle::Inspector`] is the inspector the block executor is
//!    built with, and [`TargetLifecycle::INSPECT`] says whether execution routes through it. A
//!    driver that only wants receipts keeps the plain execution path; a driver that wants a trace
//!    arms one.
//! 2. **Before the target executes** — [`TargetLifecycle::before_target`] turns the transaction the
//!    endpoint served into the transaction that actually runs. The identity is the mined
//!    transaction; a driver that applies overrides returns its own wrapper, and the target's
//!    pre-execution nonce is then read for the *returned* transaction's signer.
//! 3. **After it executed, before it commits** — [`TargetLifecycle::on_target_executed`] is the
//!    only moment at which the pre-target database state is still observable while the target's own
//!    outcome is already known. What it produces ([`TargetLifecycle::Draft`]) is opaque to the
//!    kernel.
//! 4. **After the block finished** — the kernel does *not* call back. A draft comes out of the run
//!    wrapped in a [`PendingDraft`], and taking it out needs the [`CleanRun`] proof that the walk
//!    completed. Since a block that fails to finish drops every draft inside the kernel, holding
//!    both a draft and the proof means the block ran to a clean finish — which is the condition a
//!    driver must not publish an artifact without.
//!
//! Either hook may fail. A failure aborts the block body exactly like a failed
//! fetch or a rejected transaction: the walk stops, the block is still finished,
//! and the abort is attributed to the target the driver was working on.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use alloy_consensus::Transaction as _;
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use mega_evm::{
    alloy_evm::{block::BlockExecutor, Evm, EvmEnv, IntoTxEnv, RecoveredTx},
    alloy_op_evm::block::OpAlloyReceiptBuilder,
    revm::{
        context::{
            result::{ExecutionResult, ResultAndState},
            ContextTr,
        },
        database::{states::bundle_state::BundleRetention, State, StateBuilder},
        DatabaseRef, Inspector,
    },
    MegaBlockExecutionCtx, MegaBlockExecutorFactory, MegaContext, MegaEvmFactory, MegaHaltReason,
    MegaHardforks, MegaSpecId, MegaTransaction, MegaTransactionExt, MegaTxEnvelope,
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
pub(super) struct MinedBlockRun<'a, H, I> {
    /// Hardfork schedule the block executes under.
    pub(super) hardforks: H,
    /// `MegaETH` external environment (SALT buckets, oracle) for the EVM factory.
    pub(super) external_envs: EvmeExternalEnvs,
    /// Block-level execution context: parent hash, beacon root, extra data, limits.
    pub(super) block_ctx: MegaBlockExecutionCtx,
    /// Config and block environment every transaction executes under.
    pub(super) evm_env: EvmEnv<MegaSpecId>,
    /// Inspector the block executor is built with. It observes execution only
    /// when the driver's [`TargetLifecycle::INSPECT`] says so, and is handed
    /// back to the driver at both lifecycle points.
    pub(super) inspector: I,
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

/// The driver's typed participation in one mined block.
///
/// Every member exists because a driver needs it at a specific moment of the
/// run; see the module documentation for the moments themselves.
pub(super) trait TargetLifecycle {
    /// Inspector the block executor is built with.
    ///
    /// It is not the kernel's: the driver owns it, hands it over for the
    /// duration of the block, and reads it back at both hooks. A driver with
    /// nothing to inspect uses `NoOpInspector`.
    type Inspector;

    /// Whether execution runs through [`Self::Inspector`].
    ///
    /// The EVM has two execution paths, one that consults an inspector at every
    /// step and one that does not. A driver that installed a real inspector must
    /// set this; a driver that only holds a `NoOpInspector` place-holder must not,
    /// so it keeps the plain path it would have had with no inspector at all.
    const INSPECT: bool;

    /// The form in which a target transaction actually executes.
    ///
    /// The identity is the mined transaction (`Recovered<&MegaTxEnvelope>`); a
    /// driver that rewrites gas, value or input returns its own wrapper around
    /// it. The transaction borrows the endpoint's answer, hence the lifetime.
    type Tx<'a>: IntoTxEnv<MegaTransaction>
        + RecoveredTx<MegaTxEnvelope>
        + MegaTransactionExt
        + Encodable2718
        + Copy;

    /// Value the driver carries from the pre-commit point to the harvest.
    type Draft;

    /// Turn the target the endpoint served into the transaction that runs.
    ///
    /// Called once per target, after the block body's answer has been
    /// authenticated and before anything is read for it: the returned
    /// transaction is what executes, and its signer is whose nonce is read for
    /// the created-contract address. Non-target transactions of the body are
    /// never routed through here — they always execute as they were mined.
    ///
    /// `inspector` is handed over before the target runs through it, which is
    /// the one moment a driver can arm an inspector for the target alone. The
    /// borrow ends with the call, so the returned transaction is free to borrow
    /// `tx` for as long as the kernel needs it.
    fn before_target<'tx>(
        &mut self,
        tx: &'tx Transaction,
        inspector: &mut Self::Inspector,
    ) -> Result<Self::Tx<'tx>>;

    /// Observe one target between its execution and its commit.
    ///
    /// `inspector` is what the target's own execution left behind — it is
    /// separate from [`TargetExecution`] because it is the driver's own object
    /// coming back, not something the kernel observed.
    fn on_target_executed<DB>(
        &mut self,
        target: TargetExecution<'_, DB>,
        inspector: &Self::Inspector,
    ) -> Result<Self::Draft>
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
    /// The transaction that just executed, as the endpoint served it.
    pub(super) tx: &'a Transaction,
    /// How many block hashes this transaction read (the record is cleared before
    /// every transaction, so the count is this target's own).
    pub(super) accessed_block_hash_count: usize,
    /// What the execution produced: its result and the state diff it would
    /// commit, still uncommitted.
    pub(super) result_and_state: &'a ResultAndState<MegaHaltReason>,
}

/// What one kernel run observed.
pub(super) struct BlockRun<D> {
    /// How the transaction loop ended.
    pub(super) loop_outcome: LoopOutcome,
    /// What `finish()` produced.
    pub(super) finish: FinishOutcome<D>,
}

/// How the walk over the block body ended.
pub(super) enum LoopOutcome {
    /// The walk ran to its end: every target that the body holds committed.
    /// The proof it carries is what unlocks the drafts of that run.
    Completed(CleanRun),
    /// The walk stopped early, so the executor's state no longer matches the
    /// chain and the remaining targets were not replayed.
    Aborted {
        /// Why the walk stopped.
        error: ReplayError,
        /// The transaction whose iteration raised it. This is attribution by
        /// construction rather than by error introspection: some rejections
        /// raised *about* a transaction do not embed its hash in the error (the
        /// block-gas admission check, for one), and an error that does name a
        /// hash may name a different transaction than the one being walked.
        tx_hash: B256,
    },
}

/// Proof that the block's transaction loop completed.
///
/// Only the kernel can mint one, and only when the walk ended on its own terms.
/// It is what [`PendingDraft::redeem`] asks for, which is why a draft cannot be
/// taken out of a run that aborted midway.
pub(super) struct CleanRun(());

/// A draft the driver built at the pre-commit point, held until the block ends.
///
/// Its whole purpose is that the value inside cannot be *moved out* without a
/// [`CleanRun`]: publishing an artifact consumes the draft that describes it, so
/// a driver physically cannot publish behind the kernel's back. Reading the
/// draft ([`Self::peek`]) stays open, because a discarded draft still has to be
/// reported on.
///
/// Drafts only exist on the harvested path — a block that fails to finish drops
/// them inside the kernel — so a redeemed draft is one from a block that both
/// walked and finished cleanly.
pub(super) struct PendingDraft<D>(D);

impl<D> PendingDraft<D> {
    /// Read the draft without taking it.
    ///
    /// This is the view a driver has when the block aborted: enough to report
    /// what was discarded, not enough to publish it.
    pub(super) const fn peek(&self) -> &D {
        &self.0
    }

    /// Take the draft out, against the proof that the run completed.
    pub(super) fn redeem(self, _proof: &CleanRun) -> D {
        self.0
    }
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
    /// Whatever the driver's [`TargetLifecycle`] produced for this target,
    /// redeemable only against the run's [`CleanRun`] proof.
    pub(super) draft: PendingDraft<D>,
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
    /// Whatever the driver's [`TargetLifecycle`] produced for this target.
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
///
/// The inspector bound is quantified over the executor's borrow of the state,
/// which only exists inside this function — hence the `for<'state>` and the
/// `'static` provider. Both are load-bearing for the shape of the state: the
/// forked database is *moved* into [`State`] rather than borrowed, because a
/// second `&mut` layer under the quantifier is more than the compiler can prove
/// a real inspector satisfies. A driver whose inspector is a plain
/// `NoOpInspector` never exercises any of this; one that installs a tracer does.
pub(super) async fn execute_until_targets<P, H, K>(
    provider: &P,
    run: MinedBlockRun<'_, H, K::Inspector>,
    lifecycle: &mut K,
) -> std::result::Result<BlockRun<K::Draft>, SetupError>
where
    P: Provider<op_alloy_network::Optimism> + Clone + core::fmt::Debug + 'static,
    H: MegaHardforks + Clone,
    K: TargetLifecycle,
    K::Inspector: for<'state> Inspector<
        MegaContext<&'state mut State<EvmeState<op_alloy_network::Optimism, P>>, EvmeExternalEnvs>,
    >,
{
    let MinedBlockRun {
        hardforks,
        external_envs,
        block_ctx,
        evm_env,
        inspector,
        fork_block,
        identity,
        tx_hashes,
        targets,
    } = run;

    info!(block = identity.number, fork_block, "Forking state for block");
    let database = EvmeState::new_forked(
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
    let mut state = StateBuilder::new().with_database(database).with_bundle_update().build();
    let mut block_executor = block_executor_factory
        .create_executor_with_inspector(&mut state, block_ctx, evm_env, inspector);
    // The executor is always built holding the driver's inspector, but only a
    // driver that has something to inspect pays for the inspected execution
    // path. Set before the pre-execution changes so the whole block, system
    // calls included, runs the way the driver asked for.
    block_executor.evm_mut().set_inspector_enabled(K::INSPECT);

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
    // cannot be replayed faithfully. The abort is recorded here, inside the
    // iteration that raised it, so the transaction it is attributed to is the
    // one being walked rather than one recovered from the error afterwards.
    let mut aborted: Option<(B256, ReplayError)> = None;
    for (tx_index, tx_hash) in tx_hashes.iter().enumerate() {
        let step: Result<()> = async {
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

            if !targets.contains(tx_hash) {
                // Not reported on, so it only has to move the state the way the
                // chain did: it executes exactly as it was mined.
                let outcome = block_executor
                    .run_transaction(tx.as_recovered())
                    .map_err(ReplayError::BlockExecutionError)?;
                block_executor
                    .commit_transaction_outcome(outcome)
                    .map_err(ReplayError::BlockExecutionError)?;
                committed += 1;
                return Ok(());
            }

            let start = Instant::now();
            // The driver decides what this target actually runs as, and the
            // nonce for its created-contract address is read for whoever signs
            // the transaction it handed back.
            let prepared = lifecycle.before_target(&tx, block_executor.inspector_mut())?;
            let pre_execution_nonce = block_executor
                .evm()
                .db_ref()
                .basic_ref(*RecoveredTx::signer(&prepared))?
                .map(|acc| acc.nonce)
                .unwrap_or(0);

            let outcome = block_executor
                .run_transaction(prepared)
                .map_err(ReplayError::BlockExecutionError)?;

            // The driver's hook runs before commit: the database is the state
            // after the preceding transactions, with this target's own result
            // still uncommitted. Its draft and the target's result are taken
            // together so a target either contributes both or neither.
            let accessed_block_hashes = block_executor.get_accessed_block_hashes();
            let draft = lifecycle.on_target_executed(
                TargetExecution {
                    db: block_executor.evm().db_ref(),
                    tx_hash: *tx_hash,
                    tx: &tx,
                    accessed_block_hash_count: accessed_block_hashes.len(),
                    result_and_state: &outcome.inner.result_and_state,
                },
                block_executor.inspector(),
            )?;
            let exec_result = outcome.inner.result.clone();

            let gas_used = block_executor
                .commit_transaction_outcome(outcome)
                .map_err(ReplayError::BlockExecutionError)?;
            let commit_index = committed;
            committed += 1;

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
            Ok(())
        }
        .await;

        if let Err(error) = step {
            aborted = Some((*tx_hash, error));
            break;
        }
        // Stop once every requested target that can run has committed: trailing
        // non-targets are irrelevant to this job's receipts and fixtures.
        if Some(tx_index) == last_target_index {
            break;
        }
    }

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
                    draft: PendingDraft(target.draft),
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

    let loop_outcome = match aborted {
        Some((tx_hash, error)) => LoopOutcome::Aborted { error, tx_hash },
        None => LoopOutcome::Completed(CleanRun(())),
    };

    Ok(BlockRun { loop_outcome, finish })
}
