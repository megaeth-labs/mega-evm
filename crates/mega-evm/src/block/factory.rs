//! Factory of block executors for alloy-evm consumers.

use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{
    block::{BlockExecutorFactory, StateDB},
    EvmEnv, EvmFactory, FromRecoveredTx, FromTxWithEncoded,
};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use revm::Inspector;

use crate::{
    ExternalEnvFactory, MegaBlockExecutionCtx, MegaBlockExecutor, MegaBlockTxResult, MegaContext,
    MegaEvm, MegaEvmFactory, MegaHardforks, MegaSpecId, MegaTransaction, TrustedObserver,
};

/// Creates the block executors a node drives.
///
/// It holds what every block of the chain shares — the hardfork schedule, the EVM factory and
/// the receipt builder — and each block adds its own [`MegaBlockExecutionCtx`].
#[derive(Clone, Debug, Default)]
pub struct MegaBlockExecutorFactory<R, Spec, EvmF> {
    receipt_builder: R,
    spec: Spec,
    evm_factory: EvmF,
}

impl<R, Spec, EvmF> MegaBlockExecutorFactory<R, Spec, EvmF> {
    /// Creates a factory over a receipt builder, a hardfork schedule and an EVM factory.
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

impl<R, Spec, ExtEnvFactory> MegaBlockExecutorFactory<R, Spec, MegaEvmFactory<ExtEnvFactory>>
where
    ExtEnvFactory: ExternalEnvFactory,
    Spec: MegaHardforks,
    R: OpReceiptBuilder,
{
    /// Creates an executor over an EVM that runs a declared observer.
    ///
    /// This is the one way an inspected transaction reaches a block: the inspector's type
    /// declares that it writes nothing back ([`TrustedObserver`]), so what the block executes is
    /// what the chain executes. An inspector without that declaration is refused by
    /// [`apply_pre_execution_changes`](alloy_evm::block::BlockExecutor::apply_pre_execution_changes).
    pub fn create_executor_with_trusted_inspector<DB, I>(
        &self,
        db: DB,
        evm_env: EvmEnv<MegaSpecId>,
        ctx: MegaBlockExecutionCtx,
        inspector: I,
    ) -> MegaBlockExecutor<MegaEvm<DB, I, ExtEnvFactory::EnvTypes>, &R, &Spec>
    where
        DB: StateDB,
        I: TrustedObserver + Inspector<MegaContext<DB, ExtEnvFactory::EnvTypes>>,
    {
        let evm = self
            .evm_factory
            .create_evm(db, evm_env)
            .with_trusted_inspector(inspector)
            .with_tx_runtime_limits(ctx.block_limits.to_evm_tx_runtime_limits());
        MegaBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
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
    /// The block's transaction-level limits are installed here, so the two ways to reach an
    /// executor — this one and
    /// [`create_executor_with_trusted_inspector`](Self::create_executor_with_trusted_inspector) —
    /// run every transaction under the same limits, whatever the caller did or did not apply.
    fn create_executor<'a, DB, I>(
        &'a self,
        evm: MegaEvm<DB, I, ExtEnvFactory::EnvTypes>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<MegaContext<DB, ExtEnvFactory::EnvTypes>>,
    {
        let evm = evm.with_tx_runtime_limits(ctx.block_limits.to_evm_tx_runtime_limits());
        MegaBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
    }
}
