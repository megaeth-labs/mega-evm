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

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;
use alloy_consensus::{Transaction, TxReceipt};
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

/// Block execution context for the `MegaETH` chain, aliasing the Optimism block execution
/// context.
pub type BlockExecutionCtx = alloy_op_evm::block::OpBlockExecutionCtx;

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
        MegaBlockExecutor::new(evm, ctx, &self.spec, &self.receipt_builder)
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
    inner: OpBlockExecutor<E, R, C>,
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
    pub fn new(evm: E, ctx: BlockExecutionCtx, spec: C, receipt_builder: R) -> Self {
        Self { inner: OpBlockExecutor::new(evm, ctx, spec, receipt_builder) }
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
