use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{block::StateDB, Database, EvmEnv, EvmFactory, FromRecoveredTx, FromTxWithEncoded};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::{Bytes, B256};
use revm::{database::State, inspector::NoOpInspector, Inspector};

use crate::{BlockLimits, MegaBlockExecutor, MegaEvm, MegaHardforks, MegaSpecId, MegaTxEnvelope};

/// `MegaETH` block executor factory.
///
/// A factory for creating block executors configured with MegaETH-specific specifications
/// and optimizations. This factory encapsulates the chain specification, EVM factory,
/// and receipt builder needed to create block executors that support `MegaETH` features
/// such as enhanced security measures and increased contract size limits.
///
/// # Generic Parameters
///
/// - `Hardforks`: The hardforks implementing [`MegaHardforks`]
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
pub struct MegaBlockExecutorFactory<Hardforks, EvmF, ReceiptBuilder> {
    receipt_builder: ReceiptBuilder,
    hardforks: Hardforks,
    evm_factory: EvmF,
}

impl<Hardforks, EvmF, ReceiptBuilder> MegaBlockExecutorFactory<Hardforks, EvmF, ReceiptBuilder>
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
    pub fn new(hardforks: Hardforks, evm_factory: EvmF, receipt_builder: ReceiptBuilder) -> Self {
        Self { receipt_builder, hardforks, evm_factory }
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

impl<Hardforks, ExtEnvFactory, ReceiptBuilder>
    MegaBlockExecutorFactory<Hardforks, crate::MegaEvmFactory<ExtEnvFactory>, ReceiptBuilder>
where
    Hardforks: MegaHardforks + Clone,
    ReceiptBuilder: OpReceiptBuilder<Transaction: Transaction + Encodable2718> + Clone,
    crate::MegaTransaction: FromRecoveredTx<ReceiptBuilder::Transaction>,
    ExtEnvFactory: crate::ExternalEnvFactory + Clone,
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
        block_ctx: MegaBlockExecutionCtx,
        evm_env: EvmEnv<MegaSpecId>,
    ) -> MegaBlockExecutor<
        Hardforks,
        MegaEvm<&'a mut State<DB>, NoOpInspector, ExtEnvFactory::EnvTypes>,
        ReceiptBuilder,
    >
    where
        DB: Database + 'a,
    {
        let runtime_limits = block_ctx.block_limits.to_evm_tx_runtime_limits();
        let evm = self.evm_factory.create_evm(db, evm_env).with_tx_runtime_limits(runtime_limits);
        MegaBlockExecutor::new(evm, block_ctx, self.hardforks.clone(), self.receipt_builder.clone())
    }

    /// Create a new block executor with a read-only inspector its type's author has declared
    /// [`TrustedObserver`](crate::TrustedObserver).
    ///
    /// The declaration is what the canonical block-execution path admits an inspected transaction
    /// on, so this is the entry a node tracing block production or validation takes.
    /// [`create_executor_with_inspector`](Self::create_executor_with_inspector) builds an executor
    /// that refuses every transaction it is given.
    ///
    /// A `revm-inspectors` tracer cannot be declared where both it and the trait are foreign, so a
    /// node writes a forwarding newtype of its own and declares that; `bin/mega-evme`'s replay
    /// command is the shape to copy.
    ///
    /// # Parameters
    ///
    /// - `db`: The database to use for EVM state.
    /// - `evm_env`: The EVM environment, including block and config environments.
    /// - `block_ctx`: The block execution context for tracking access patterns.
    /// - `inspector`: The declared read-only inspector to observe execution with.
    pub fn create_executor_with_trusted_inspector<'a, DB, I>(
        &self,
        db: &'a mut State<DB>,
        block_ctx: MegaBlockExecutionCtx,
        evm_env: EvmEnv<MegaSpecId>,
        inspector: I,
    ) -> MegaBlockExecutor<
        Hardforks,
        MegaEvm<&'a mut State<DB>, I, ExtEnvFactory::EnvTypes>,
        ReceiptBuilder,
    >
    where
        DB: Database + 'a,
        I: Inspector<crate::MegaContext<&'a mut State<DB>, ExtEnvFactory::EnvTypes>>
            + crate::TrustedObserver
            + 'a,
    {
        let runtime_limits = block_ctx.block_limits.to_evm_tx_runtime_limits();
        let evm = self
            .evm_factory
            .create_evm(db, evm_env)
            .with_trusted_inspector(inspector)
            .with_tx_runtime_limits(runtime_limits);
        MegaBlockExecutor::new(evm, block_ctx, self.hardforks.clone(), self.receipt_builder.clone())
    }

    /// Create a new block executor with an inspector that carries no read-only declaration.
    ///
    /// The executor this builds refuses every transaction it is asked to run or admit, with
    /// [`MegaBlockExecutionError::UndeclaredInspector`](
    /// crate::MegaBlockExecutionError::UndeclaredInspector) — the canonical path admits an
    /// inspected transaction only on a [`TrustedObserver`](crate::TrustedObserver) declaration,
    /// which this entry's bound does not ask for. It stays because the EVM underneath it is
    /// reachable through [`MegaBlockExecutor::evm_mut`], which an embedder can drive itself.
    ///
    /// A tracer belongs on
    /// [`create_executor_with_trusted_inspector`](Self::create_executor_with_trusted_inspector).
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
        block_ctx: MegaBlockExecutionCtx,
        evm_env: EvmEnv<MegaSpecId>,
        inspector: I,
    ) -> MegaBlockExecutor<
        Hardforks,
        MegaEvm<&'a mut State<DB>, I, ExtEnvFactory::EnvTypes>,
        ReceiptBuilder,
    >
    where
        DB: Database + 'a,
        I: Inspector<crate::MegaContext<&'a mut State<DB>, ExtEnvFactory::EnvTypes>> + 'a,
    {
        let runtime_limits = block_ctx.block_limits.to_evm_tx_runtime_limits();
        let evm = self
            .evm_factory
            .create_evm_with_inspector(db, evm_env, inspector)
            .with_tx_runtime_limits(runtime_limits);
        MegaBlockExecutor::new(evm, block_ctx, self.hardforks.clone(), self.receipt_builder.clone())
    }
}

impl<Hardforks, ExtEnvFactory, ReceiptBuilder> alloy_evm::block::BlockExecutorFactory
    for MegaBlockExecutorFactory<Hardforks, crate::MegaEvmFactory<ExtEnvFactory>, ReceiptBuilder>
where
    ReceiptBuilder: OpReceiptBuilder<Transaction = MegaTxEnvelope, Receipt: TxReceipt>,
    MegaTxEnvelope: alloy_consensus::TransactionEnvelope,
    Hardforks: MegaHardforks + Clone,
    ExtEnvFactory: crate::ExternalEnvFactory + Clone,
    crate::MegaTransaction: FromRecoveredTx<ReceiptBuilder::Transaction>
        + FromTxWithEncoded<ReceiptBuilder::Transaction>,
    Self: 'static,
{
    type EvmFactory = crate::MegaEvmFactory<ExtEnvFactory>;
    type TxExecutionResult = crate::MegaBlockTxResult<
        <ReceiptBuilder::Transaction as alloy_consensus::TransactionEnvelope>::TxType,
    >;
    type ExecutionCtx<'a> = MegaBlockExecutionCtx;
    type Transaction = ReceiptBuilder::Transaction;
    type Receipt = ReceiptBuilder::Receipt;
    type Executor<
        'a,
        DB: StateDB,
        I: Inspector<<Self::EvmFactory as alloy_evm::EvmFactory>::Context<DB>>,
    > = MegaBlockExecutor<
        &'a Hardforks,
        <Self::EvmFactory as alloy_evm::EvmFactory>::Evm<DB, I>,
        &'a ReceiptBuilder,
    >;

    fn evm_factory(&self) -> &Self::EvmFactory {
        self.evm_factory_ref()
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as alloy_evm::EvmFactory>::Evm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<<Self::EvmFactory as alloy_evm::EvmFactory>::Context<DB>>,
    {
        // Nothing is checked about the inspector here. This entry takes an EVM the caller built,
        // so its inspector may or may not carry a declaration, and the answer is a runtime one
        // the executor's own entries ask per transaction — as an error that fails the block, not
        // an assertion that stops the process. See `MegaBlockExecutionError::UndeclaredInspector`.

        // Synchronize EVM tx runtime limits with the block context's BlockLimits.
        // This mirrors the inherent factory paths above which apply this
        // unconditionally on every spec since introduction. Without this, the
        // trait impl path silently ran against whatever limits the caller did
        // or did not pre-apply via with_tx_runtime_limits, leaving an asymmetry
        // between the inherent and trait construction routes.
        let runtime_limits = ctx.block_limits.to_evm_tx_runtime_limits();
        let evm = evm.with_tx_runtime_limits(runtime_limits);
        MegaBlockExecutor::new(evm, ctx, &self.hardforks, &self.receipt_builder)
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

    /// The block limits.
    pub block_limits: BlockLimits,
}

impl MegaBlockExecutionCtx {
    /// Create a new block execution context with default limits.
    pub fn new(
        parent_hash: B256,
        parent_beacon_block_root: Option<B256>,
        extra_data: Bytes,
        block_limits: BlockLimits,
    ) -> Self {
        Self { parent_hash, parent_beacon_block_root, extra_data, block_limits }
    }
}
