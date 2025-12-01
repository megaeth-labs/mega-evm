#[cfg(not(feature = "std"))]
use alloc as std;
use std::{borrow::Cow, boxed::Box, collections::BTreeMap, vec::Vec};

use alloy_consensus::{Eip658Value, Header, Transaction, TxReceipt};
use alloy_eips::{Encodable2718, Typed2718};
pub use alloy_evm::block::CommitChanges;
use alloy_evm::{
    block::{
        state_changes::{balance_increment_state, post_block_balance_increments},
        BlockExecutionError, BlockExecutionResult, BlockValidationError, ExecutableTx, OnStateHook,
        StateChangePostBlockSource, StateChangeSource, SystemCaller,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
    Database, Evm as _, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, RecoveredTx,
};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_op_hardforks::OpHardforks;
use alloy_primitives::B256;
use op_alloy_consensus::OpDepositReceipt;
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    context::result::ExecutionResult, database::State, handler::EvmTr, DatabaseCommit, Inspector,
};

use crate::{
    ensure_high_precision_timestamp_oracle_contract_deployed, ensure_oracle_contract_deployed,
    BlockLimiter, BlockMegaTransactionOutcome, BucketId, MegaBlockExecutionCtx, MegaSpecId,
    MegaTransaction, MegaTransactionExt, MegaTransactionOutcome,
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
/// - `C`: The chain specification implementing `OpHardforks` (typically `SpecId`)
/// - `E`: The EVM type implementing `alloy_evm::Evm`
/// - `R`: The receipt builder implementing `OpReceiptBuilder`
///
/// # Implementation Strategy
///
/// This executor uses the delegation pattern to efficiently wrap the underlying Optimism
/// block executor (`OpBlockExecutor`) while providing MegaETH-specific customizations.
/// The delegation ensures minimal overhead while maintaining full compatibility with
/// the Optimism EVM infrastructure.
pub struct MegaBlockExecutor<C, E, R: OpReceiptBuilder> {
    chain_spec: C,
    receipt_builder: R,
    ctx: MegaBlockExecutionCtx,
    evm: E,

    block_limiter: BlockLimiter,

    system_caller: SystemCaller<C>,

    receipts: Vec<R::Receipt>,
}

impl<C, E, R: OpReceiptBuilder> core::fmt::Debug for MegaBlockExecutor<C, E, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethBlockExecutor").finish_non_exhaustive()
    }
}

impl<C, E, R> MegaBlockExecutor<C, E, R>
where
    C: OpHardforks + Clone,
    E: alloy_evm::Evm<Tx: FromRecoveredTx<R::Transaction>>,
    R: OpReceiptBuilder,
{
    /// Create a new block executor.
    ///
    /// # Parameters
    ///
    /// - `evm`: The EVM instance to use for transaction execution
    /// - `ctx`: The block execution context for tracking access patterns
    /// - `spec`: The chain specification implementing [`OpHardforks`]
    /// - `receipt_builder`: The receipt builder for processing transaction receipts
    ///
    /// # Returns
    ///
    /// A new `BlockExecutor` instance configured with the provided parameters.
    pub fn new(evm: E, ctx: MegaBlockExecutionCtx, spec: C, receipt_builder: R) -> Self {
        // do some safety check on hardforks
        let timestamp = evm.block().timestamp.saturating_to();
        assert!(
            spec.is_regolith_active_at_timestamp(timestamp),
            "mega-evm assumes Regolith hardfork is not active"
        );
        assert!(
            spec.is_canyon_active_at_timestamp(timestamp),
            "mega-evm assumes Canyon hardfork is always active"
        );
        assert!(
            spec.is_isthmus_active_at_timestamp(timestamp),
            "mega-evm assumes Isthmus hardfork is always active"
        );

        #[cfg(not(any(test, feature = "test-utils")))]
        assert!(
            ctx.block_limits.block_gas_limit == evm.block().gas_limit,
            "block gas limit must be set to the block env gas limit"
        );

        Self {
            chain_spec: spec.clone(),
            receipt_builder,
            receipts: Vec::new(),
            block_limiter: ctx.block_limits.to_block_limiter(),
            ctx,
            evm,
            system_caller: SystemCaller::new(spec),
        }
    }

    /// Gets a mutable reference to the inspector in the MegaEVM.
    pub fn inspector_mut(&mut self) -> &mut <E as alloy_evm::Evm>::Inspector {
        self.evm.inspector_mut()
    }
}

impl<'db, DB, C, R, INSP, ExtEnvs>
    MegaBlockExecutor<C, crate::MegaEvm<&'db mut State<DB>, INSP, ExtEnvs>, R>
where
    DB: Database + 'db,
    C: OpHardforks,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<&'db mut State<DB>, ExtEnvs>>,
    R: OpReceiptBuilder<
        Transaction: Transaction + Encodable2718 + MegaTransactionExt,
        Receipt: TxReceipt,
    >,
{
    /// Execute a transaction with a commit condition function without committing the execution
    /// result to the block executor's inner state.
    ///
    /// # Parameters
    ///
    /// - `tx`: The transaction to execute.
    ///
    /// # Returns
    ///
    /// Returns the execution outcome of the transaction. Note that the execution result is not
    /// committed to the block executor's inner state.
    pub fn execute_mega_transaction<Tx>(
        &mut self,
        tx: Tx,
    ) -> Result<BlockMegaTransactionOutcome<Tx>, BlockExecutionError>
    where
        Tx: IntoTxEnv<MegaTransaction> + RecoveredTx<R::Transaction> + Copy,
    {
        let is_deposit = tx.tx().ty() == DEPOSIT_TRANSACTION_TYPE;
        let tx_size = tx.tx().encode_2718_len() as u64;
        let da_size = tx.tx().estimated_da_size();

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
                self.evm
                    .db_mut()
                    .load_cache_account(*tx.signer())
                    .map(|acc| acc.account_info().unwrap_or_default())
            })
            .transpose()
            .map_err(BlockExecutionError::other)?;

        let hash = tx.tx().trie_hash();

        // Execute transaction.
        let outcome = self
            .evm
            .execute_transaction(tx.into_tx_env())
            .map_err(move |err| BlockExecutionError::evm(err, hash))?;

        Ok(BlockMegaTransactionOutcome { tx, tx_size, da_size, depositor, inner: outcome })
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
    pub fn commit_execution_outcome<Tx>(
        &mut self,
        outcome: BlockMegaTransactionOutcome<Tx>,
    ) -> Result<u64, BlockExecutionError>
    where
        Tx: IntoTxEnv<MegaTransaction> + RecoveredTx<R::Transaction> + Copy,
    {
        // Check block-level limits after transaction execution but before committing
        self.block_limiter.post_execution_check(&outcome)?;

        let BlockMegaTransactionOutcome { tx, depositor, inner, .. } = outcome;
        let MegaTransactionOutcome { result, state, .. } = inner;
        let gas_used = result.gas_used();

        self.system_caller.on_state(StateChangeSource::Transaction(self.receipts.len()), &state);

        let block_gas_used = self.block_limiter.block_gas_used;
        self.receipts.push(
            match self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx: tx.tx(),
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

    /// Get the block hashes used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the block hashes used during transaction execution.
    pub fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.evm.db().block_hashes.clone()
    }
}

/// Implementation of `alloy_evm::block::BlockExecutor` for `MegaETH` block executor.
///
/// This implementation delegates all block execution operations to the underlying
/// Optimism block executor while providing MegaETH-specific customizations through
/// the configured chain specification and EVM factory.
impl<'db, DB, C, R, INSP, ExtEnvs> alloy_evm::block::BlockExecutor
    for MegaBlockExecutor<C, crate::MegaEvm<&'db mut State<DB>, INSP, ExtEnvs>, R>
where
    DB: Database + 'db,
    C: OpHardforks,
    ExtEnvs: crate::ExternalEnvTypes,
    INSP: Inspector<crate::MegaContext<&'db mut State<DB>, ExtEnvs>>,
    R: OpReceiptBuilder<
        Transaction: Transaction + Encodable2718 + MegaTransactionExt,
        Receipt: TxReceipt,
    >,
    crate::MegaTransaction: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
{
    type Transaction = R::Transaction;

    type Receipt = R::Receipt;

    type Evm = crate::MegaEvm<&'db mut State<DB>, INSP, ExtEnvs>;

    /// NOTE: this function resembles the one in
    /// `alloy_op_evm::OpBlockExecutor::apply_pre_execution_changes`. Changes there should be
    /// synced.
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        // In MegaETH, the Spurious Dragon hardfork is always active, so we can safely set the state
        // clear flag to true.
        self.evm.db_mut().set_state_clear_flag(true);

        self.system_caller.apply_blockhashes_contract_call(self.ctx.parent_hash, &mut self.evm)?;
        self.system_caller
            .apply_beacon_root_contract_call(self.ctx.parent_beacon_block_root, &mut self.evm)?;

        // In MegaETH, the Isthmus hardfork is always active, which means the Canyon hardfork has
        // already activated and the create2 deployer is already deployed, so we can safely assume
        // that `ensure_create2_deployer` function will never be called.

        // If the MiniRex hardfork is active, we need to ensure the oracle contract is deployed.
        if self.evm.ctx.spec.is_enabled(MegaSpecId::MINI_REX) {
            // System oracle contract, which is the centralized storage of oracle data for all
            // oracle services.
            let state = ensure_oracle_contract_deployed(self.evm_mut().db_mut())
                .map_err(BlockExecutionError::other)?;
            // Invoke the state hook with state changes. We tentatively use
            // `StateChangeSource::Transaction(0)` as state change source as there is no specific
            // source defined for this oracle contract in alloy. This may change in the
            // future.
            self.system_caller.on_state(StateChangeSource::Transaction(0), &state);

            // commit changes to database
            self.evm.db_mut().commit(state);

            // High precision timestamp oracle service
            let state =
                ensure_high_precision_timestamp_oracle_contract_deployed(self.evm_mut().db_mut())
                    .map_err(BlockExecutionError::other)?;
            // Invoke the state hook with state changes. We tentatively use
            // `StateChangeSource::Transaction(0)` as state change source as there is no specific
            // source defined for this oracle contract in alloy. This may change in the
            // future.
            self.system_caller.on_state(StateChangeSource::Transaction(0), &state);

            // commit changes to database
            self.evm.db_mut().commit(state);
        }

        Ok(())
    }

    /// NOTE: this function resembles the one in
    /// `alloy_op_evm::OpBlockExecutor::execute_transaction_with_commit_condition`. Changes there
    /// should be synced.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&ExecutionResult<<Self::Evm as alloy_evm::Evm>::HaltReason>) -> CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        let outcome = self.execute_mega_transaction(tx)?;
        if f(&outcome.result).should_commit() {
            let gas_used = self.commit_execution_outcome(outcome)?;
            Ok(Some(gas_used))
        } else {
            Ok(None)
        }
    }

    /// NOTE: this function resembles the one in
    /// `alloy_op_evm::OpBlockExecutor::finish`. Changes there should be
    /// synced.
    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        let balance_increments =
            post_block_balance_increments::<Header>(&self.chain_spec, self.evm.block(), &[], None);
        // increment balances
        self.evm
            .db_mut()
            .increment_balances(balance_increments.clone())
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;
        // call state hook with changes due to balance increments.
        self.system_caller.try_on_state_with(|| {
            balance_increment_state(&balance_increments, self.evm.db_mut()).map(|state| {
                (
                    StateChangeSource::PostBlock(StateChangePostBlockSource::BalanceIncrements),
                    Cow::Owned(state),
                )
            })
        })?;

        let gas_used = self.receipts.last().map(|r| r.cumulative_gas_used()).unwrap_or_default();
        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Default::default(),
                gas_used,
            },
        ))
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.system_caller.with_state_hook(hook);
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }
}
