use alloy_consensus::{Transaction, TxReceipt};
use alloy_eips::Encodable2718;
use alloy_evm::{block::BlockExecutorFor, Database, FromRecoveredTx, FromTxWithEncoded};
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_op_hardforks::OpHardforks;
use alloy_primitives::{Bytes, B256};
use revm::{database::State, inspector::NoOpInspector, Inspector};

use crate::{BlockLimits, MegaBlockExecutor, MegaEvm, MegaEvmEnvAndSettings, MegaTxEnvelope};

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
    pub fn create_executor_with_config<'a, DB>(
        &self,
        db: &'a mut State<DB>,
        block_ctx: MegaBlockExecutionCtx,
        evm_config: MegaEvmEnvAndSettings,
    ) -> MegaBlockExecutor<
        ChainSpec,
        MegaEvm<&'a mut State<DB>, NoOpInspector, ExtEnvs>,
        ReceiptBuilder,
    >
    where
        DB: Database + 'a,
    {
        let evm = self.evm_factory.create_evm_with_config(db, evm_config);
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
    pub fn create_executor_with_config_and_inspector<'a, DB, I>(
        &self,
        db: &'a mut State<DB>,
        block_ctx: MegaBlockExecutionCtx,
        evm_config: MegaEvmEnvAndSettings,
        inspector: I,
    ) -> MegaBlockExecutor<ChainSpec, MegaEvm<&'a mut State<DB>, I, ExtEnvs>, ReceiptBuilder>
    where
        DB: Database + 'a,
        I: Inspector<crate::MegaContext<&'a mut State<DB>, ExtEnvs>> + 'a,
    {
        let evm = self.evm_factory.create_evm_with_config_and_inspector(db, evm_config, inspector);
        MegaBlockExecutor::new(evm, block_ctx, self.spec.clone(), self.receipt_builder.clone())
    }
}

impl<ChainSpec, ExtEnvs, ReceiptBuilder> alloy_evm::block::BlockExecutorFactory
    for MegaBlockExecutorFactory<ChainSpec, crate::MegaEvmFactory<ExtEnvs>, ReceiptBuilder>
where
    ReceiptBuilder: OpReceiptBuilder<Transaction = MegaTxEnvelope, Receipt: TxReceipt>,
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

    /// The block limits.
    pub block_limits: BlockLimits,
}

impl Default for MegaBlockExecutionCtx {
    fn default() -> Self {
        Self {
            parent_hash: B256::ZERO,
            parent_beacon_block_root: None,
            extra_data: Bytes::new(),
            block_limits: BlockLimits::default(),
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

    /// Set the block limits.
    pub fn with_block_limits(mut self, limits: BlockLimits) -> Self {
        self.block_limits = limits;
        self
    }
}
