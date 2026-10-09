//! Block execution kernel shared by the replay drivers.
//!
//! Replaying a mined transaction always means the same thing: fork the parent
//! block's state, walk the block body in order, and stop once every requested
//! target has committed. This module owns that sequence — and nothing else.
//!
//! Everything that decides *which* block, *whether* the endpoint's answers are
//! coherent, and *how* a result is reported stays with the driver
//! ([`super::batch`] and [`super::cmd`]): block and parent fetches, the
//! coherence guards, the on-chain receipt prefetch, the per-target entry
//! assembly and ordering, the error-to-report adaptation, and the decision to
//! publish a fixture. The kernel takes the pieces those decisions produced,
//! executes, and hands back what it observed.
//!
//! A *pending* transaction runs through here too, as a one-transaction body on
//! top of the latest block, which fills both the fork and the environment role.
//! Its driver hands the target over as a [`BodyEntry::Served`] transaction
//! rather than a listed hash: the target's pending metadata is exactly what the
//! online cache refuses to keep, so looking it up again would ask the endpoint a
//! second time.
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
//! A run that executed and committed every transaction of the block body and
//! finished it also hands back the block itself ([`WholeBlock`]): the body it
//! executed and what the block executor produced for it. A driver that compares
//! the replayed block against the block the chain sealed reads it there; a run
//! that stopped anywhere short of the whole body has none.
//!
//! Either hook may fail. A failure aborts the block body exactly like a failed
//! fetch or a rejected transaction: the walk stops, the block is still finished,
//! and the abort is attributed to the target the driver was working on.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use alloy_consensus::Transaction as _;
use alloy_eips::{eip7685::Requests, Encodable2718};
use alloy_primitives::{Address, Bytes, B256};
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
use op_alloy_consensus::OpReceiptEnvelope;
use op_alloy_rpc_types::Transaction;
use tracing::info;

use crate::{
    common::{
        create_address, op_receipt_to_tx_receipt, pre_execution_nonce, EvmeExternalEnvs,
        OpTxReceipt,
    },
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

/// One transaction of the body the kernel walks.
///
/// Every entry is authenticated against its hash before it executes; the two
/// forms differ only in where the transaction comes from.
#[derive(Debug, Clone, Copy)]
pub(super) enum BodyEntry<'a> {
    /// A hash the block body listed. The kernel fetches the transaction from
    /// the endpoint.
    Listed(B256),
    /// A transaction the driver already fetched, under the hash it was
    /// requested by. The kernel executes it without asking the endpoint again.
    Served {
        /// Hash the transaction was requested by.
        tx_hash: B256,
        /// The transaction the endpoint served for it.
        tx: &'a Transaction,
    },
}

impl BodyEntry<'_> {
    /// Hash the entry stands for, which targets, reports and aborts name.
    pub(super) const fn tx_hash(&self) -> B256 {
        match self {
            Self::Listed(tx_hash) | Self::Served { tx_hash, .. } => *tx_hash,
        }
    }
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
    /// Number of the block the state is forked from: a mined block's parent, or
    /// the latest block a pending target runs on top of.
    pub(super) fork_block: u64,
    /// Identity stamped onto the harvested receipts.
    pub(super) identity: BlockIdentity,
    /// The block body, in body order: every transaction the kernel may execute.
    ///
    /// A driver may hand over only the body up to its last target, since the
    /// walk never goes past it; [`Self::body_len`] still states the whole
    /// body's length.
    pub(super) body: &'a [BodyEntry<'a>],
    /// How many transactions the block body lists. A run produces a
    /// [`WholeBlock`] only when it committed this many, which a walk over a
    /// prefix of the body never does.
    pub(super) body_len: usize,
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
    /// The block finished.
    Harvested {
        /// One harvest per target that committed, in commit order.
        targets: Vec<TargetHarvest<D>>,
        /// The block the run executed, present iff the walk completed without
        /// an abort and committed every transaction of the body: only then is
        /// the finished block the one the body describes.
        whole_block: Option<WholeBlock>,
    },
    /// `finish()` failed, so no target of the block has a receipt and every
    /// draft is dropped unpublished.
    Failed {
        /// Why the block could not be finished.
        error: ReplayError,
        /// Targets that had executed, in commit order.
        executed: Vec<B256>,
    },
}

/// A block the run executed in full, as the block executor finished it.
///
/// Carries what a block header commits to about execution, in the raw form the
/// executor produced it, so a driver can rebuild those commitments without the
/// kernel deciding which of them matter.
pub(super) struct WholeBlock {
    /// EIP-2718 encodings of the body's transactions, in body order. Each is
    /// the encoding the transaction authenticated against its body-listed hash.
    pub(super) transactions: Vec<Bytes>,
    /// The receipts the block executor produced, one per transaction, in body
    /// order.
    pub(super) receipts: Vec<OpReceiptEnvelope>,
    /// Gas the block used, as the block executor accounted it.
    pub(super) gas_used: u64,
    /// Blob gas the block used, as the block executor accounted it.
    pub(super) blob_gas_used: u64,
    /// EIP-7685 requests the block executor produced.
    pub(super) requests: Requests,
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
    /// Nonce the target's signer held before it executed — the one the
    /// receipt's `contractAddress` was derived from, reported so a driver does
    /// not have to read it back from a database the target has since committed
    /// to.
    pub(super) pre_execution_nonce: u64,
    /// Receipt built from the block's own receipt for this target. Its
    /// `contractAddress` is the target's [`create_address`], set for a failed
    /// creation too.
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
        body,
        body_len,
        targets,
    } = run;

    // The receipts report the price each transaction paid, which its signed fee
    // fields and this block's base fee decide; read before the environment moves
    // into the executor.
    let base_fee = evm_env.block_env.basefee;

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
    let last_target_index = body
        .iter()
        .enumerate()
        .filter(|(_, entry)| targets.contains(&entry.tx_hash()))
        .map(|(i, _)| i)
        .max();
    let mut pending: Vec<PendingTarget<K::Draft>> = Vec::new();
    let mut committed = 0usize;
    // The authenticated EIP-2718 encoding of every transaction walked, in body
    // order, for the [`WholeBlock`] a complete run hands back.
    let mut transactions: Vec<Bytes> = Vec::with_capacity(body.len());

    // Run the block's transactions in order. Any failure aborts the block: the
    // executor state no longer matches the chain, so the remaining targets
    // cannot be replayed faithfully. The abort is recorded here, inside the
    // iteration that raised it, so the transaction it is attributed to is the
    // one being walked rather than one recovered from the error afterwards.
    let mut aborted: Option<(B256, ReplayError)> = None;
    for (tx_index, entry) in body.iter().enumerate() {
        let tx_hash = entry.tx_hash();
        let step: Result<()> = async {
            // Isolate BLOCKHASH reads per transaction so a fixture dump sees only
            // the target's own accesses.
            //
            // Invariant: the record is cleared immediately before every
            // transaction, and the only thing ever read from it is a target's
            // own count, at [`TargetLifecycle::on_target_executed`]. Clearing
            // per transaction is therefore indistinguishable from clearing once
            // just before the target — the reads of the transactions in between
            // are discarded either way, and no reader exists between two clears.
            // That equivalence is what let the single-transaction path, which
            // cleared once after its preceding transactions, move onto this
            // rhythm without changing what a dump refuses.
            block_executor.clear_accessed_block_hashes();

            let fetched;
            let tx = match entry {
                // Every hash here came from the block body this endpoint
                // already served. `Ok(None)` therefore means the endpoint is
                // inconsistent (reorg or load-balanced divergent views), not
                // that the hash is unknown — that definitive answer only
                // applies to a user-supplied target lookup on the
                // single-transaction path.
                BodyEntry::Listed(_) => {
                    fetched = provider
                        .get_transaction_by_hash(tx_hash)
                        .await
                        .map_err(|e| ReplayError::BlockBodyTransactionFetch {
                            tx_hash,
                            message: e.to_string(),
                        })?
                        .ok_or(ReplayError::BlockBodyTransactionNull(tx_hash))?;
                    &fetched
                }
                BodyEntry::Served { tx, .. } => *tx,
            };
            // A served object that fails authentication is the same class as a
            // null answer on a body-listed hash: the endpoint failed to deliver
            // a transaction it claimed to include. Executing it instead would
            // advance the block state on the wrong transaction, or report
            // another transaction's outcome under a target hash. A transaction
            // the driver already holds is checked too — the check is local, and
            // it keeps every transaction this kernel executes authenticated
            // against the hash it reports under.
            let encoded = verify::authenticate_transaction(tx, tx_hash)
                .map_err(|message| ReplayError::BlockBodyTransactionFetch { tx_hash, message })?;
            transactions.push(encoded);

            if !targets.contains(&tx_hash) {
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
            let prepared = lifecycle.before_target(tx, block_executor.inspector_mut())?;
            let pre_execution_nonce = pre_execution_nonce(
                block_executor.evm().db_ref(),
                *RecoveredTx::signer(&prepared),
            )?;

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
                    tx_hash,
                    tx,
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
                tx_hash,
                tx_index: tx_index as u64,
                commit_index,
                exec_result,
                exec_time: start.elapsed(),
                gas_used,
                pre_execution_nonce,
                from: tx.inner.inner.signer(),
                to: tx.inner.inner.to(),
                effective_gas_price: tx.inner.inner.effective_gas_price(Some(base_fee)),
                draft,
            });
            Ok(())
        }
        .await;

        if let Err(error) = step {
            aborted = Some((tx_hash, error));
            break;
        }
        // Stop once every requested target that can run has committed: trailing
        // non-targets are irrelevant to this job's receipts and fixtures.
        if Some(tx_index) == last_target_index {
            break;
        }
    }

    // Only a walk that executed and committed the whole body produced the
    // block the body describes.
    let whole_body = aborted.is_none() && committed == body_len;

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
                // Block-global log index: cumulative log count of all committed
                // receipts that precede this target in the block.
                //
                // The window starts at `offset`, so a receipt produced before
                // the block's first transaction does not count. That is the
                // deliberate reading: the index this stamps is the one the chain
                // assigns, and the chain numbers logs across the receipts of the
                // block *body*, one per transaction. A receipt with no
                // transaction behind it is not part of that list, so counting
                // its logs would shift every index of the block off the chain's
                // numbering. Today no such receipt exists (`offset` is always
                // zero), which is why the window is the same set as "every
                // receipt but this target's own" — the two readings only part
                // company if the executor ever grows a pre-transaction receipt,
                // and this one stays right when it does.
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
                    create_address(target.from, target.to.into(), target.pre_execution_nonce),
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
                    pre_execution_nonce: target.pre_execution_nonce,
                    receipt,
                    draft: PendingDraft(target.draft),
                })));
            }
            let whole_block = whole_body.then(|| WholeBlock {
                transactions,
                receipts,
                gas_used: block_result.gas_used,
                blob_gas_used: block_result.blob_gas_used,
                requests: block_result.requests,
            });
            FinishOutcome::Harvested { targets: harvested, whole_block }
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use alloy_consensus::{transaction::Recovered, BlockHeader};
    use alloy_primitives::b256;
    use alloy_rpc_types_eth::Block;
    use clap::Parser;
    use mega_evm::revm::inspector::NoOpInspector;

    use super::*;
    use crate::{
        common::{cfg_env, OpProvider, RpcArgs},
        replay::{get_hardfork_config, world, ReplayHardforks},
    };

    /// The transaction the committed offline capture can replay.
    const TARGET: B256 =
        b256!("0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000");

    /// The block [`TARGET`] was mined in.
    const BLOCK: u64 = 18_172_461;

    /// The committed offline capture that answers [`TARGET`]'s replay.
    fn capture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay_offline.cache.json")
    }

    /// A copy of the capture that no longer answers the lookup of [`TARGET`].
    fn capture_without_the_target_lookup() -> tempfile::NamedTempFile {
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(capture()).expect("read the capture"))
                .expect("the capture is JSON");
        let target = format!("{TARGET:#x}");
        let entries = envelope["cache"].as_array_mut().expect("the capture holds a cache array");
        let before = entries.len();
        entries.retain(|entry| {
            let response: serde_json::Value =
                serde_json::from_str(entry["value"].as_str().expect("a cached response"))
                    .expect("a cached response is JSON");
            let result = &response["result"];
            !(result["hash"].as_str() == Some(target.as_str()) &&
                result.get("blockNumber").is_some())
        });
        assert_eq!(entries.len(), before - 1, "the capture answers the target lookup once");
        let file = tempfile::NamedTempFile::new().expect("create a temporary capture");
        std::fs::write(file.path(), serde_json::to_vec(&envelope).expect("serialize"))
            .expect("write the temporary capture");
        file
    }

    /// An offline provider answering from `path`.
    async fn offline_provider(path: &Path) -> OpProvider {
        let args = RpcArgs::parse_from([
            "mega-evme",
            "--rpc.replay-file",
            path.to_str().expect("utf-8 path"),
        ]);
        args.build_replay_provider().await.expect("the capture loads").provider
    }

    /// A driver that only wants the target's receipt.
    struct ReceiptsOnly;

    impl TargetLifecycle for ReceiptsOnly {
        type Inspector = NoOpInspector;
        const INSPECT: bool = false;
        type Tx<'tx> = Recovered<&'tx MegaTxEnvelope>;
        type Draft = ();

        fn before_target<'tx>(
            &mut self,
            tx: &'tx Transaction,
            _inspector: &mut NoOpInspector,
        ) -> Result<Self::Tx<'tx>> {
            Ok(tx.as_recovered())
        }

        fn on_target_executed<DB>(
            &mut self,
            _target: TargetExecution<'_, DB>,
            _inspector: &NoOpInspector,
        ) -> Result<()>
        where
            DB: DatabaseRef,
            DB::Error: core::fmt::Display,
        {
            Ok(())
        }
    }

    /// The block [`TARGET`] belongs to and its parent, as `provider` serves them.
    async fn target_block(provider: &OpProvider) -> (Block<Transaction>, Block<Transaction>) {
        let fetch = |number: u64| async move {
            provider
                .get_block_by_number(number.into())
                .await
                .expect("the capture answers the block")
                .expect("the block exists")
        };
        (fetch(BLOCK).await, fetch(BLOCK - 1).await)
    }

    /// Walk [`TARGET`]'s block up to the target, handing the target over as
    /// `target` and every preceding transaction as listed.
    async fn run_target(
        provider: &OpProvider,
        block: &Block<Transaction>,
        parent: &Block<Transaction>,
        target: BodyEntry<'_>,
    ) -> BlockRun<()> {
        let mut body: Vec<BodyEntry<'_>> = block
            .transactions
            .hashes()
            .take_while(|hash| *hash != TARGET)
            .map(BodyEntry::Listed)
            .collect();
        body.push(target);
        let targets: HashSet<B256> = HashSet::from([TARGET]);

        let chain_id = 4326;
        let chain = get_hardfork_config(chain_id);
        let hardforks = ReplayHardforks::Chain(&chain);
        let spec = hardforks.spec_id(block.header.timestamp());
        let block_env = world::retrieve_block_env(block).expect("the header builds a block env");
        let run = execute_until_targets(
            provider,
            MinedBlockRun {
                hardforks,
                external_envs: EvmeExternalEnvs::new(),
                block_ctx: world::block_ctx(&hardforks, block, parent.hash())
                    .expect("a fork is active"),
                evm_env: EvmEnv::new(cfg_env(chain_id, spec), block_env),
                inspector: NoOpInspector,
                fork_block: BLOCK - 1,
                identity: BlockIdentity {
                    number: BLOCK,
                    timestamp: block.header.timestamp(),
                    hash: block.hash(),
                },
                body: &body,
                body_len: block.transactions.len(),
                targets: &targets,
            },
            &mut ReceiptsOnly,
        )
        .await;
        let Ok(run) = run else { panic!("the block must set up") };
        run
    }

    /// The one target's harvest of a run that completed and finished.
    fn harvested(run: BlockRun<()>) -> HarvestedTarget<()> {
        assert!(matches!(run.loop_outcome, LoopOutcome::Completed(_)), "the walk must complete");
        let FinishOutcome::Harvested { mut targets, .. } = run.finish else {
            panic!("the block must finish")
        };
        assert_eq!(targets.len(), 1, "one target, one harvest");
        match targets.pop() {
            Some(TargetHarvest::Receipt(target)) => *target,
            _ => panic!("the target must have a receipt"),
        }
    }

    /// A target handed over as served executes exactly as the same target
    /// fetched from its listed hash: same result, same receipt.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_target_executes_like_its_listed_form() {
        let provider = offline_provider(&capture()).await;
        let (block, parent) = target_block(&provider).await;
        let tx = provider
            .get_transaction_by_hash(TARGET)
            .await
            .expect("the capture answers the target")
            .expect("the target exists");

        let listed =
            harvested(run_target(&provider, &block, &parent, BodyEntry::Listed(TARGET)).await);
        let served = harvested(
            run_target(&provider, &block, &parent, BodyEntry::Served { tx_hash: TARGET, tx: &tx })
                .await,
        );

        assert_eq!(served.tx_hash, listed.tx_hash);
        assert_eq!(served.tx_index, listed.tx_index);
        assert_eq!(served.exec_result, listed.exec_result);
        assert_eq!(served.pre_execution_nonce, listed.pre_execution_nonce);
        assert_eq!(served.receipt, listed.receipt);
    }

    /// A served target is not looked up again: against an endpoint that no
    /// longer answers the lookup, the listed form aborts on it while the served
    /// form executes.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_target_is_not_looked_up_again() {
        let tx = offline_provider(&capture())
            .await
            .get_transaction_by_hash(TARGET)
            .await
            .expect("the capture answers the target")
            .expect("the target exists");
        let doctored = capture_without_the_target_lookup();
        let provider = offline_provider(doctored.path()).await;
        let (block, parent) = target_block(&provider).await;

        let listed = run_target(&provider, &block, &parent, BodyEntry::Listed(TARGET)).await;
        match listed.loop_outcome {
            LoopOutcome::Aborted {
                error: ReplayError::BlockBodyTransactionFetch { .. },
                tx_hash,
            } => {
                assert_eq!(tx_hash, TARGET, "the abort is the target's own");
            }
            LoopOutcome::Aborted { error, .. } => panic!("unexpected abort: {error}"),
            LoopOutcome::Completed(_) => panic!("the listed target must be looked up"),
        }

        let served =
            run_target(&provider, &block, &parent, BodyEntry::Served { tx_hash: TARGET, tx: &tx })
                .await;
        assert_eq!(harvested(served).tx_hash, TARGET);
    }

    /// A served transaction is still authenticated against the hash it is
    /// reported under, so a driver cannot have one transaction executed in
    /// another's name.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_served_target_is_authenticated_against_its_hash() {
        let provider = offline_provider(&capture()).await;
        let (block, parent) = target_block(&provider).await;
        let other = block.transactions.hashes().next().expect("the block has a transaction");
        assert_ne!(other, TARGET, "the target is not the block's first transaction");
        let impostor = provider
            .get_transaction_by_hash(other)
            .await
            .expect("the capture answers the transaction")
            .expect("the transaction exists");

        let run = run_target(
            &provider,
            &block,
            &parent,
            BodyEntry::Served { tx_hash: TARGET, tx: &impostor },
        )
        .await;
        match run.loop_outcome {
            LoopOutcome::Aborted {
                error: ReplayError::BlockBodyTransactionFetch { tx_hash, message },
                ..
            } => {
                assert_eq!(tx_hash, TARGET);
                assert!(message.contains("different transaction"), "message={message}");
            }
            LoopOutcome::Aborted { error, .. } => panic!("unexpected abort: {error}"),
            LoopOutcome::Completed(_) => panic!("an impostor must not execute"),
        }
    }
}
