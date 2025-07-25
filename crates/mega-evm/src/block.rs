use alloc::boxed::Box;
use alloy_consensus::{transaction::Recovered, Transaction, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutorFor, CommitChanges, ExecutableTx,
    },
    Database, FromRecoveredTx, FromTxWithEncoded,
};
use alloy_op_evm::{block::receipt_builder::OpReceiptBuilder, OpBlockExecutor};
use alloy_op_hardforks::OpHardforks;
use delegate::delegate;
use revm::{context::result::ExecutionResult, database::State, Inspector};

use crate::EvmFactory;

/// Block execution context for the `MegaETH` chain.
pub type BlockExecutionCtx = alloy_op_evm::block::OpBlockExecutionCtx;

/// `MegaETH` block executor factory.
#[derive(Debug, Clone)]
pub struct BlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder> {
    receipt_builder: ReceiptBuilder,
    spec: ChainSpec,
    evm_factory: EvmF,
}

impl<ChainSpec, EvmF, ReceiptBuilder> BlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder> {
    /// Create a new block executor factory.
    pub fn new(spec: ChainSpec, evm_factory: EvmF, receipt_builder: ReceiptBuilder) -> Self {
        Self { receipt_builder, spec, evm_factory }
    }
}

impl<ChainSpec, EvmF, ReceiptBuilder> alloy_evm::block::BlockExecutorFactory
    for BlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder>
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
    type ExecutionCtx<'a> = BlockExecutionCtx;
    type Transaction = ReceiptBuilder::Transaction;
    type Receipt = ReceiptBuilder::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
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
        BlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
    }
}

/// Block executor for the `MegaETH` chain.
pub struct BlockExecutor<C, E, R: OpReceiptBuilder> {
    inner: OpBlockExecutor<E, R, C>,
}

impl<C, E, R: OpReceiptBuilder> core::fmt::Debug for BlockExecutor<C, E, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethBlockExecutor").finish_non_exhaustive()
    }
}

impl<C, E, R> BlockExecutor<C, E, R>
where
    C: OpHardforks + Clone,
    E: alloy_evm::Evm<Tx: FromRecoveredTx<R::Transaction>>,
    R: OpReceiptBuilder,
{
    /// Create a new block executor.
    pub fn new(evm: E, ctx: BlockExecutionCtx, spec: C, receipt_builder: R) -> Self {
        Self { inner: OpBlockExecutor::new(evm, ctx, spec, receipt_builder) }
    }
}

impl<'db, DB, E, C, R> alloy_evm::block::BlockExecutor for BlockExecutor<C, E, R>
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

    delegate! {
        to self.inner {
            fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError>;
            fn execute_transaction_with_commit_condition(
                &mut self,
                tx: impl ExecutableTx<Self>,
                f: impl FnOnce(&ExecutionResult<<Self::Evm as alloy_evm::Evm>::HaltReason>) -> CommitChanges,
            ) -> Result<Option<u64>, BlockExecutionError>;
            fn finish(self) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError>;
            fn set_state_hook(&mut self, hook: Option<Box<dyn alloy_evm::block::OnStateHook>>);
            fn evm_mut(&mut self) -> &mut Self::Evm;
            fn evm(&self) -> &Self::Evm;
        }
    }
}
