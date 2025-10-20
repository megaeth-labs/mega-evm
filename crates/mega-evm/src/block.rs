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

use std::borrow::Cow;

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;
use alloy_consensus::{Eip658Value, Header, Transaction, TxReceipt};
use alloy_eips::{Encodable2718, Typed2718};
use alloy_evm::{
    block::{
        state_changes::{balance_increment_state, post_block_balance_increments},
        BlockExecutionError, BlockExecutionResult, BlockExecutorFor, BlockValidationError,
        CommitChanges, ExecutableTx, OnStateHook, StateChangePostBlockSource, StateChangeSource,
        SystemCaller,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
    Database, FromRecoveredTx, FromTxWithEncoded,
};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_op_hardforks::OpHardforks;
use alloy_primitives::{Bytes, B256};
use op_alloy_consensus::OpDepositReceipt;
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::{
    context::result::{ExecutionResult, ResultAndState},
    database::State,
    DatabaseCommit, Inspector,
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

impl<ChainSpec, EvmF, ReceiptBuilder> MegaBlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder> {
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

impl<ChainSpec, EvmF, ReceiptBuilder> alloy_evm::block::BlockExecutorFactory
    for MegaBlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder>
where
    ReceiptBuilder: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    ChainSpec: OpHardforks + Clone,
    EvmF: alloy_evm::EvmFactory<
        Tx: FromRecoveredTx<ReceiptBuilder::Transaction>
                + FromTxWithEncoded<ReceiptBuilder::Transaction>,
    >,
    Self: 'static,
{
    type EvmFactory = EvmF;
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
    /// Whether this is the first block of MiniRex spec
    pub first_mini_rex_block: bool,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// The block's extra data.
    pub extra_data: Bytes,
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
            spec.is_isthmus_active_at_timestamp(timestamp),
            "mega-evm assumes Isthmus hardfork is always active"
        );
        assert!(
            spec.is_canyon_active_at_timestamp(timestamp),
            "mega-evm assumes Canyon hardfork is always active"
        );
        assert!(
            !spec.is_regolith_active_at_timestamp(timestamp),
            "mega-evm assumes Regolith hardfork is not active"
        );
        Self {
            ctx,
            chain_spec: spec.clone(),
            receipt_builder,
            receipts: Vec::new(),
            gas_used: 0,
            evm,
            system_caller: SystemCaller::new(spec.clone()),
        }
    }
}

/// Implementation of `alloy_evm::block::BlockExecutor` for `MegaETH` block executor.
///
/// This implementation delegates all block execution operations to the underlying
/// Optimism block executor while providing MegaETH-specific customizations through
/// the configured chain specification and EVM factory.
impl<'db, DB, E, C, R> alloy_evm::block::BlockExecutor for MegaBlockExecutor<C, E, R>
where
    DB: Database + 'db,
    C: OpHardforks,
    E: alloy_evm::Evm<
        DB = &'db mut State<DB>,
        Tx: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
    >,
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
{
    type Transaction = R::Transaction;

    type Receipt = R::Receipt;

    type Evm = E;

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

        if !f(&result).should_commit() {
            return Ok(None);
        }

        self.system_caller.on_state(StateChangeSource::Transaction(self.receipts.len()), &state);

        let gas_used = result.gas_used();

        // append gas used
        self.gas_used += gas_used;

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
                        // transactions.
                        deposit_receipt_version: (is_deposit &&
                            self.chain_spec.is_canyon_active_at_timestamp(
                                self.evm.block().timestamp.saturating_to(),
                            ))
                        .then_some(1),
                    })
                }
            },
        );

        self.evm.db_mut().commit(state);

        Ok(Some(gas_used))
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
