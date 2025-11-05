//! Block execution abstraction for the `MegaETH` EVM.
//!
//! This module provides block execution functionality specifically tailored for the `MegaETH`
//! chain, built on top of the Optimism EVM (`op-revm`) with MegaETH-specific customizations and
//! optimizations.
//!
//! # Architecture
//!
//! The block execution system consists of three main components:
//!
//! 2. **`BlockExecutorFactory`**: Factory for creating block executors with `MegaETH`
//!    specifications
//! 3. **`BlockExecutor`**: The actual executor that processes transactions within a block
//!
//! # EVM Specifications
//!
//! `MegaETH` supports two EVM specifications:
//!
//! - **`EQUIVALENCE`**: Maintains equivalence with Optimism Isthmus EVM (default)
//! - **`MINI_REX`**: Enhanced version with quadratic LOG costs and disabled SELFDESTRUCT
//!
//! # Performance Considerations
//!
//! The `MegaETH` block executor is optimized for high-performance blockchain operations:
//!
//! - Efficient delegation to the underlying Optimism EVM implementation
//! - Minimal overhead for MegaETH-specific features
//! - Support for parallel execution through access tracking
//! - Optimized gas calculations for modified opcodes

use std::{borrow::Cow, collections::BTreeMap};

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;
use alloy_consensus::{Eip658Value, Header, Transaction, TxReceipt};
use alloy_eips::{Encodable2718, Typed2718};
pub use alloy_evm::block::CommitChanges;
use alloy_evm::{
    block::{
        state_changes::{balance_increment_state, post_block_balance_increments},
        BlockExecutionError, BlockExecutionResult, BlockExecutorFor, BlockValidationError,
        ExecutableTx, OnStateHook, StateChangePostBlockSource, StateChangeSource, SystemCaller,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
    Database, Evm as _, EvmEnv, EvmFactory, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv,
    RecoveredTx,
};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_op_hardforks::OpHardforks;
use alloy_primitives::{Bytes, B256};
use op_alloy_consensus::OpDepositReceipt;
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    context::result::{ExecutionResult, ResultAndState},
    database::State,
    handler::EvmTr,
    inspector::NoOpInspector,
    state::EvmState,
    DatabaseCommit, Inspector,
};
use salt::BucketId;

use crate::{
    ensure_high_precision_timestamp_oracle_contract_deployed, ensure_oracle_contract_deployed,
    MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};

/// `MegaETH` receipt builder type.
pub trait MegaReceiptBuilder: OpReceiptBuilder {}
impl<T: OpReceiptBuilder> MegaReceiptBuilder for T {}

/// `MegaETH` block executor factory.
///
/// A factory for creating block executors configured with MegaETH-specific specifications
/// and optimizations. This factory encapsulates the chain specification, EVM factory,
/// and receipt builder needed to create block executors that support `MegaETH` features
/// such as enhanced security measures and increased contract size limits.
///
/// # Generic Parameters
///
/// - `ChainSpec`: The chain specification implementing [`OpHardforks`]
/// - `EvmF`: The EVM factory type implementing [`alloy_evm::EvmFactory`]
/// - `ReceiptBuilder`: The receipt builder implementing [`OpReceiptBuilder`] to build op-stack
///   receipts
///
/// # Implementation Details
///
/// The factory implements `alloy_evm::block::BlockExecutorFactory` and delegates
/// to the underlying Optimism EVM implementation while providing MegaETH-specific
/// customizations through the configured chain specification and EVM factory.
#[derive(Debug, Clone)]
pub struct MegaBlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder> {
    receipt_builder: ReceiptBuilder,
    spec: ChainSpec,
    evm_factory: EvmF,
}

impl<ChainSpec, EvmF, ReceiptBuilder> MegaBlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder>
where
    ReceiptBuilder: OpReceiptBuilder,
{
    /// Create a new block executor factory.
    ///
    /// # Parameters
    ///
    /// - `spec`: The chain specification (e.g., `SpecId::MINI_REX` or `SpecId::EQUIVALENCE`)
    /// - `evm_factory`: The EVM factory for creating EVM instances
    /// - `receipt_builder`: The receipt builder for processing transaction receipts
    ///
    /// # Returns
    ///
    /// A new `BlockExecutorFactory` instance configured with the provided parameters.
    pub fn new(spec: ChainSpec, evm_factory: EvmF, receipt_builder: ReceiptBuilder) -> Self {
        Self { receipt_builder, spec, evm_factory }
    }

    /// Returns a reference to the EVM factory.
    pub fn evm_factory_ref(&self) -> &EvmF {
        &self.evm_factory
    }

    /// Returns a mutable reference to the EVM factory.
    pub fn evm_factory_mut(&mut self) -> &mut EvmF {
        &mut self.evm_factory
    }
}

impl<ChainSpec, ExtEnvs, ReceiptBuilder>
    MegaBlockExecutorFactory<ChainSpec, crate::MegaEvmFactory<ExtEnvs>, ReceiptBuilder>
where
    ChainSpec: OpHardforks + Clone,
    ReceiptBuilder: OpReceiptBuilder<Transaction: Transaction + Encodable2718> + Clone,
    crate::MegaTransaction: FromRecoveredTx<ReceiptBuilder::Transaction>,
    ExtEnvs: crate::ExternalEnvs + Clone,
{
    /// Create a new block executor.
    ///
    /// # Parameters
    ///
    /// - `db`: The database to use for EVM state.
    /// - `evm_env`: The EVM environment, including block and config environments.
    /// - `block_ctx`: The block execution context for tracking access patterns.
    ///
    /// # Returns
    ///
    /// A new `BlockExecutor` instance configured with the provided parameters.
    pub fn create_executor<'a, DB>(
        &self,
        db: &'a mut State<DB>,
        evm_env: EvmEnv<MegaSpecId>,
        block_ctx: MegaBlockExecutionCtx,
    ) -> MegaBlockExecutor<
        ChainSpec,
        MegaEvm<&'a mut State<DB>, NoOpInspector, ExtEnvs>,
        ReceiptBuilder,
    >
    where
        DB: Database + 'a,
    {
        let evm = self.evm_factory.create_evm(db, evm_env);
        MegaBlockExecutor::new(evm, block_ctx, self.spec.clone(), self.receipt_builder.clone())
    }

    /// Create a new block executor with an inspector.
    ///
    /// # Parameters
    ///
    /// - `db`: The database to use for EVM state.
    /// - `evm_env`: The EVM environment, including block and config environments.
    /// - `block_ctx`: The block execution context for tracking access patterns.
    /// - `inspector`: The inspector to use for debugging and monitoring.
    ///
    /// # Returns
    ///
    /// A new `BlockExecutor` instance configured with the provided parameters.
    pub fn create_executor_with_inspector<'a, DB, I>(
        &self,
        db: &'a mut State<DB>,
        evm_env: EvmEnv<MegaSpecId>,
        block_ctx: MegaBlockExecutionCtx,
        inspector: I,
    ) -> MegaBlockExecutor<ChainSpec, MegaEvm<&'a mut State<DB>, I, ExtEnvs>, ReceiptBuilder>
    where
        DB: Database + 'a,
        I: Inspector<crate::MegaContext<&'a mut State<DB>, ExtEnvs>> + 'a,
    {
        let evm = self.evm_factory.create_evm_with_inspector(db, evm_env, inspector);
        MegaBlockExecutor::new(evm, block_ctx, self.spec.clone(), self.receipt_builder.clone())
    }
}

impl<ChainSpec, ExtEnvs, ReceiptBuilder> alloy_evm::block::BlockExecutorFactory
    for MegaBlockExecutorFactory<ChainSpec, crate::MegaEvmFactory<ExtEnvs>, ReceiptBuilder>
where
    ReceiptBuilder: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    ChainSpec: OpHardforks + Clone,
    ExtEnvs: crate::ExternalEnvs + Clone,
    crate::MegaTransaction: FromRecoveredTx<ReceiptBuilder::Transaction>
        + FromTxWithEncoded<ReceiptBuilder::Transaction>,
    Self: 'static,
{
    type EvmFactory = crate::MegaEvmFactory<ExtEnvs>;
    type ExecutionCtx<'a> = MegaBlockExecutionCtx;
    type Transaction = ReceiptBuilder::Transaction;
    type Receipt = ReceiptBuilder::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.evm_factory_ref()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as alloy_evm::EvmFactory>::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<<Self::EvmFactory as alloy_evm::EvmFactory>::Context<&'a mut State<DB>>> + 'a,
    {
        MegaBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
    }
}

/// Block execution context for the `MegaETH` chain.
#[derive(Debug, Clone)]
pub struct MegaBlockExecutionCtx {
    /// Parent block hash.
    pub parent_hash: B256,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// The block's extra data.
    pub extra_data: Bytes,
    /// The maximum amount of data allowed to generate from a block.
    /// Defaults to [`crate::constants::mini_rex::BLOCK_DATA_LIMIT`].
    pub block_data_limit: u64,
    /// The maximum amount of key-value updates allowed to generate from a block.
    /// Defaults to [`crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT`].
    pub block_kv_update_limit: u64,
    /// The maximum size of all transactions (transaction body, not execution outcome) included in
    /// a block.
    pub block_tx_size_limit: u64,
}

impl Default for MegaBlockExecutionCtx {
    fn default() -> Self {
        Self {
            parent_hash: B256::ZERO,
            parent_beacon_block_root: None,
            extra_data: Bytes::new(),
            block_data_limit: crate::constants::mini_rex::BLOCK_DATA_LIMIT,
            block_kv_update_limit: crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT,
            block_tx_size_limit: u64::MAX,
        }
    }
}

impl MegaBlockExecutionCtx {
    /// Create a new block execution context with default limits.
    pub fn new(
        parent_hash: B256,
        parent_beacon_block_root: Option<B256>,
        extra_data: Bytes,
    ) -> Self {
        Self { parent_hash, parent_beacon_block_root, extra_data, ..Default::default() }
    }

    /// Set a custom block data limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified data limit.
    pub fn with_data_limit(mut self, limit: u64) -> Self {
        self.block_data_limit = limit;
        self
    }

    /// Set a custom block KV update limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified KV update limit.
    pub fn with_kv_update_limit(mut self, limit: u64) -> Self {
        self.block_kv_update_limit = limit;
        self
    }

    /// Set a custom block transaction size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction size limit.
    pub fn with_tx_size_limit(mut self, limit: u64) -> Self {
        self.block_tx_size_limit = limit;
        self
    }
}

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

    system_caller: SystemCaller<C>,

    receipts: Vec<R::Receipt>,
    gas_used: u64,
    block_data_used: u64,
    block_kv_updates_used: u64,
    block_tx_size_used: u64,
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
        Self {
            ctx,
            chain_spec: spec.clone(),
            receipt_builder,
            receipts: Vec::new(),
            gas_used: 0,
            block_data_used: 0,
            block_kv_updates_used: 0,
            block_tx_size_used: 0,
            evm,
            system_caller: SystemCaller::new(spec),
        }
    }
}

// Helper methods for accessing MegaEvm-specific functionality
impl<DB, C, R, INSP, ExtEnvs> MegaBlockExecutor<C, crate::MegaEvm<DB, INSP, ExtEnvs>, R>
where
    DB: Database,
    C: OpHardforks,
    ExtEnvs: crate::ExternalEnvs,
    INSP: Inspector<crate::MegaContext<DB, ExtEnvs>>,
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718>,
{
    /// Get the current data size and KV update count from the EVM context.
    fn get_tx_resource_usage(&self) -> (u64, u64) {
        let additional_limit = self.evm.ctx.additional_limit.borrow();
        let data_size = additional_limit.data_size_tracker.current_size();
        let kv_updates = additional_limit.kv_update_counter.current_count();
        (data_size, kv_updates)
    }

    /// Check and update block-level resource limits
    fn post_check_limits(&mut self) -> Result<(), BlockExecutionError> {
        let (tx_data_size, tx_kv_updates) = self.get_tx_resource_usage();

        // Get limits from context
        let block_data_limit = self.ctx.block_data_limit;
        let block_kv_update_limit = self.ctx.block_kv_update_limit;

        let new_block_data = self.block_data_used + tx_data_size;
        let new_block_kv_updates = self.block_kv_updates_used + tx_kv_updates;

        if new_block_data > block_data_limit {
            return Err(BlockExecutionError::other(MegaBlockLimitExceededError::DataLimit {
                block_used: self.block_data_used,
                tx_used: tx_data_size,
                limit: block_data_limit,
            }));
        }

        if new_block_kv_updates > block_kv_update_limit {
            return Err(BlockExecutionError::other(MegaBlockLimitExceededError::KVUpdateLimit {
                block_used: self.block_kv_updates_used,
                tx_used: tx_kv_updates,
                limit: block_kv_update_limit,
            }));
        }

        // Update accumulators
        self.block_data_used = new_block_data;
        self.block_kv_updates_used = new_block_kv_updates;

        Ok(())
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
    ExtEnvs: crate::ExternalEnvs,
    INSP: Inspector<crate::MegaContext<&'db mut State<DB>, ExtEnvs>>,
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    crate::MegaTransaction: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
    Self: MegaBlockExecutorExt<R>,
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
        self.execute_mega_transaction(tx, |outcome| f(outcome.result))
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

/// Extension trait for `MegaBlockExecutor` to add MegaETH-specific transaction execution
/// functionality.
pub trait MegaBlockExecutorExt<R: OpReceiptBuilder> {
    /// Execute a transaction with a commit condition function.
    ///
    /// This method executes a transaction and calls the commit condition function with the
    /// transaction execution outcome.
    ///
    /// # Parameters
    ///
    /// - `tx`: The transaction to execute.
    /// - `on_outcome`: The function to call with the transaction execution outcome. The function
    ///   should return whether the transaction should be committed into the block executor's inner
    ///   state.
    ///
    /// # Returns
    ///
    /// Returns the gas used by the transaction.
    fn execute_mega_transaction(
        &mut self,
        tx: impl IntoTxEnv<MegaTransaction> + RecoveredTx<R::Transaction> + Copy,
        on_outcome: impl FnOnce(MegaTransactionExecutionOutcome<'_>) -> CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError>;

    /// Get the bucket IDs used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the bucket IDs used during transaction execution.
    fn get_accessed_bucket_ids(&self) -> Vec<BucketId>;

    /// Get the block hashes used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the block hashes used during transaction execution.
    fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256>;
}

impl<'db, DB, C, R, INSP, ExtEnvs> MegaBlockExecutorExt<R>
    for MegaBlockExecutor<C, crate::MegaEvm<&'db mut State<DB>, INSP, ExtEnvs>, R>
where
    DB: Database + 'db,
    C: OpHardforks,
    ExtEnvs: crate::ExternalEnvs,
    INSP: Inspector<crate::MegaContext<&'db mut State<DB>, ExtEnvs>>,
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
{
    fn execute_mega_transaction(
        &mut self,
        tx: impl IntoTxEnv<MegaTransaction> + RecoveredTx<R::Transaction> + Copy,
        f: impl FnOnce(MegaTransactionExecutionOutcome<'_>) -> CommitChanges,
    ) -> Result<Option<u64>, BlockExecutionError> {
        let is_deposit = tx.tx().ty() == DEPOSIT_TRANSACTION_TYPE;

        // The sum of the transaction’s gas limit, Tg, and the gas utilized in this block prior,
        // must be no greater than the block’s gasLimit.
        let block_available_gas = self.evm.block().gas_limit - self.gas_used;
        // In MegaETH, the Regolith hardfork is not active, so we can safely assume that the
        // transaction gas limit is always less than the block's gas limit.
        if tx.tx().gas_limit() > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: tx.tx().gas_limit(),
                block_available_gas,
            }
            .into());
        }

        // The sum of the transaction size must be no greater than the block's tx size limit.
        let tx_size = tx.tx().encode_2718_len() as u64;
        if tx_size + self.block_tx_size_used > self.ctx.block_tx_size_limit {
            return Err(BlockExecutionError::other(
                MegaBlockLimitExceededError::TransactionSizeLimit {
                    block_used: self.block_tx_size_used,
                    tx_used: tx_size,
                    limit: self.ctx.block_tx_size_limit,
                },
            ));
        }

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
        let ResultAndState { result, state } =
            self.evm.transact(tx).map_err(move |err| BlockExecutionError::evm(err, hash))?;

        let evm_ctx = self.evm.ctx_ref();
        if !f(MegaTransactionExecutionOutcome {
            result: &result,
            state: &state,
            data_size: evm_ctx.generated_data_size(),
            kv_updates: evm_ctx.kv_update_count(),
        })
        .should_commit()
        {
            return Ok(None);
        }

        // Check block-level limits after transaction execution but before committing
        self.post_check_limits()?;

        self.system_caller.on_state(StateChangeSource::Transaction(self.receipts.len()), &state);

        let gas_used = result.gas_used();

        // append gas used
        self.gas_used += gas_used;

        // append transaction size
        self.block_tx_size_used += tx_size;

        self.receipts.push(
            match self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx: tx.tx(),
                result,
                cumulative_gas_used: self.gas_used,
                evm: &self.evm,
                state: &state,
            }) {
                Ok(receipt) => receipt,
                Err(ctx) => {
                    let receipt = alloy_consensus::Receipt {
                        // Success flag was added in `EIP-658: Embedding transaction status code
                        // in receipts`.
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used: self.gas_used,
                        logs: ctx.result.into_logs(),
                    };

                    self.receipt_builder.build_deposit_receipt(OpDepositReceipt {
                        inner: receipt,
                        deposit_nonce: depositor.map(|account| account.nonce),
                        // The deposit receipt version was introduced in Canyon to indicate an
                        // update to how receipt hashes should be computed
                        // when set. The state transition process ensures
                        // this is only set for post-Canyon deposit
                        // transactions. In MegaETH, Canyon is always active.
                        deposit_receipt_version: is_deposit.then_some(1),
                    })
                }
            },
        );

        self.evm.db_mut().commit(state);

        Ok(Some(gas_used))
    }

    fn get_accessed_bucket_ids(&self) -> Vec<BucketId> {
        self.evm.ctx_ref().dynamic_gas_cost.borrow().get_bucket_ids()
    }

    fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.evm.db().block_hashes.clone()
    }
}

/// The execution outcome of a transaction in `MegaETH`.
///
/// This struct contains additional information about the transaction execution on top of the
/// standard EVM's execution result and state.
#[derive(Debug, Clone)]
pub struct MegaTransactionExecutionOutcome<'a> {
    /// The transaction execution result.
    pub result: &'a ExecutionResult<MegaHaltReason>,
    /// The post-execution evm state.
    pub state: &'a EvmState,
    /// The data size usage in bytes.
    pub data_size: u64,
    /// The number of KV updates.
    pub kv_updates: u64,
}

/// Error type for block-level limit exceeded. These errors are only thrown after the transaction
/// execution but before any changes are committed to the database.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MegaBlockLimitExceededError {
    /// Block data limit exceeded.
    #[error(
        "Block data limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}"
    )]
    DataLimit {
        /// Data used by block so far
        block_used: u64,
        /// Data used by current transaction
        tx_used: u64,
        /// Block data limit
        limit: u64,
    },

    /// Block KV update limit exceeded.
    #[error("Block KV update limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    KVUpdateLimit {
        /// KV updates used by block so far
        block_used: u64,
        /// KV updates used by current transaction
        tx_used: u64,
        /// Block KV update limit
        limit: u64,
    },

    /// Transaction size limit exceeded.
    #[error("Transaction size limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    TransactionSizeLimit {
        /// Transaction size used by block so far
        block_used: u64,
        /// Transaction size used by current transaction
        tx_used: u64,
        /// Transaction size limit
        limit: u64,
    },
}
