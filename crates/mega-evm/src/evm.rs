//! EVM implementation for the `MegaETH` chain.
//!
//! This module provides the core EVM implementation specifically tailored for the `MegaETH`
//! chain, built on top of the Optimism EVM (`op-revm`) with MegaETH-specific customizations
//! and optimizations.
//!
//! # Architecture
//!
//! The EVM implementation consists of two main components:
//!
//! 1. **`EvmFactory`**: Factory for creating EVM instances with `MegaETH` specifications
//! 2. **`Evm`**: The main EVM instance that wraps the Optimism EVM with `MegaETH` customizations
//!
//! # EVM Specifications
//!
//! `MegaETH` supports two EVM specifications:
//!
//! - **`EQUIVALENCE`**: Maintains equivalence with Optimism Isthmus EVM (default)
//! - **`MINI_REX`**: Enhanced version with quadratic LOG costs and disabled SELFDESTRUCT
//!
//! # Usage Example
//!
//! ```rust
//! use mega_evm::{Context, Evm, SpecId, Transaction};
//! use revm::{
//!     context::TxEnv,
//!     database::{CacheDB, EmptyDB},
//!     inspector::NoOpInspector,
//!     primitives::TxKind,
//! };
//!
//! // Create EVM instance with MINI_REX spec
//! let mut db = CacheDB::<EmptyDB>::default();
//! let spec = SpecId::MINI_REX;
//! let mut context = Context::new(db, spec);
//! let mut evm = Evm::new(context, NoOpInspector);
//!
//! // Execute transaction
//! let tx = Transaction {
//!     base: TxEnv {
//!         caller: address!("..."),
//!         // ... other fields
//!     },
//! };
//! let result = evm.transact_raw(tx);
//! ```

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use alloy_primitives::{Bytes, U256};
use op_revm::{L1BlockInfo, OpContext, OpSpecId};
use revm::{
    context::{
        result::{EVMError, ExecResultAndState, ExecutionResult, ResultAndState},
        BlockEnv, Cfg, ContextSetters, ContextTr, FrameStack, TxEnv,
    },
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::InstructionProvider,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult, PrecompileProvider, SystemCallTx,
    },
    inspector::{InspectorHandler, NoOpInspector},
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallOutcome, CreateOutcome,
        FrameInput, Gas, InputsImpl, InstructionResult, Interpreter, InterpreterResult,
        InterpreterTypes,
    },
    primitives::{Address, TxKind},
    state::EvmState,
    DatabaseCommit, ExecuteEvm, InspectEvm, Inspector, Journal, SystemCallEvm,
};

use crate::{
    exceeding_limit_frame_result, mark_frame_result_as_exceeding_limit, AdditionalLimit,
    BlockEnvAccess, ExternalEnvOracle, HostExt, IntoMegaethCfgEnv, MegaContext, MegaHaltReason,
    MegaHandler, MegaInstructions, MegaPrecompiles, MegaSpecId, MegaTransaction, MegaTxType,
    NoOpOracle, TransactionError,
};

/// Factory for creating `MegaETH` EVM instances.
///
/// The `EvmFactory` is responsible for creating EVM instances configured with `MegaETH`-specific
/// specifications and optimizations. It encapsulates the oracle service and provides methods
/// to create EVM instances with different configurations.
///
/// # Type Parameters
///
/// - `Oracle`: The oracle service to provide deterministic external information during EVM
///   execution. Must implement [`ExternalEnvOracle`] and [`Clone`] traits.
///
/// # Usage
///
/// ```rust
/// use mega_evm::{EvmFactory, NoOpOracle, SpecId};
/// use revm::{database::CacheDB, primitives::EmptyDB};
///
/// // Create a factory with default oracle
/// let factory = EvmFactory::default();
///
/// // Create EVM instance
/// let db = CacheDB::<EmptyDB>::default();
/// let evm_env = EvmEnv::default();
/// let evm = factory.create_evm(db, evm_env);
/// ```
///
/// # Implementation Details
///
/// The factory implements [`alloy_evm::EvmFactory`] and provides `MegaETH`-specific
/// customizations through the configured oracle service and chain specifications.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct MegaEvmFactory<Oracle> {
    /// The oracle service to provide deterministic external information during EVM execution.
    oracle: Oracle,
}

impl Default for MegaEvmFactory<NoOpOracle> {
    /// Creates a new [`EvmFactory`] instance with the default [`NoOpOracle`].
    ///
    /// This is the recommended way to create a factory when no custom oracle is needed.
    /// The `NoOpOracle` provides a no-operation implementation that doesn't perform
    /// any external environment queries.
    fn default() -> Self {
        Self::new(NoOpOracle)
    }
}

impl<Oracle> MegaEvmFactory<Oracle> {
    /// Creates a new [`EvmFactory`] instance with the given oracle.
    ///
    /// # Parameters
    ///
    /// - `oracle`: The oracle service to provide deterministic external information during EVM
    ///   execution
    ///
    /// # Returns
    ///
    /// A new `EvmFactory` instance configured with the provided oracle.
    pub fn new(oracle: Oracle) -> Self {
        Self { oracle }
    }
}

impl<Oracle> alloy_evm::EvmFactory for MegaEvmFactory<Oracle>
where
    Oracle: ExternalEnvOracle + Clone,
{
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = MegaEvm<DB, I, Oracle>;
    type Context<DB: Database> = MegaContext<DB, Oracle>;
    type Tx = MegaTransaction;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, TransactionError>;
    type HaltReason = MegaHaltReason;
    type Spec = MegaSpecId;
    type Precompiles = PrecompilesMap;

    /// Creates a new `Evm` instance with the provided database and EVM environment.
    ///
    /// This method constructs a new `Context` using the given database, the specification from the
    /// EVM environment, and the factory's oracle. It then sets up the transaction, block, config,
    /// and chain environment for the context, and finally returns a new `Evm` instance using the
    /// [`NoOpInspector`] as the default inspector.
    ///
    /// # Parameters
    ///
    /// - `db`: The database to use for EVM state.
    /// - `evm_env`: The EVM environment, including block and config environments.
    ///
    /// # Returns
    ///
    /// A new [`Evm`] instance configured with the provided database and environment.
    fn create_evm<DB: Database>(
        &self,
        db: DB,
        evm_env: EvmEnv<Self::Spec>,
    ) -> Self::Evm<DB, revm::inspector::NoOpInspector> {
        let spec = evm_env.cfg_env().spec();
        let ctx = MegaContext::new(db, spec, self.oracle.clone())
            .with_tx(MegaTransaction::default())
            .with_block(evm_env.block_env)
            .with_cfg(evm_env.cfg_env)
            .with_chain(L1BlockInfo::default());
        MegaEvm::new(ctx, NoOpInspector)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        Self::create_evm(self, db, input).with_inspector(inspector)
    }
}

/// The main EVM implementation for the `MegaETH` chain.
///
/// This struct wraps the underlying Optimism EVM (`OpEvm`) with `MegaETH`-specific customizations
/// and optimizations. It provides access to enhanced security features, increased limits, and
/// block environment access tracking capabilities.
///
/// # Type Parameters
///
/// - `DB`: The database type implementing [`Database`]
/// - `INSP`: The inspector type implementing [`Inspector`]
/// - `Oracle`: The oracle type implementing [`ExternalEnvOracle`]
///
/// # Usage
///
/// ```rust
/// use mega_evm::{Context, Evm, SpecId};
/// use revm::{database::CacheDB, inspector::NoOpInspector, primitives::EmptyDB};
///
/// let mut db = CacheDB::<EmptyDB>::default();
/// let spec = SpecId::MINI_REX;
/// let context = Context::new(db, spec);
/// let evm = Evm::new(context, NoOpInspector);
/// ```
///
/// # Implementation Details
///
/// The EVM uses delegation to efficiently wrap the underlying Optimism EVM while providing
/// `MegaETH`-specific customizations through the configured context, instructions, and precompiles.
#[allow(missing_debug_implementations)]
#[allow(clippy::type_complexity)]
pub struct MegaEvm<DB: Database, INSP, Oracle: ExternalEnvOracle> {
    inner: revm::context::Evm<
        MegaContext<DB, Oracle>,
        INSP,
        MegaInstructions<DB, Oracle>,
        PrecompilesMap,
        EthFrame<EthInterpreter>,
    >,
    /// Whether to enable the inspector at runtime.
    inspect: bool,
}

impl<DB: Database, INSP, Oracle: ExternalEnvOracle> core::fmt::Debug for MegaEvm<DB, INSP, Oracle> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethEvm").field("inspect", &self.inspect).finish_non_exhaustive()
    }
}

impl<DB: Database, INSP, Oracle: ExternalEnvOracle> core::ops::Deref for MegaEvm<DB, INSP, Oracle> {
    type Target = revm::context::Evm<
        MegaContext<DB, Oracle>,
        INSP,
        MegaInstructions<DB, Oracle>,
        PrecompilesMap,
        EthFrame<EthInterpreter>,
    >;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<DB: Database, INSP, Oracle: ExternalEnvOracle> core::ops::DerefMut
    for MegaEvm<DB, INSP, Oracle>
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<DB: Database, INSP, Oracle: ExternalEnvOracle> MegaEvm<DB, INSP, Oracle> {
    /// Creates a new `MegaETH` EVM instance.
    ///
    /// # Parameters
    ///
    /// - `context`: The `MegaETH` context containing database, configuration, and oracle
    /// - `inspect`: The inspector to use for debugging and monitoring
    ///
    /// # Returns
    ///
    /// A new `Evm` instance configured with the provided context and inspector.
    pub fn new(context: MegaContext<DB, Oracle>, inspect: INSP) -> Self {
        let spec = context.mega_spec();
        let op_spec = context.cfg().spec();
        Self {
            inner: revm::context::Evm::new_with_inspector(
                context,
                inspect,
                MegaInstructions::new(spec),
                PrecompilesMap::from_static(MegaPrecompiles::new_with_spec(op_spec).precompiles()),
            ),
            inspect: false,
        }
    }

    /// Creates a new `MegaETH` EVM instance with the given inspector enabled at runtime.
    ///
    /// # Parameters
    ///
    /// - `inspector`: The new inspector to use for debugging and monitoring
    ///
    /// # Returns
    ///
    /// A new `Evm` instance with the specified inspector enabled.
    pub fn with_inspector<I>(self, inspector: I) -> MegaEvm<DB, I, Oracle> {
        let inner = revm::context::Evm::new_with_inspector(
            self.inner.ctx,
            inspector,
            self.inner.instruction,
            self.inner.precompiles,
        );
        MegaEvm { inner, inspect: true }
    }

    /// Enables inspector at runtime.
    ///
    /// This allows the inspector to be activated during EVM execution for debugging
    /// and monitoring purposes without recreating the EVM instance.
    pub fn enable_inspect(&mut self) {
        self.inspect = true;
    }

    /// Disables inspector at runtime.
    ///
    /// This deactivates the inspector during EVM execution to improve performance
    /// when debugging is not needed.
    pub fn disable_inspect(&mut self) {
        self.inspect = false;
    }
}

impl<DB: Database, INSP, Oracle: ExternalEnvOracle> MegaEvm<DB, INSP, Oracle> {
    /// Provides a reference to the block environment.
    ///
    /// The block environment contains information about the current block being processed,
    /// including block number, timestamp, gas limit, and other block-specific data.
    #[inline]
    pub fn block_env_ref(&self) -> &BlockEnv {
        &self.ctx_ref().block
    }

    /// Provides a mutable reference to the block environment.
    ///
    /// This allows modification of block environment data during EVM execution,
    /// which is useful for testing and simulation scenarios.
    #[inline]
    pub fn block_env_mut(&mut self) -> &mut BlockEnv {
        &mut self.ctx().block
    }

    /// Provides a reference to the journaled state.
    ///
    /// The journaled state tracks all state changes during transaction execution,
    /// enabling rollback capabilities and state management.
    #[inline]
    pub fn journaled_state(&self) -> &Journal<DB> {
        &self.ctx_ref().journaled_state
    }

    /// Provides a mutable reference to the journaled state.
    ///
    /// This allows direct manipulation of the journaled state for advanced
    /// use cases and testing scenarios.
    #[inline]
    pub fn journaled_state_mut(&mut self) -> &mut Journal<DB> {
        &mut self.ctx().journaled_state
    }

    /// Consumes self and returns the journaled state.
    ///
    /// This is useful when you need to extract the final state after EVM execution
    /// and no longer need the EVM instance.
    #[inline]
    pub fn into_journaled_state(self) -> Journal<DB> {
        self.inner.ctx.inner.journaled_state
    }
}

impl<DB: Database, Oracle: ExternalEnvOracle> PrecompileProvider<MegaContext<DB, Oracle>>
    for PrecompilesMap
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: OpSpecId) -> bool {
        PrecompileProvider::<OpContext<DB>>::set_spec(self, spec)
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut MegaContext<DB, Oracle>,
        address: &Address,
        inputs: &InputsImpl,
        is_static: bool,
        gas_limit: u64,
    ) -> Result<Option<Self::Output>, String> {
        PrecompileProvider::<OpContext<DB>>::run(
            self, context, address, inputs, is_static, gas_limit,
        )
    }

    #[inline]
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        PrecompileProvider::<OpContext<DB>>::warm_addresses(self)
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        PrecompileProvider::<OpContext<DB>>::contains(self, address)
    }
}

impl<DB, INSP, Oracle: ExternalEnvOracle> revm::handler::EvmTr for MegaEvm<DB, INSP, Oracle>
where
    DB: Database,
{
    type Context = MegaContext<DB, Oracle>;

    type Instructions = MegaInstructions<DB, Oracle>;

    type Precompiles = PrecompilesMap;

    type Frame = EthFrame<EthInterpreter>;

    #[inline]
    fn ctx(&mut self) -> &mut Self::Context {
        &mut self.inner.ctx
    }

    #[inline]
    fn ctx_ref(&self) -> &Self::Context {
        &self.inner.ctx
    }

    #[inline]
    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        (&mut self.inner.ctx, &mut self.inner.instruction)
    }

    #[inline]
    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        (&mut self.inner.ctx, &mut self.inner.precompiles)
    }

    fn frame_stack(&mut self) -> &mut FrameStack<Self::Frame> {
        &mut self.inner.frame_stack
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as revm::handler::FrameTr>::FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        // we need to first get a reference to the `AdditionalLimit` before
        // calling frame_init to avoid borrowing issues
        let additional_limit = self.ctx().additional_limit.clone();
        let is_mini_rex_enabled = self.ctx().spec.is_enabled(MegaSpecId::MINI_REX);

        // call the inner frame_init function to initialize the frame
        let init_result = self.inner.frame_init(frame_input)?;

        // Apply the additional limits only when the `MINI_REX` spec is enabled.
        if is_mini_rex_enabled {
            // call the `on_frame_init` function to update the `AdditionalLimit`, if the limit is
            // exceeded, return the error frame result
            if additional_limit.borrow_mut().on_frame_init(&init_result).exceeded_limit() {
                let frame_result = match init_result {
                    revm::handler::ItemOrResult::Item(frame) => {
                        let (gas_limit, return_memory_offset) = match &frame.input {
                            FrameInput::Create(inputs) => (inputs.gas_limit, None),
                            FrameInput::Call(inputs) => {
                                (inputs.gas_limit, Some(inputs.return_memory_offset.clone()))
                            }
                            FrameInput::Empty => unreachable!(),
                        };
                        exceeding_limit_frame_result(gas_limit, return_memory_offset)
                    }
                    revm::handler::ItemOrResult::Result(frame_result) => {
                        mark_frame_result_as_exceeding_limit(frame_result)
                    }
                };
                return Ok(FrameInitResult::Result(frame_result));
            }
        }

        Ok(init_result)
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        mut result: <Self::Frame as revm::handler::FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as revm::handler::FrameTr>::FrameResult>,
        ContextDbError<Self::Context>,
    > {
        // Apply the additional limits only when the `MINI_REX` spec is enabled.
        if self.ctx_ref().spec.is_enabled(MegaSpecId::MINI_REX) {
            // Return early if the limit is already exceeded before processing the child frame return result.
            if self.ctx_ref().additional_limit.borrow().is_exceeding_limit_result(&result) {
                return Ok(Some(result));
            }

            // call the `on_frame_return` function to update the `AdditionalLimit` if the limit is
            // exceeded, return the error frame result
            if self
                .ctx_ref()
                .additional_limit
                .borrow_mut()
                .on_frame_return(&result)
                .exceeded_limit()
            {
                match &mut result {
                    FrameResult::Call(outcome) => {
                        outcome.result.result = AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT;
                    }
                    FrameResult::Create(outcome) => {
                        outcome.result.result = AdditionalLimit::EXCEEDING_LIMIT_INSTRUCTION_RESULT;
                    }
                }
                return Ok(Some(result));
            }
        }

        // call the inner frame_return_result function to return the frame result
        self.inner.frame_return_result(result)
    }
}

impl<DB, INSP, Oracle: ExternalEnvOracle> revm::inspector::InspectorEvmTr
    for MegaEvm<DB, INSP, Oracle>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, Oracle>>,
{
    type Inspector = INSP;

    fn inspector(&mut self) -> &mut Self::Inspector {
        &mut self.inner.inspector
    }

    fn ctx_inspector(&mut self) -> (&mut Self::Context, &mut Self::Inspector) {
        (&mut self.inner.ctx, &mut self.inner.inspector)
    }

    fn ctx_inspector_frame(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame) {
        (&mut self.inner.ctx, &mut self.inner.inspector, self.inner.frame_stack.get())
    }

    fn ctx_inspector_frame_instructions(
        &mut self,
    ) -> (&mut Self::Context, &mut Self::Inspector, &mut Self::Frame, &mut Self::Instructions) {
        (
            &mut self.inner.ctx,
            &mut self.inner.inspector,
            self.inner.frame_stack.get(),
            &mut self.inner.instruction,
        )
    }
}

/// Implementation of [`alloy_evm::Evm`] for `MegaETH` EVM.
///
/// This implementation provides the core EVM interface required by the Alloy EVM framework,
/// enabling seamless integration with Alloy-based applications while providing `MegaETH`-specific
/// customizations and optimizations.
impl<DB, INSP, Oracle: ExternalEnvOracle> alloy_evm::Evm for MegaEvm<DB, INSP, Oracle>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, Oracle>>,
{
    type DB = DB;
    type Tx = MegaTransaction;
    type Error = EVMError<DB::Error, TransactionError>;
    type HaltReason = MegaHaltReason;
    type Spec = MegaSpecId;
    type Precompiles = PrecompilesMap;
    type Inspector = INSP;

    fn block(&self) -> &BlockEnv {
        self.block_env_ref()
    }

    fn chain_id(&self) -> u64 {
        self.ctx_ref().cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            InspectEvm::inspect_tx(self, tx)
        } else {
            revm::ExecuteEvm::transact(self, tx)
        }
    }

    /// Transact a system call.
    ///
    /// This method enables system calls within the `MegaETH` EVM, following the same
    /// pattern as the Optimism EVM. System calls are special transactions that can
    /// interact with the underlying system without going through the normal transaction
    /// validation process.
    ///
    /// # Parameters
    ///
    /// - `caller`: The address making the system call
    /// - `contract`: The target contract address
    /// - `data`: The call data to execute
    ///
    /// # Returns
    ///
    /// The execution result and state changes from the system call.
    ///
    /// # Note
    ///
    /// This function copies the logic from `alloy_op_evm::OpEvm::transact_system_call`
    /// to maintain compatibility with the Optimism EVM system call interface.
    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.transact_system_call_with_caller_finalize(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>)
    where
        Self: Sized,
    {
        let spec = self.inner.ctx.mega_spec();
        let revm::Context { block: block_env, cfg: cfg_env, journaled_state, .. } =
            self.inner.ctx.into_inner();
        let cfg_env = cfg_env.into_megaeth_cfg(spec);
        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        (&self.inner.ctx.journaled_state.database, &self.inner.inspector, &self.inner.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        (
            &mut self.inner.ctx.journaled_state.database,
            &mut self.inner.inspector,
            &mut self.inner.precompiles,
        )
    }
}

impl<DB, INSP, Oracle: ExternalEnvOracle> revm::ExecuteEvm for MegaEvm<DB, INSP, Oracle>
where
    DB: Database,
{
    type Tx = MegaTransaction;
    type Block = BlockEnv;
    type State = EvmState;
    type Error = EVMError<DB::Error, TransactionError>;
    type ExecutionResult = ExecutionResult<MegaHaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.ctx().set_tx(tx);
        let mut h = MegaHandler::<_, _, EthFrame<EthInterpreter>>::new();
        revm::handler::Handler::run(&mut h, self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = MegaHandler::<_, _, EthFrame<EthInterpreter>>::new();
        revm::handler::Handler::run(&mut h, self).map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP, Oracle: ExternalEnvOracle> revm::ExecuteCommitEvm for MegaEvm<DB, INSP, Oracle>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.ctx().db_mut().commit(state);
    }
}

impl<DB, INSP, Oracle: ExternalEnvOracle> revm::InspectEvm for MegaEvm<DB, INSP, Oracle>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, Oracle>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.ctx().set_tx(tx);
        let mut h = MegaHandler::<_, _, EthFrame<EthInterpreter>>::new();
        revm::inspector::InspectorHandler::inspect_run(&mut h, self)
    }
}

impl<DB, INSP, Oracle: ExternalEnvOracle> revm::InspectCommitEvm for MegaEvm<DB, INSP, Oracle>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<MegaContext<DB, Oracle>>,
{
}

impl<DB, INSP, Oracle: ExternalEnvOracle> revm::SystemCallEvm for MegaEvm<DB, INSP, Oracle>
where
    DB: Database,
{
    fn transact_system_call_with_caller(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.ctx().set_tx(<MegaTransaction as SystemCallTx>::new_system_tx_with_caller(
            caller, contract, data,
        ));
        let mut h = MegaHandler::<_, _, EthFrame<EthInterpreter>>::new();
        revm::handler::Handler::run_system_call(&mut h, self)
    }
}
