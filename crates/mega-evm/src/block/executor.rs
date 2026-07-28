#[cfg(not(feature = "std"))]
use alloc as std;
use std::{boxed::Box, collections::BTreeMap, vec::Vec};

use alloy_consensus::{Eip658Value, Header, Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::{Encodable2718, Typed2718};
pub use alloy_evm::block::CommitChanges;
use alloy_evm::{
    Database, Evm as _, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, RecoveredTx,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockValidationError, ExecutableTx, GasOutput,
        state_changes::post_block_balance_increments,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::B256;
use op_alloy_consensus::OpDepositReceipt;
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    DatabaseCommit, Inspector, context::result::ExecResultAndState, database::State,
    handler::EvmTr, state::EvmState,
};

/// Receives state changes together with their Mega-specific execution source.
pub trait MegaOnStateHook: Send + 'static {
    /// Handles a state change immediately before it is committed.
    fn on_state(&mut self, source: StateChangeSource, state: &EvmState);
}

impl<F> MegaOnStateHook for F
where
    F: FnMut(StateChangeSource, &EvmState) + Send + 'static,
{
    fn on_state(&mut self, source: StateChangeSource, state: &EvmState) {
        self(source, state)
    }
}

use crate::{
    BlockLimiter, BlockMegaTransactionOutcome, BucketId, MegaBlockExecutionCtx, MegaHardforks,
    MegaSystemCallOutcome, MegaTransactionExt, MegaTransactionOutcome, StateChangePostBlockSource,
    StateChangePreBlockSource, StateChangeSource, block::eips, flat_system_contract_specs,
    is_apply_pending_changes_due, resolve_system_address, transact_apply_pending_changes,
    transact_deploy, transact_deploy_sequencer_registry,
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
    state_hook: Option<Box<dyn MegaOnStateHook>>,

    /// The inner evm instance.
    pub evm: E,
    /// The block limiter for tracking the limit usage.
    pub block_limiter: BlockLimiter,
    /// The receipts for the transactions in the block.
    pub receipts: Vec<R::Receipt>,
}

impl<C, E, R: OpReceiptBuilder> core::fmt::Debug for MegaBlockExecutor<C, E, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethBlockExecutor").finish_non_exhaustive()
    }
}

impl<DB, H, R, INSP, ExtEnvs> MegaBlockExecutor<H, crate::MegaEvm<DB, INSP, ExtEnvs>, R>
where
    DB: Database + DatabaseCommit,
    H: MegaHardforks + Clone,
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
            hardforks: hardforks.clone(),
            receipt_builder,
            receipts: Vec::new(),
            block_limiter: ctx.block_limits.to_block_limiter(),
            ctx,
            evm,
            state_hook: None,
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
    DB: Database + DatabaseCommit,
    C: MegaHardforks + Clone,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<DB, ExtEnvs>>,
    R: OpReceiptBuilder<
            Transaction: Transaction + Encodable2718 + MegaTransactionExt,
            Receipt: TxReceipt,
        >,
{
    /// Make pre-execution changes on the state. Note that the execution result is not
    /// committed to the block executor's inner state.
    pub fn pre_execution_changes(
        &mut self,
    ) -> Result<Vec<MegaSystemCallOutcome>, BlockExecutionError> {
        let mut outcomes = Vec::new();

        // In MegaETH, the Spurious Dragon hardfork is always active, so we can safely set the state
        // clear flag to true.
        let block_timestamp: u64 = self.evm.block().timestamp.saturating_to();
        let is_rex_5 = self.hardforks.is_rex_5_active_at_timestamp(block_timestamp);

        // EIP-2935
        let result_and_state = eips::transact_blockhashes_contract_call(
            &self.hardforks,
            self.ctx.parent_hash,
            &mut self.evm,
        )?;
        if let Some(ExecResultAndState { result, state }) = result_and_state {
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
        if let Some(ExecResultAndState { result, state }) = result_and_state {
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
                let ExecResultAndState { state, .. } =
                    transact_apply_pending_changes(&mut self.evm)?;
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
            if let Some(hook) = self.state_hook.as_mut() {
                hook.on_state(outcome.source, &outcome.state);
            }
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
        Tx: IntoTxEnv<alloy_op_evm::OpTx>
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
        Tx: IntoTxEnv<alloy_op_evm::OpTx>
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
    pub fn run_transaction_with_sizes<Tx>(
        &mut self,
        tx: Tx,
        tx_size: u64,
        da_size: u64,
    ) -> Result<BlockMegaTransactionOutcome<Tx>, BlockExecutionError>
    where
        Tx: IntoTxEnv<alloy_op_evm::OpTx> + RecoveredTx<R::Transaction> + Copy,
    {
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
            .then(|| {
                revm::Database::basic(self.evm.db_mut(), *tx.signer())
                    .map(Option::unwrap_or_default)
            })
            .transpose()
            .map_err(BlockExecutionError::other)?;

        let hash = tx.tx().trie_hash();

        // Execute transaction.
        let outcome = self
            .evm
            .execute_transaction(tx.into_tx_env().into())
            .map_err(move |err| BlockExecutionError::evm(alloy_op_evm::map_op_err(err), hash))?;

        Ok(BlockMegaTransactionOutcome { tx, tx_size, da_size, depositor, inner: outcome })
    }

    /// Alias to [`MegaBlockExecutor::commit_transaction_outcome`].
    pub fn commit_execution_outcome<Tx>(
        &mut self,
        outcome: BlockMegaTransactionOutcome<Tx>,
    ) -> Result<u64, BlockExecutionError>
    where
        Tx: RecoveredTx<R::Transaction> + Copy,
    {
        self.commit_transaction_outcome(outcome)
    }

    /// Commit the execution outcome of a transaction.
    ///
    /// This method commits the execution outcome of a transaction to the block executor's inner
    /// state.
    ///
    /// # Parameters
    ///
    /// - `outcome`: The execution outcome of the transaction.
    ///
    /// # Returns
    ///
    /// Returns the gas used by the transaction.
    pub fn commit_transaction_outcome<Tx>(
        &mut self,
        outcome: BlockMegaTransactionOutcome<Tx>,
    ) -> Result<u64, BlockExecutionError>
    where
        Tx: RecoveredTx<R::Transaction> + Copy,
    {
        // Re-validate limits at commit time to handle parallel execution race conditions.
        // Between run_transaction() and commit_transaction_outcome(), other transactions
        // may have been committed, potentially exceeding block limits.
        self.block_limiter.pre_execution_check(
            outcome.tx.tx().tx_hash(),
            outcome.tx.tx().gas_limit(),
            outcome.tx_size,
            outcome.da_size,
            outcome.tx.tx().ty() == DEPOSIT_TRANSACTION_TYPE,
        )?;

        // Accumulate post-execution resource usage into block-level counters.
        // This does not validate limits; over-limit enforcement happens in
        // `pre_execution_check` before the next transaction.
        self.block_limiter.post_execution_update(&outcome)?;

        let BlockMegaTransactionOutcome { tx, depositor, inner, .. } = outcome;
        let revm::context::result::ResultAndState { result, state } = inner.inner;
        let gas_used = result.tx_gas_used();

        if let Some(hook) = self.state_hook.as_mut() {
            hook.on_state(StateChangeSource::Transaction(self.receipts.len()), &state);
        }

        let block_gas_used = self.block_limiter.block_gas_used;
        self.receipts.push(
            match self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx_type: tx.tx().tx_type(),
                result,
                cumulative_gas_used: block_gas_used,
                evm: &self.evm,
                state: &state,
            }) {
                Ok(receipt) => receipt,
                Err(ctx) => {
                    let receipt = alloy_consensus::Receipt {
                        // Success flag was added in `EIP-658: Embedding transaction status code
                        // in receipts`.
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used: block_gas_used,
                        logs: ctx.result.into_logs(),
                    };

                    self.receipt_builder.build_deposit_receipt(OpDepositReceipt {
                        inner: receipt,
                        // The deposit receipt version was introduced in Canyon to indicate an
                        // update to how receipt hashes should be computed
                        // when set. The state transition process ensures
                        // this is only set for post-Canyon deposit
                        // transactions. In MegaETH, Canyon is always active.
                        deposit_receipt_version: depositor.is_some().then_some(1),
                        deposit_nonce: depositor.map(|account| account.nonce),
                    })
                }
            },
        );

        self.evm.db_mut().commit(state);

        Ok(gas_used)
    }

    /// Get the bucket IDs used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the bucket IDs used during transaction execution.
    pub fn get_accessed_bucket_ids(&self) -> Vec<BucketId> {
        self.evm.ctx_ref().dynamic_storage_gas_cost.borrow().get_bucket_ids()
    }

    /// Sets the hook invoked immediately before each state commit.
    pub fn set_state_hook(&mut self, hook: Option<Box<dyn MegaOnStateHook>>) {
        self.state_hook = hook;
    }
}

impl<'db, DB, H, R, INSP, ExtEnvs>
    MegaBlockExecutor<H, crate::MegaEvm<&'db mut State<DB>, INSP, ExtEnvs>, R>
where
    DB: Database,
    R: OpReceiptBuilder,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<&'db mut State<DB>, ExtEnvs>>,
{
    /// Returns the block hashes read during execution.
    pub fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.evm.db().block_hashes.iter().collect()
    }

    /// Clears block-hash reads recorded by the state cache.
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
    DB: alloy_evm::block::StateDB,
    C: MegaHardforks + Clone,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<DB, ExtEnvs>>,
    R: OpReceiptBuilder<
            Transaction: Transaction + Encodable2718 + MegaTransactionExt,
            Receipt: TxReceipt,
        >,
    alloy_op_evm::OpTx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
{
    type Transaction = R::Transaction;

    type Receipt = R::Receipt;

    type Evm = crate::MegaEvm<DB, INSP, ExtEnvs>;

    type Result = BlockMegaTransactionOutcome<(
        <R::Transaction as TransactionEnvelope>::TxType,
        B256,
        u64,
        bool,
    )>;

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
            if let Some(hook) = self.state_hook.as_mut() {
                hook.on_state(StateChangeSource::Transaction(0), &state);
            }
            self.evm.db_mut().commit(state);
        }
        self.evm.ctx_mut().set_system_address(system_address);

        Ok(())
    }

    /// NOTE: this function resembles the one in
    /// `alloy_op_evm::OpBlockExecutor::execute_transaction_with_commit_condition`. Changes there
    /// should be synced.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&Self::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let outcome = self.execute_transaction_without_commit(tx)?;
        if f(&outcome).should_commit() {
            Ok(Some(self.commit_transaction(outcome)))
        } else {
            Ok(None)
        }
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, tx) = tx.into_parts();
        let tx_size = tx.tx().encode_2718_len() as u64;
        let da_size = tx.tx().estimated_da_size();
        let is_deposit = tx.tx().ty() == DEPOSIT_TRANSACTION_TYPE;

        self.block_limiter.pre_execution_check(
            tx.tx().tx_hash(),
            tx.tx().gas_limit(),
            tx_size,
            da_size,
            is_deposit,
        )?;

        let depositor = is_deposit
            .then(|| {
                revm::Database::basic(self.evm.db_mut(), *tx.signer())
                    .map(Option::unwrap_or_default)
            })
            .transpose()
            .map_err(BlockExecutionError::other)?;
        let tx_hash = tx.tx().trie_hash();
        let tx_gas_limit = tx.tx().gas_limit();
        let tx_type = tx.tx().tx_type();
        let inner = self
            .evm
            .execute_transaction(tx_env.into())
            .map_err(|err| BlockExecutionError::evm(alloy_op_evm::map_op_err(err), tx_hash))?;

        Ok(BlockMegaTransactionOutcome {
            tx: (tx_type, tx_hash, tx_gas_limit, is_deposit),
            tx_size,
            da_size,
            depositor,
            inner,
        })
    }

    fn commit_transaction(&mut self, outcome: Self::Result) -> GasOutput {
        let BlockMegaTransactionOutcome {
            tx: (tx_type, _, _, is_deposit),
            tx_size,
            da_size,
            depositor,
            inner,
        } = outcome;
        self.block_limiter.post_execution_update_raw(
            inner.result.tx_gas_used(),
            tx_size,
            da_size,
            inner.data_size,
            inner.kv_updates,
            inner.compute_gas_used,
            inner.state_growth_used,
            is_deposit,
        );

        let MegaTransactionOutcome { inner, .. } = inner;
        let revm::context::result::ResultAndState { result, state } = inner;
        let gas_used = result.tx_gas_used();
        if let Some(hook) = self.state_hook.as_mut() {
            hook.on_state(StateChangeSource::Transaction(self.receipts.len()), &state);
        }
        let cumulative_gas_used = self.block_limiter.block_gas_used;
        self.receipts.push(
            match self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx_type,
                result,
                cumulative_gas_used,
                evm: &self.evm,
                state: &state,
            }) {
                Ok(receipt) => receipt,
                Err(ctx) => {
                    let receipt = alloy_consensus::Receipt {
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used,
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
        GasOutput::new(gas_used)
    }

    /// NOTE: this function resembles the one in
    /// `alloy_op_evm::OpBlockExecutor::finish`. Changes there should be
    /// synced.
    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
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

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }
}
