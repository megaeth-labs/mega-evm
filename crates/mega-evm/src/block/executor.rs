#[cfg(not(feature = "std"))]
use alloc as std;
use std::{boxed::Box, collections::BTreeMap, vec::Vec};

use alloy_consensus::{Eip658Value, Header, Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::{Encodable2718, Typed2718};
pub use alloy_evm::block::CommitChanges;
use alloy_evm::{
    block::{
        state_changes::post_block_balance_increments, BlockExecutionError, BlockExecutionResult,
        BlockValidationError, ExecutableTx, GasOutput, StateDB,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
    Database, Evm as _, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, RecoveredTx,
};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::B256;
use op_alloy_consensus::OpDepositReceipt;
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{context::result::ResultAndState, database::State, handler::EvmTr, Inspector};

use crate::{
    block::eips, flat_system_contract_specs, is_apply_pending_changes_due, resolve_system_address,
    transact_apply_pending_changes, transact_deploy, transact_deploy_sequencer_registry,
    BlockLimiter, BlockMegaTransactionOutcome, BucketId, MegaBlockExecutionCtx, MegaHardforks,
    MegaSystemCallOutcome, MegaTransaction, MegaTransactionExt, MegaTransactionOutcome,
    StateChangePostBlockSource, StateChangePreBlockSource, StateChangeSource,
};

/// Block executor for the `MegaETH` chain.
///
/// A block executor that processes transactions within a block using `MegaETH`-specific
/// EVM specifications and optimizations. This executor wraps the Optimism block executor
/// and provides access to `MegaETH` features such as enhanced security measures, increased
/// contract size limits, and block environment access tracking for parallel execution.
///
/// # Generic Parameters
///
/// - `H`: The hardfork configuration implementing `MegaHardforks`
/// - `E`: The EVM type implementing `alloy_evm::Evm`
/// - `R`: The receipt builder implementing `OpReceiptBuilder`
///
/// # Implementation Strategy
///
/// This executor uses the delegation pattern to efficiently wrap the underlying Optimism
/// block executor (`OpBlockExecutor`) while providing MegaETH-specific customizations.
/// The delegation ensures minimal overhead while maintaining full compatibility with
/// the Optimism EVM infrastructure.
pub struct MegaBlockExecutor<H, E, R: OpReceiptBuilder> {
    hardforks: H,
    receipt_builder: R,
    ctx: MegaBlockExecutionCtx,
    /// Commit-time block-limit failure latched by the infallible
    /// [`alloy_evm::block::BlockExecutor::commit_transaction`], surfaced by
    /// [`alloy_evm::block::BlockExecutor::finish`].
    pending_commit_error: Option<BlockExecutionError>,

    /// The inner evm instance.
    pub evm: E,
    /// The block limiter for tracking the limit usage.
    pub block_limiter: BlockLimiter,
    /// The receipts for the transactions in the block.
    ///
    /// Mid-build contents are only meaningful together with the rejection latch: a commit-time
    /// block-limit rejection commits nothing — no receipt here, no limiter update, no state
    /// change — and on the infallible [`alloy_evm::block::BlockExecutor::commit_transaction`]
    /// path the only records of it are the latched error and the zero gas it returned. A caller
    /// harvesting this field (or the equivalent trait accessor) directly must consult
    /// [`MegaBlockExecutor::pending_commit_error`] first, or let
    /// [`alloy_evm::block::BlockExecutor::finish`] fail the block; otherwise a rejected
    /// transaction silently vanishes from the block it believes it built.
    pub receipts: Vec<R::Receipt>,
}

impl<C, E, R: OpReceiptBuilder> core::fmt::Debug for MegaBlockExecutor<C, E, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethBlockExecutor").finish_non_exhaustive()
    }
}

/// Refuses a transaction whose inspector was never declared read-only, on the canonical path.
///
/// Block production and block validation are the two places where what the executor reports has to
/// be what the EVM did, reproducibly, on every node. An inspector lives in one node's
/// configuration and its edits reach the receipt, the block's counters and the transaction's
/// state, so the canonical path runs one only on the strength of a
/// [`TrustedObserver`](crate::TrustedObserver) declaration — see
/// [`MegaBlockExecutionError::UndeclaredInspector`].
///
/// The criterion is the declaration and not the measurement, because the measurement cannot answer
/// the question. The shim compares what it is handed across a callback boundary; an inspector that
/// edits the interpreter's stack or memory contents, or writes the journal directly, changes the
/// transaction and leaves every lane at zero. A declaration is what someone asserts in source about
/// a concrete type, which is the only thing that reaches inside a callback.
///
/// Enforced in release builds, deliberately. This is a boundary the canonical path holds against
/// its embedder rather than an invariant `MegaETH` maintains internally, so it has to hold in the
/// binaries that build and validate blocks, and it fails the block rather than the process.
#[inline]
fn reject_undeclared_inspector(
    tx_hash: B256,
    undeclared_inspector: bool,
) -> Result<(), BlockExecutionError> {
    if undeclared_inspector {
        return Err(BlockExecutionError::other(
            crate::MegaBlockExecutionError::UndeclaredInspector { tx_hash },
        ));
    }
    Ok(())
}

/// Refuses a result whose gas accounting an inspector is measured to have moved.
///
/// The backstop behind [`reject_undeclared_inspector`], for what a declaration does not cover: a
/// declared type that did not keep its promise, and a result that reaches the commit funnel from
/// somewhere this executor cannot see — another executor instance, an embedder driving
/// [`crate::MegaEvm::execute_transaction`] itself, or a value built by hand. The result's own
/// ledger is the only thing at that funnel that knows anything about how it was produced.
///
/// The criterion is the whole ledger, not its gas lanes. A rewrite of a frame's classification or
/// output, or a frame the inspector answered itself, moves no gas anywhere and would pass a
/// gas-only check while producing different state and a different receipt.
///
/// The check is free on every path that passes it: the ledger is a `Copy` struct already on the
/// outcome, and this reads its fields once per transaction.
#[inline]
fn reject_inspector_adjusted_accounting(
    tx_hash: B256,
    ledger: crate::InspectorLedger,
) -> Result<(), BlockExecutionError> {
    if ledger.is_zero() {
        return Ok(());
    }
    Err(BlockExecutionError::other(crate::MegaBlockExecutionError::InspectorAdjustedAccounting {
        tx_hash,
        ledger: std::boxed::Box::new(ledger),
    }))
}

impl<DB, H, R, INSP, ExtEnvs> MegaBlockExecutor<H, crate::MegaEvm<DB, INSP, ExtEnvs>, R>
where
    DB: StateDB,
    H: MegaHardforks,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<DB, ExtEnvs>>,
    R: OpReceiptBuilder,
{
    /// Create a new block executor.
    ///
    /// # Parameters
    ///
    /// - `evm`: The EVM instance to use for transaction execution
    /// - `ctx`: The block execution context for tracking access patterns
    /// - `hardforks`: The hardforks configuration implementing [`MegaHardforks`]
    /// - `receipt_builder`: The receipt builder for processing transaction receipts
    ///
    /// # Returns
    ///
    /// A new `BlockExecutor` instance configured with the provided parameters.
    pub fn new(
        evm: crate::MegaEvm<DB, INSP, ExtEnvs>,
        ctx: MegaBlockExecutionCtx,
        hardforks: H,
        receipt_builder: R,
    ) -> Self {
        // Sanity check: spec id must match hardfork
        let block_timestamp = evm.block().timestamp.saturating_to();
        #[cfg(not(any(test, feature = "test-utils")))]
        {
            use crate::HostExt;
            let spec_id = evm.spec_id();
            let expected_spec_id = hardforks.spec_id(block_timestamp);
            assert_eq!(
                spec_id, expected_spec_id,
                "The spec id {} in cfg env must match the expected spec id {} for timestamp {}",
                spec_id, expected_spec_id, block_timestamp
            );
        }
        assert!(
            hardforks.is_regolith_active_at_timestamp(block_timestamp),
            "mega-evm assumes Regolith hardfork is always active"
        );
        assert!(
            hardforks.is_canyon_active_at_timestamp(block_timestamp),
            "mega-evm assumes Canyon hardfork is always active"
        );
        assert!(
            hardforks.is_isthmus_active_at_timestamp(block_timestamp),
            "mega-evm assumes Isthmus hardfork is always active"
        );

        #[cfg(not(any(test, feature = "test-utils")))]
        assert!(
            ctx.block_limits.block_gas_limit == evm.block().gas_limit,
            "block gas limit must be set to the block env gas limit"
        );

        Self {
            hardforks,
            receipt_builder,
            receipts: Vec::new(),
            block_limiter: ctx.block_limits.to_block_limiter(),
            ctx,
            evm,
            pending_commit_error: None,
        }
    }

    /// Gets a mutable reference to the inspector in the `MegaEVM`.
    pub fn inspector_mut(&mut self) -> &mut INSP {
        self.evm.inspector_mut()
    }

    /// Gets a reference to the inspector in the `MegaEVM`.
    pub fn inspector(&self) -> &INSP {
        self.evm.inspector()
    }
}

impl<DB, C, R, INSP, ExtEnvs> MegaBlockExecutor<C, crate::MegaEvm<DB, INSP, ExtEnvs>, R>
where
    DB: StateDB,
    C: MegaHardforks,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<DB, ExtEnvs>>,
    R: OpReceiptBuilder<
        Transaction: Transaction + Encodable2718 + MegaTransactionExt + TransactionEnvelope,
        Receipt: TxReceipt,
    >,
{
    /// Make pre-execution changes on the state. Note that the execution result is not
    /// committed to the block executor's inner state.
    pub fn pre_execution_changes(
        &mut self,
    ) -> Result<Vec<MegaSystemCallOutcome>, BlockExecutionError> {
        let mut outcomes = Vec::new();

        // MegaETH always has Spurious Dragon active. EIP-161 state clearing is now driven by the
        // journal's spec during finalize rather than by a flag on the state database, so there is
        // nothing to set here.

        let block_timestamp: u64 = self.evm.block().timestamp.saturating_to();
        let is_rex_5 = self.hardforks.is_rex_5_active_at_timestamp(block_timestamp);

        // EIP-2935
        let result_and_state = eips::transact_blockhashes_contract_call(
            &self.hardforks,
            self.ctx.parent_hash,
            &mut self.evm,
        )?;
        if let Some(ResultAndState { result, state }) = result_and_state {
            if is_rex_5 && !result.is_success() {
                return Err(BlockValidationError::BlockHashContractCall {
                    message: std::format!(
                        "EIP-2935 pre-block system call did not succeed: {result:?}"
                    ),
                }
                .into());
            }
            outcomes.push(MegaSystemCallOutcome {
                source: StateChangeSource::PreBlock(StateChangePreBlockSource::BlockHashesContract),
                state,
            });
        }

        // EIP-4788
        let result_and_state = eips::transact_beacon_root_contract_call(
            &self.hardforks,
            self.ctx.parent_beacon_block_root,
            &mut self.evm,
        )?;
        if let Some(ResultAndState { result, state }) = result_and_state {
            if is_rex_5 && !result.is_success() {
                let parent_beacon_block_root =
                    self.ctx.parent_beacon_block_root.unwrap_or_default();
                return Err(BlockValidationError::BeaconRootContractCall {
                    parent_beacon_block_root: Box::new(parent_beacon_block_root),
                    message: std::format!(
                        "EIP-4788 pre-block system call did not succeed: {result:?}"
                    ),
                }
                .into());
            }
            outcomes.push(MegaSystemCallOutcome {
                source: StateChangeSource::PreBlock(StateChangePreBlockSource::BeaconRootContract),
                state,
            });
        }

        // In MegaETH, the Isthmus hardfork is always active, which means the Canyon hardfork has
        // already activated and the create2 deployer is already deployed, so we can safely assume
        // that `ensure_create2_deployer` function will never be called.

        // Flat system contracts (Oracle, high-precision timestamp Oracle, KeylessDeploy,
        // MegaAccessControl, MegaLimitControl) share one deploy path via the canonical
        // registry. We tentatively use `StateChangeSource::Transaction(0)` as the state
        // change source, as alloy defines no specific source for these predeploys.
        for spec in flat_system_contract_specs(&self.hardforks, block_timestamp) {
            let state =
                transact_deploy(self.evm.db_mut(), &spec).map_err(BlockExecutionError::other)?;
            outcomes
                .push(MegaSystemCallOutcome { source: StateChangeSource::Transaction(0), state });
        }

        // Rex5 hardfork: deploy SequencerRegistry (first block only) and apply pending
        // role changes if any are due.
        let block_number = self.evm.block().number.to::<u64>();
        if is_rex_5 {
            // Deploy: seeds system address, sequencer, admin, and initialFromBlock
            // into storage on first deploy.
            // Cloned so the helper below can take `&mut self`; two addresses, once per block.
            let params = self
                .hardforks
                .fork_params::<crate::SequencerRegistryConfig>()
                .ok_or_else(|| BlockValidationError::BlockHashContractCall {
                    message: "Rex5 active but SequencerRegistryConfig not configured".into(),
                })?
                .clone();

            // The deploy and apply-pending-changes outcomes commit in push order, while the
            // apply system call always executes against the not-yet-committed state and thus
            // carries the pre-deploy account info in its result. Pre-Rex6 both record the
            // same v1.0.0 code, so the order is irrelevant; at the Rex6 activation block the
            // deploy performs the in-place v1 → v2 bytecode upgrade (when a v1 registry
            // exists), and an apply outcome committed after it would overwrite the upgraded
            // account info with the stale pre-upgrade code. From Rex6 the apply call
            // therefore runs BEFORE the deploy so the upgrade outcome commits last; the
            // `applyPendingChanges()` logic is identical in v1/v2 (v2 changes only rotation
            // scheduling), so its semantics do not depend on which side of the deploy it
            // executes. Pre-Rex6 blocks keep the original deploy-then-apply order untouched.
            let is_rex_6 = self.hardforks.is_rex_6_active_at_timestamp(block_timestamp);

            if !is_rex_6 {
                self.push_deploy_sequencer_registry_outcome(
                    block_timestamp,
                    block_number,
                    &params,
                    &mut outcomes,
                )?;
            }

            // Apply pending role changes if any are due.
            // NOTE: The pre-check reads from the DB *before* the deploy outcome is committed.
            // On the bootstrap block, the registry account does not yet exist in the DB,
            // so the pre-check returns false. This is correct because deploy does not seed
            // any pending slots.
            let (due, witness_state) =
                is_apply_pending_changes_due(self.evm.db_mut(), block_number)?;
            // Always push the witness state (read-only account + slot records).
            outcomes.push(MegaSystemCallOutcome {
                source: StateChangeSource::Transaction(0),
                state: witness_state,
            });
            if due {
                let ResultAndState { state, .. } = transact_apply_pending_changes(&mut self.evm)?;
                outcomes.push(MegaSystemCallOutcome {
                    source: StateChangeSource::Transaction(0),
                    state,
                });
            }

            if is_rex_6 {
                self.push_deploy_sequencer_registry_outcome(
                    block_timestamp,
                    block_number,
                    &params,
                    &mut outcomes,
                )?;
            }
        }

        Ok(outcomes)
    }

    /// Runs the `SequencerRegistry` deploy (bootstrap, in-place upgrade, or idempotent no-op)
    /// and pushes its outcome.
    fn push_deploy_sequencer_registry_outcome(
        &mut self,
        block_timestamp: u64,
        block_number: u64,
        params: &crate::SequencerRegistryConfig,
        outcomes: &mut Vec<MegaSystemCallOutcome>,
    ) -> Result<(), BlockExecutionError> {
        let result_and_state = transact_deploy_sequencer_registry(
            &self.hardforks,
            block_timestamp,
            block_number,
            self.evm.db_mut(),
            params,
        )?;
        if let Some(state) = result_and_state {
            outcomes
                .push(MegaSystemCallOutcome { source: StateChangeSource::Transaction(0), state });
        }
        Ok(())
    }

    /// Make post-execution changes on the state. Note that the execution result is not
    /// committed to the block executor's inner state.
    pub fn post_execution_changes(
        &mut self,
    ) -> Result<Vec<MegaSystemCallOutcome>, BlockExecutionError> {
        let mut outcomes = Vec::new();

        // post block balance increments
        let balance_increments =
            post_block_balance_increments::<Header>(&self.hardforks, self.evm.block(), &[], None);
        // self.evm
        //     .db_mut()
        //     .increment_balances(balance_increments.clone())
        //     .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;
        // let state = balance_increment_state(&balance_increments, self.evm.db_mut())?;
        let state = eips::transact_balance_increments(balance_increments, self.evm.db_mut())
            .map_err(BlockExecutionError::other)?;
        if let Some(state) = state {
            outcomes.push(MegaSystemCallOutcome {
                source: StateChangeSource::PostBlock(StateChangePostBlockSource::BalanceIncrements),
                state,
            });
        }

        Ok(outcomes)
    }

    /// Commit the system call outcomes to the internal state of the block executor.
    pub fn commit_system_call_outcomes(
        &mut self,
        outcomes: Vec<MegaSystemCallOutcome>,
    ) -> Result<(), BlockExecutionError> {
        for outcome in outcomes {
            // The state commit hook is installed on the `State` database and fires from
            // `DatabaseCommit::commit`, so the commit below is what surfaces the change.
            self.evm.db_mut().commit(outcome.state);
        }

        Ok(())
    }

    /// Alias to [`MegaBlockExecutor::run_transaction`].
    pub fn execute_mega_transaction<Tx>(
        &mut self,
        tx: Tx,
    ) -> Result<BlockMegaTransactionOutcome<Tx>, BlockExecutionError>
    where
        Tx: IntoTxEnv<MegaTransaction>
            + RecoveredTx<R::Transaction>
            + MegaTransactionExt
            + Encodable2718
            + Copy,
    {
        self.run_transaction(tx)
    }

    /// Execute a transaction with a commit condition function without committing the execution
    /// result to the block executor's inner state.
    ///
    /// `tx_size`/`da_size` are resolved from `Tx` through [`MegaTransactionExt`]: a `Tx` that
    /// carries precomputed values (e.g. [`crate::EnrichedMegaTx`]) reuses them, while any other
    /// `Tx` falls back to the trait's default, which recomputes them from the EIP-2718 encoding.
    /// The choice is resolved at compile time by trait dispatch, so callers do not pick a
    /// variant — this is the single execution entry point regardless of whether the transaction
    /// carries a size cache.
    ///
    /// The `alloy_evm` block-execution path
    /// ([`alloy_evm::block::BlockExecutor::execute_transaction_with_commit_condition`]) does not
    /// route through this method: its `tx: impl ExecutableTx<Self>` parameter cannot be required
    /// to implement [`MegaTransactionExt`], so it recomputes the sizes itself and calls
    /// [`MegaBlockExecutor::run_transaction_with_sizes`] directly.
    ///
    /// # Correctness
    ///
    /// `tx_size`/`da_size` feed directly into [`BlockLimiter::pre_execution_check`]'s
    /// `tx_encode_size_limit`/`tx_da_size_limit`/block cumulative-size checks. When `Tx`
    /// overrides the defaults with cached values, those values are trusted with no validation
    /// against the real encoded transaction: callers MUST ensure `Tx::tx_size()`/
    /// `Tx::estimated_da_size()` accurately reflect `tx`'s actual EIP-2718 encoding — an
    /// understated value lets a transaction bypass a limit it should have been rejected by.
    /// This is safe for the sequencer's own block-building path (the cache is computed by the
    /// same trusted process, e.g. at mempool insertion), but a `Tx` whose cached sizes could
    /// come from an untrusted or stale source (e.g. block validation of another party's block)
    /// must not be fed here without first re-establishing that invariant. A `Tx` that uses the
    /// recomputing default (e.g. a bare `Recovered<...>`) is unconditionally safe.
    ///
    /// A `debug_assert` cross-checks the resolved values against a fresh recompute, so a caller
    /// bug is caught in tests/CI; it is compiled out in release builds. For the recomputing
    /// default it is a no-op; for a cached override it is the real safety net.
    ///
    /// # Parameters
    ///
    /// - `tx`: The transaction to execute.
    ///
    /// # Returns
    ///
    /// Returns the execution outcome of the transaction. Note that the execution result is not
    /// committed to the block executor's inner state.
    pub fn run_transaction<Tx>(
        &mut self,
        tx: Tx,
    ) -> Result<BlockMegaTransactionOutcome<Tx>, BlockExecutionError>
    where
        Tx: IntoTxEnv<MegaTransaction>
            + RecoveredTx<R::Transaction>
            + MegaTransactionExt
            + Encodable2718
            + Copy,
    {
        let tx_size = tx.tx_size();
        let da_size = tx.estimated_da_size();
        debug_assert_eq!(
            tx_size,
            tx.encode_2718_len() as u64,
            "run_transaction: Tx-reported tx_size does not match a fresh recompute from the \
             encoded transaction"
        );
        debug_assert_eq!(
            da_size,
            op_alloy_flz::tx_estimated_size_fjord_bytes(tx.encoded_2718().as_slice()),
            "run_transaction: Tx-reported da_size does not match a fresh recompute from the \
             encoded transaction"
        );
        self.run_transaction_with_sizes(tx, tx_size, da_size)
    }

    /// Shared body of [`MegaBlockExecutor::run_transaction`] and the `alloy_evm`
    /// block-execution path: `tx_size`/`da_size` are resolved by the caller (recomputed or read
    /// from a cache), this only consumes them.
    ///
    /// This is the escape hatch for callers whose `Tx` cannot implement [`MegaTransactionExt`]
    /// (e.g. the `alloy_evm` `ExecutableTx`-constrained path): they resolve the sizes themselves
    /// and pass them in. Prefer [`MegaBlockExecutor::run_transaction`] otherwise, which resolves
    /// the sizes for you and cross-checks any cached values.
    ///
    /// # Contract
    ///
    /// An EVM running an inspector whose type carries no
    /// [`TrustedObserver`](crate::TrustedObserver) declaration refuses the transaction with
    /// [`MegaBlockExecutionError::UndeclaredInspector`](
    /// crate::MegaBlockExecutionError::UndeclaredInspector) before executing it, and a result
    /// whose gas accounting an inspector is measured to have moved is refused with
    /// [`MegaBlockExecutionError::InspectorAdjustedAccounting`](
    /// crate::MegaBlockExecutionError::InspectorAdjustedAccounting) after. A declared tracer is
    /// unaffected; an embedder that wants a rewriting inspector drives
    /// [`crate::MegaEvm::execute_transaction`] directly.
    pub fn run_transaction_with_sizes<Tx>(
        &mut self,
        tx: Tx,
        tx_size: u64,
        da_size: u64,
    ) -> Result<BlockMegaTransactionOutcome<Tx>, BlockExecutionError>
    where
        Tx: IntoTxEnv<MegaTransaction> + RecoveredTx<R::Transaction> + Copy,
    {
        // Before anything else, including execution: an undeclared inspector does not run on this
        // path at all. Refusing after the fact would leave its callbacks a window in which to
        // reach the executor's own state cache through `db_mut()`.
        reject_undeclared_inspector(tx.tx().tx_hash(), self.evm.has_undeclared_inspector())?;

        let is_deposit = tx.tx().ty() == DEPOSIT_TRANSACTION_TYPE;

        // Check transaction-level and block-level limits before transaction execution
        self.block_limiter.pre_execution_check(
            tx.tx().tx_hash(),
            tx.tx().gas_limit(),
            tx_size,
            da_size,
            is_deposit,
        )?;

        // Cache the depositor account prior to the state transition for the deposit nonce.
        //
        // Note that in MegaETH, the Regolith hardfork is always active, so we always have deposit
        // nonces. In addition, regular transactions don't have deposit
        // nonces, so we don't need to touch the DB for those.
        let depositor = is_deposit
            .then(|| self.evm.db_mut().basic(*tx.signer()).map(|info| info.unwrap_or_default()))
            .transpose()
            .map_err(BlockExecutionError::other)?;

        let hash = tx.tx().trie_hash();

        // Execute transaction.
        let outcome = self
            .evm
            .execute_transaction(tx.into_tx_env())
            .map_err(move |err| BlockExecutionError::evm(alloy_op_evm::map_op_err(err), hash))?;
        reject_inspector_adjusted_accounting(tx.tx().tx_hash(), outcome.inspector_ledger)?;

        Ok(BlockMegaTransactionOutcome { tx, tx_size, da_size, depositor, inner: outcome })
    }

    /// Runs a transaction that has already been split into its EVM environment and its recovered
    /// consensus form.
    ///
    /// `alloy_evm::block::ExecutableTx` no longer exposes the transaction directly, so the
    /// `BlockExecutor` path splits it via `into_parts` and hands both halves here. Behaviour is
    /// identical to [`MegaBlockExecutor::run_transaction_with_sizes`].
    pub fn run_tx_env_with_sizes<Rec>(
        &mut self,
        tx_env: MegaTransaction,
        recovered: Rec,
        tx_size: u64,
        da_size: u64,
    ) -> Result<(Option<revm::state::AccountInfo>, MegaTransactionOutcome), BlockExecutionError>
    where
        Rec: RecoveredTx<R::Transaction>,
    {
        // Same order as `run_transaction_with_sizes`: the inspector's declaration is settled
        // before the transaction runs.
        reject_undeclared_inspector(recovered.tx().tx_hash(), self.evm.has_undeclared_inspector())?;

        let is_deposit = recovered.tx().ty() == DEPOSIT_TRANSACTION_TYPE;

        self.block_limiter.pre_execution_check(
            recovered.tx().tx_hash(),
            recovered.tx().gas_limit(),
            tx_size,
            da_size,
            is_deposit,
        )?;

        let depositor = is_deposit
            .then(|| self.evm.db_mut().basic(*recovered.signer()).map(|i| i.unwrap_or_default()))
            .transpose()
            .map_err(BlockExecutionError::other)?;

        let hash = recovered.tx().trie_hash();
        let outcome = self
            .evm
            .execute_transaction(tx_env)
            .map_err(move |err| BlockExecutionError::evm(alloy_op_evm::map_op_err(err), hash))?;
        reject_inspector_adjusted_accounting(recovered.tx().tx_hash(), outcome.inspector_ledger)?;

        Ok((depositor, outcome))
    }

    /// Commits a [`crate::MegaBlockTxResult`] produced by
    /// [`MegaBlockExecutor::run_tx_env_with_sizes`].
    ///
    /// This is the single commit body: every other commit entry —
    /// [`MegaBlockExecutor::commit_transaction_outcome`], its alias, and the infallible
    /// `BlockExecutor::commit_transaction` — funnels into it. Receipts feed the receipts root, so
    /// their construction must not exist twice. All identity fields (hash, gas limit, deposit
    /// signal) are read from the already-recorded result; the deposit signal is the `depositor`
    /// record (`Some` iff the transaction is a deposit).
    ///
    /// # Contract
    ///
    /// This is the fallible commit entry point for a result, and the one an embedder that drives
    /// execution and commit as two separate steps should use.
    ///
    /// Block-level admission is re-validated here against the limiter state as it stands *now*,
    /// not as it stood when the transaction executed. Execution and commit are separate steps, so
    /// other transactions may have been committed in between (this is how the parallel executor
    /// works: speculatively execute many transactions, then commit the survivors one by one), and
    /// the block may have filled up in that window. A transaction that no longer fits is
    /// rejected: this returns `Err` **before** touching any executor state, so no receipt is
    /// pushed, no limiter counter is advanced and no state is committed, leaving the executor
    /// usable for the remaining transactions. The caller decides what to do with the rejected
    /// transaction (typically: drop it from the block and put it back in the pool).
    ///
    /// Rejection is not a block-level failure — a block that ends without the rejected
    /// transaction is perfectly valid — which is why the error is returned rather than latched.
    ///
    /// This is also where a result an inspector took part in is refused, ahead of admission and
    /// of any other reading: the producers guard their own outputs, but a result reaching this
    /// funnel may have been produced by another executor instance, by an embedder driving
    /// [`crate::MegaEvm::execute_transaction`] itself, or built by hand. What the outcome carries
    /// is the only thing here that knows how it was produced — the inspector's declaration, and
    /// then the ledger. See [`MegaBlockExecutionError::UndeclaredInspector`](
    /// crate::MegaBlockExecutionError::UndeclaredInspector) and
    /// [`MegaBlockExecutionError::InspectorAdjustedAccounting`](
    /// crate::MegaBlockExecutionError::InspectorAdjustedAccounting).
    pub fn commit_tx_result(
        &mut self,
        result: crate::MegaBlockTxResult<<R::Transaction as TransactionEnvelope>::TxType>,
    ) -> Result<u64, BlockExecutionError>
    where
        R::Transaction: TransactionEnvelope,
    {
        let crate::MegaBlockTxResult {
            tx_type,
            tx_hash,
            gas_limit,
            tx_size,
            da_size,
            depositor,
            inner:
                MegaTransactionOutcome {
                    result_and_state: ResultAndState { result, state },
                    data_size,
                    kv_updates,
                    compute_gas_used,
                    // The transaction's derived destroyed total is a reported number; the block
                    // reports it through `compute_gas_used`, which already carries it, and reaches
                    // its own enforced counter through `compute_gas_enforced` instead of
                    // subtracting this one back out.
                    compute_gas_destroyed: _,
                    compute_gas_enforced,
                    state_growth_used,
                    inspector_ledger,
                    undeclared_inspector,
                },
        } = result;

        // Before anything else, including admission: a result an inspector took part in is not
        // one this block may contain at all, whether or not it would still fit. The declaration
        // is asked first, because it is the admission rule and the ledger is the backstop behind
        // it — an undeclared inspector is refused whether or not anything it did was measurable.
        reject_undeclared_inspector(tx_hash, undeclared_inspector)?;
        reject_inspector_adjusted_accounting(tx_hash, inspector_ledger)?;

        // Re-validate limits at commit time to handle parallel execution race conditions.
        // Between execution and commit, other transactions may have been committed, potentially
        // exhausting the block's remaining capacity.
        self.block_limiter.pre_execution_check(
            tx_hash,
            gas_limit,
            tx_size,
            da_size,
            depositor.is_some(),
        )?;

        // Accumulate post-execution resource usage into block-level counters. This does not
        // validate limits; over-limit enforcement happens in `pre_execution_check` before the
        // next transaction. The deposit-nonce record doubles as the deposit signal here.
        //
        // Compute gas crosses this boundary as the pair execution produced it — the full reported
        // total and the part of it the transaction enforced its own limits against — so the
        // limiter can report one and enforce the other. Collapsing them here would hand the block
        // a single number that is right for reporting and wrong for admission.
        self.block_limiter.post_execution_update_raw(
            result.tx_gas_used(),
            tx_size,
            da_size,
            data_size,
            kv_updates,
            compute_gas_used,
            compute_gas_enforced,
            state_growth_used,
            depositor.is_some(),
        );

        let gas_used = result.tx_gas_used();
        let block_gas_used = self.block_limiter.block_gas_used;
        self.receipts.push(
            match self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx_type,
                result,
                cumulative_gas_used: block_gas_used,
                evm: &self.evm,
                state: &state,
            }) {
                Ok(receipt) => receipt,
                Err(ctx) => {
                    let receipt = alloy_consensus::Receipt {
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used: block_gas_used,
                        logs: ctx.result.into_logs(),
                    };
                    self.receipt_builder.build_deposit_receipt(OpDepositReceipt {
                        inner: receipt,
                        deposit_receipt_version: depositor.is_some().then_some(1),
                        deposit_nonce: depositor.map(|account| account.nonce),
                    })
                }
            },
        );

        self.evm.db_mut().commit(state);

        Ok(gas_used)
    }

    /// Alias to [`MegaBlockExecutor::commit_transaction_outcome`].
    pub fn commit_execution_outcome<Tx>(
        &mut self,
        outcome: BlockMegaTransactionOutcome<Tx>,
    ) -> Result<u64, BlockExecutionError>
    where
        Tx: RecoveredTx<R::Transaction>,
    {
        self.commit_transaction_outcome(outcome)
    }

    /// Commit the execution outcome of a transaction.
    ///
    /// This is [`MegaBlockExecutor::commit_tx_result`] for callers still holding the transaction
    /// object: it derives the identity fields the commit needs (`tx_type`, `tx_hash`,
    /// `gas_limit`) and forwards. There is deliberately no separate commit body — receipts feed
    /// the receipts root, so every path must build them through the same code.
    ///
    /// The `depositor` record doubles as the deposit signal downstream (`Some` iff the
    /// transaction is a deposit); the `run_transaction*` methods that produce outcomes uphold
    /// that, and a hand-built outcome must too.
    ///
    /// Block-level admission is re-validated at commit time: a transaction whose block capacity
    /// was consumed by another transaction committed in between is rejected with `Err`, before
    /// any receipt, limiter counter or state change is applied. The executor stays usable and
    /// the caller decides what to do with the rejected transaction.
    ///
    /// # Returns
    ///
    /// Returns the gas used by the transaction.
    pub fn commit_transaction_outcome<Tx>(
        &mut self,
        outcome: BlockMegaTransactionOutcome<Tx>,
    ) -> Result<u64, BlockExecutionError>
    where
        Tx: RecoveredTx<R::Transaction>,
    {
        let BlockMegaTransactionOutcome { tx, tx_size, da_size, depositor, inner } = outcome;

        self.commit_tx_result(crate::MegaBlockTxResult {
            tx_type: tx.tx().tx_type(),
            tx_hash: tx.tx().tx_hash(),
            gas_limit: tx.tx().gas_limit(),
            tx_size,
            da_size,
            depositor,
            inner,
        })
    }

    /// Get the bucket IDs used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the bucket IDs used during transaction execution.
    pub fn get_accessed_bucket_ids(&self) -> Vec<BucketId> {
        self.evm.ctx_ref().dynamic_storage_gas_cost.borrow().get_bucket_ids()
    }

    /// The commit-time block-limit failure latched by the infallible
    /// [`alloy_evm::block::BlockExecutor::commit_transaction`], if any.
    ///
    /// `commit_transaction` cannot report a rejected transaction through its return type, so it
    /// records the failure here instead of committing, and
    /// [`alloy_evm::block::BlockExecutor::finish`] fails the block with it. Callers that want to
    /// react before then (e.g. to drop the transaction and keep building) can poll this after
    /// every commit, or use the fallible [`MegaBlockExecutor::commit_tx_result`] instead, which
    /// never latches.
    ///
    /// Only the first failure is kept: once the block has an inadmissible transaction, it is the
    /// one that explains the failure, and later commits do not overwrite it.
    pub fn pending_commit_error(&self) -> Option<&BlockExecutionError> {
        self.pending_commit_error.as_ref()
    }

    /// Takes the latched commit-time block-limit failure, clearing it.
    ///
    /// Clearing it makes [`alloy_evm::block::BlockExecutor::finish`] succeed again, so a caller
    /// that takes the error is asserting it has handled the rejected transaction (which was never
    /// committed: no receipt, no limiter update, no state change) and that the block is still
    /// valid without it. See [`MegaBlockExecutor::pending_commit_error`].
    pub fn take_pending_commit_error(&mut self) -> Option<BlockExecutionError> {
        self.pending_commit_error.take()
    }
}

/// Block-hash accessors that require the concrete revm [`State`] database.
///
/// These are kept separate from the generic executor body because
/// [`alloy_evm::block::BlockExecutorFactory`] only guarantees `DB: StateDB`, which does not expose
/// the block hash cache.
impl<'db, DB, C, R, INSP, ExtEnvs>
    MegaBlockExecutor<C, crate::MegaEvm<&'db mut State<DB>, INSP, ExtEnvs>, R>
where
    DB: Database + 'db,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<&'db mut State<DB>, ExtEnvs>>,
    R: OpReceiptBuilder,
{
    /// Get the block hashes used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the block hashes used during transaction execution.
    pub fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.evm.db().block_hashes.iter().collect()
    }

    /// Clears the recorded block hash accesses.
    ///
    /// Block hash reads accumulate in the executor's database across every
    /// transaction executed so far. Callers that need to attribute BLOCKHASH
    /// reads to a single transaction (e.g. replay fixture dumping) clear the
    /// record before executing it. The record is a cache: a cleared hash is
    /// simply re-fetched from the underlying database on the next access, so
    /// execution results are unaffected.
    pub fn clear_accessed_block_hashes(&mut self) {
        self.evm.db_mut().block_hashes = Default::default();
    }
}

/// Implementation of `alloy_evm::block::BlockExecutor` for `MegaETH` block executor.
///
/// This implementation delegates all block execution operations to the underlying
/// Optimism block executor while providing MegaETH-specific customizations through
/// the configured chain specification and EVM factory.
impl<DB, C, R, INSP, ExtEnvs> alloy_evm::block::BlockExecutor
    for MegaBlockExecutor<C, crate::MegaEvm<DB, INSP, ExtEnvs>, R>
where
    DB: StateDB,
    C: MegaHardforks,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<DB, ExtEnvs>>,
    R: OpReceiptBuilder<
        Transaction: Transaction + Encodable2718 + MegaTransactionExt + TransactionEnvelope,
        Receipt: TxReceipt,
    >,
    crate::MegaTransaction: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
{
    type Transaction = R::Transaction;

    type Receipt = R::Receipt;

    type Evm = crate::MegaEvm<DB, INSP, ExtEnvs>;

    /// `BlockExecutor::Result` must be a single concrete type, so it cannot borrow the caller's
    /// transaction. The receipt builder only needs the transaction type, so that is all this
    /// carries — no clone of the envelope.
    type Result = crate::MegaBlockTxResult<<R::Transaction as TransactionEnvelope>::TxType>;

    /// NOTE: this function resembles the one in
    /// `alloy_op_evm::OpBlockExecutor::apply_pre_execution_changes`. Changes there should be
    /// synced.
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        let outcomes = self.pre_execution_changes()?;
        self.commit_system_call_outcomes(outcomes)?;

        // After all pre-block outcomes are committed, resolve the system address for this block.
        // This reads _currentSystemAddress from the now-committed SequencerRegistry storage.
        // The returned EvmState captures the read as a witness record.
        let spec = self.evm.ctx().mega_spec();
        let (system_address, read_state) =
            resolve_system_address(&self.hardforks, spec, self.evm.db_mut())?;
        if let Some(state) = read_state {
            self.evm.db_mut().commit(state);
        }
        self.evm.ctx_mut().set_system_address(system_address);

        Ok(())
    }

    /// Executes and commits in one step, so no other transaction can be committed in between and
    /// the commit-time re-validation performed by [`MegaBlockExecutor::commit_tx_result`] cannot
    /// observe a different block state than the admission check already passed by
    /// [`alloy_evm::block::BlockExecutor::execute_transaction_without_commit`]. Any rejection is
    /// still returned as `Err` rather than latched, because this method can report it.
    ///
    /// NOTE: this function resembles the one in
    /// `alloy_op_evm::OpBlockExecutor::execute_transaction_with_commit_condition`. Changes there
    /// should be synced.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&Self::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let output = self.execute_transaction_without_commit(tx)?;
        if !f(&output).should_commit() {
            return Ok(None);
        }
        // Commit through the fallible path rather than `commit_transaction`, so a rejection
        // surfaces to this caller directly instead of being latched for `finish`.
        self.commit_tx_result(output).map(|gas_used| Some(GasOutput::new(gas_used)))
    }

    /// Runs the block-level admission check for `tx` and executes it, without committing.
    ///
    /// Passing this check does not entitle the result to be committed: the block may fill up
    /// before [`alloy_evm::block::BlockExecutor::commit_transaction`] runs, which re-checks. See
    /// that method for how such a late rejection is reported.
    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, recovered) = tx.into_parts();
        // `tx: impl ExecutableTx<Self>` cannot be required to implement `MegaTransactionExt`, so
        // this path recomputes the sizes from the raw inner transaction and bypasses
        // `run_transaction` (which reads them via the trait). See `run_transaction`'s docs.
        let tx_size = recovered.tx().encode_2718_len() as u64;
        let da_size = recovered.tx().estimated_da_size();
        let tx_type = recovered.tx().tx_type();
        let tx_hash = recovered.tx().tx_hash();
        let gas_limit = recovered.tx().gas_limit();
        let (depositor, inner) = self.run_tx_env_with_sizes(tx_env, recovered, tx_size, da_size)?;
        Ok(crate::MegaBlockTxResult {
            tx_type,
            tx_hash,
            gas_limit,
            tx_size,
            da_size,
            depositor,
            inner,
        })
    }

    /// # Contract
    ///
    /// Upstream made this hook infallible, but `MegaETH` re-validates block-level admission at
    /// commit time: execution and commit are separate steps, and under parallel execution other
    /// transactions may have been committed in between, leaving no room for this one. Such a
    /// transaction must not be committed — a block containing it would violate the block limits
    /// — yet the signature has no way to say so.
    ///
    /// So on rejection this commits nothing (no receipt, no limiter update, no state change),
    /// records the failure, and reports zero gas used, which is accurate: the transaction
    /// contributed nothing to the block. [`alloy_evm::block::BlockExecutor::finish`] then fails
    /// the block with the recorded error, so a rejection can never be silently dropped;
    /// [`MegaBlockExecutor::pending_commit_error`] exposes it earlier for callers that want to
    /// react before `finish`. Zero is unambiguous: a committed transaction always uses at least
    /// intrinsic gas, so a zero return from this method always means the rejection latch is set.
    ///
    /// Prefer [`MegaBlockExecutor::commit_tx_result`] when the caller can act on a rejection: it
    /// is the same commit, returns the error instead of latching it, and leaves the executor
    /// free to continue building the block without the rejected transaction.
    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        match self.commit_tx_result(output) {
            Ok(gas_used) => GasOutput::new(gas_used),
            Err(err) => {
                // Keep the first rejection: it is the one that explains why the block is invalid.
                self.pending_commit_error.get_or_insert(err);
                GasOutput::new(0)
            }
        }
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }

    /// Fails the block if [`alloy_evm::block::BlockExecutor::commit_transaction`] rejected a
    /// transaction, since that path could not report the rejection itself.
    ///
    /// NOTE: this function resembles the one in
    /// `alloy_op_evm::OpBlockExecutor::finish`. Changes there should be
    /// synced.
    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        if let Some(err) = self.pending_commit_error.take() {
            return Err(err);
        }

        let outcomes = self.post_execution_changes()?;
        self.commit_system_call_outcomes(outcomes)?;

        let gas_used = self.receipts.last().map(|r| r.cumulative_gas_used()).unwrap_or_default();
        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Default::default(),
                gas_used,
                blob_gas_used: 0,
            },
        ))
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }
}
