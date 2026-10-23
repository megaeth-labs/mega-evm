//! Factory of block executors for alloy-evm consumers.

use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    block::{BlockExecutorFactory, StateDB},
    FromRecoveredTx, FromTxWithEncoded,
};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use revm::Inspector;

use crate::{
    ExternalEnvFactory, MegaBlockExecutionCtx, MegaBlockExecutor, MegaBlockTxResult, MegaContext,
    MegaEvm, MegaEvmFactory, MegaHardforks, MegaTransaction,
};

/// Creates the block executors a node drives.
///
/// It holds what every block of the chain shares — the hardfork schedule, the EVM factory and the
/// receipt builder — and each block adds its own [`MegaBlockExecutionCtx`].
#[derive(Clone, Debug, Default)]
pub struct MegaBlockExecutorFactory<R, Spec, EvmF> {
    receipt_builder: R,
    spec: Spec,
    evm_factory: EvmF,
}

impl<R, Spec, EvmF> MegaBlockExecutorFactory<R, Spec, EvmF> {
    /// Creates a factory over a receipt builder, a hardfork schedule and an EVM factory.
    ///
    /// The executors hold every block to its spec's limits, whatever the EVM they run on was
    /// created with; the EVMs the EVM factory creates outside block execution run under the same
    /// ones ([`MegaEvmFactory`]).
    pub const fn new(receipt_builder: R, spec: Spec, evm_factory: EvmF) -> Self {
        Self { receipt_builder, spec, evm_factory }
    }

    /// The receipt builder.
    pub const fn receipt_builder(&self) -> &R {
        &self.receipt_builder
    }

    /// The chain's hardfork schedule.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }

    /// The EVM factory.
    pub const fn evm_factory_ref(&self) -> &EvmF {
        &self.evm_factory
    }
}

impl<R, Spec, ExtEnvFactory> BlockExecutorFactory
    for MegaBlockExecutorFactory<R, Spec, MegaEvmFactory<ExtEnvFactory>>
where
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt> + 'static,
    Spec: MegaHardforks + 'static,
    ExtEnvFactory: ExternalEnvFactory + 'static,
    MegaTransaction: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
    Self: 'static,
{
    type EvmFactory = MegaEvmFactory<ExtEnvFactory>;
    type ExecutionCtx<'a> = MegaBlockExecutionCtx;
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type TxExecutionResult =
        MegaBlockTxResult<<R::Transaction as alloy_consensus::TransactionEnvelope>::TxType>;
    type Executor<'a, DB: StateDB, I: Inspector<MegaContext<DB, ExtEnvFactory::EnvTypes>>> =
        MegaBlockExecutor<MegaEvm<DB, I, ExtEnvFactory::EnvTypes>, &'a R, &'a Spec>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    /// Creates an executor over an EVM the caller built.
    ///
    /// The spec's limits are installed by [`MegaBlockExecutor::new`], so an EVM the caller built
    /// with others still runs the block's transactions under them.
    ///
    /// The EVM may carry any inspector: the engine does not refuse one, and a node decides which
    /// inspectors run on block production.
    fn create_executor<'a, DB, I>(
        &'a self,
        evm: MegaEvm<DB, I, ExtEnvFactory::EnvTypes>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<MegaContext<DB, ExtEnvFactory::EnvTypes>>,
    {
        MegaBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
    }
}
