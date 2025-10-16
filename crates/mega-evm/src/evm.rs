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

use core::convert::Infallible;

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use alloy_primitives::Bytes;
use op_revm::{L1BlockInfo, OpContext, OpSpecId};
use revm::{
    context::{
        result::{EVMError, ExecResultAndState, ExecutionResult, ResultAndState},
        BlockEnv, ContextSetters, ContextTr, FrameStack,
    },
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::InstructionProvider,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult, ItemOrResult, PrecompileProvider,
        SystemCallTx,
    },
    inspector::NoOpInspector,
    interpreter::{
        interpreter::EthInterpreter, FrameInput, InputsImpl, InstructionResult, InterpreterAction,
        InterpreterResult,
    },
    primitives::Address,
    state::EvmState,
    DatabaseCommit, InspectEvm, Inspector, Journal, SystemCallEvm,
};

use crate::{
    constants, create_exceeding_limit_frame_result, mark_interpreter_result_as_exceeding_limit,
    DefaultExternalEnvs, ExternalEnvs, IntoMegaethCfgEnv, MegaContext, MegaHaltReason, MegaHandler,
    MegaInstructions, MegaPrecompiles, MegaSpecId, MegaTransaction, MegaTransactionError,
};

/// Factory for creating `MegaETH` EVM instances.
///
/// The `EvmFactory` is responsible for creating EVM instances configured with `MegaETH`-specific
/// specifications and optimizations. It encapsulates the `external_envs` service and provides
/// methods to create EVM instances with different configurations.
///
/// # Type Parameters
///
/// - `Oracle`: The `external_envs` service to provide deterministic external information during EVM
///   execution. Must implement [`ExternalEnvs`] and [`Clone`] traits.
///
/// # Usage
///
/// ```rust
/// use alloy_evm::{EvmEnv, EvmFactory};
/// use mega_evm::{DefaultExternalEnvs, MegaEvmFactory, MegaSpecId};
/// use revm::database::{CacheDB, EmptyDB};
///
/// // Create a factory with default external_envs
/// let factory = MegaEvmFactory::default();
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
/// customizations through the configured `external_envs` service and chain specifications.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct MegaEvmFactory<ExtEnvs> {
    /// The `external_envs` service to provide deterministic external information during EVM
    /// execution.
    external_envs: ExtEnvs,
}

impl Default for MegaEvmFactory<DefaultExternalEnvs<Infallible>> {
    /// Creates a new [`EvmFactory`] instance with the default [`DefaultExternalEnvs`].
    ///
    /// This is the recommended way to create a factory when no custom `external_envs` is needed.
    /// The `DefaultExternalEnvs` provides a no-operation implementation that doesn't perform
    /// any external environment queries.
    fn default() -> Self {
        Self::new(DefaultExternalEnvs::<Infallible>::new())
    }
}

impl<ExtEnvs> MegaEvmFactory<ExtEnvs> {
    /// Creates a new [`EvmFactory`] instance with the given `external_envs`.
    ///
    /// # Parameters
    ///
    /// - `external_envs`: The `external_envs` service to provide deterministic external information
    ///   during EVM execution
    ///
    /// # Returns
    ///
    /// A new `EvmFactory` instance configured with the provided `external_envs`.
    pub fn new(external_envs: ExtEnvs) -> Self {
        Self { external_envs }
    }

    /// Provides a reference to the external environments.
    pub fn external_envs_ref(&self) -> &ExtEnvs {
        &self.external_envs
    }

    /// Provides a mutable reference to the external environments.
    pub fn external_envs_mut(&mut self) -> &mut ExtEnvs {
        &mut self.external_envs
    }
}

impl<ExtEnvs> alloy_evm::EvmFactory for MegaEvmFactory<ExtEnvs>
where
    ExtEnvs: ExternalEnvs + Clone,
{
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = MegaEvm<DB, I, ExtEnvs>;
    type Context<DB: Database> = MegaContext<DB, ExtEnvs>;
    type Tx = MegaTransaction;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, MegaTransactionError>;
    type HaltReason = MegaHaltReason;
    type Spec = MegaSpecId;
    type Precompiles = PrecompilesMap;

    /// Creates a new `Evm` instance with the provided database and EVM environment.
    ///
    /// This method constructs a new `Context` using the given database, the specification from the
    /// EVM environment, and the factory's `external_envs`. It then sets up the transaction, block,
    /// config, and chain environment for the context, and finally returns a new `Evm` instance
    /// using the [`NoOpInspector`] as the default inspector.
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
        let spec = evm_env.cfg_env().spec;
        let ctx = MegaContext::new(db, spec, self.external_envs.clone())
            .with_tx(MegaTransaction::default())
            .with_block(evm_env.block_env)
            .with_cfg(evm_env.cfg_env)
            .with_chain(L1BlockInfo::default());
        MegaEvm::new(ctx)
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
/// - `Oracle`: The `external_envs` type implementing [`ExternalEnvs`]
///
/// # Implementation Details
///
/// The EVM uses delegation to efficiently wrap the underlying Optimism EVM while providing
/// `MegaETH`-specific customizations through the configured context, instructions, and precompiles.
#[allow(missing_debug_implementations)]
#[allow(clippy::type_complexity)]
pub struct MegaEvm<DB: Database, INSP, ExtEnvs: ExternalEnvs> {
    inner: revm::context::Evm<
        MegaContext<DB, ExtEnvs>,
        INSP,
        MegaInstructions<DB, ExtEnvs>,
        PrecompilesMap,
        EthFrame<EthInterpreter>,
    >,
    /// Whether to enable the inspector at runtime.
    inspect: bool,
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvs> core::fmt::Debug for MegaEvm<DB, INSP, ExtEnvs> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethEvm").field("inspect", &self.inspect).finish_non_exhaustive()
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvs> core::ops::Deref for MegaEvm<DB, INSP, ExtEnvs> {
    type Target = revm::context::Evm<
        MegaContext<DB, ExtEnvs>,
        INSP,
        MegaInstructions<DB, ExtEnvs>,
        PrecompilesMap,
        EthFrame<EthInterpreter>,
    >;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvs> core::ops::DerefMut for MegaEvm<DB, INSP, ExtEnvs> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvs> MegaEvm<DB, NoOpInspector, ExtEnvs> {
    /// Creates a new `MegaETH` EVM instance.
    ///
    /// # Parameters
    ///
    /// - `context`: The `MegaETH` context containing database, configuration, and `external_envs`
    /// - `inspect`: The inspector to use for debugging and monitoring
    ///
    /// # Returns
    ///
    /// A new `Evm` instance configured with the provided context and inspector.
    pub fn new(context: MegaContext<DB, ExtEnvs>) -> Self {
        let spec = context.mega_spec();
        Self {
            inner: revm::context::Evm::new_with_inspector(
                context,
                NoOpInspector,
                MegaInstructions::new(spec),
                PrecompilesMap::from_static(MegaPrecompiles::new_with_spec(spec).precompiles()),
            ),
            inspect: false,
        }
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvs> MegaEvm<DB, INSP, ExtEnvs> {
    /// Creates a new `MegaETH` EVM instance with the given inspector enabled at runtime.
    ///
    /// # Parameters
    ///
    /// - `inspector`: The new inspector to use for debugging and monitoring
    ///
    /// # Returns
    ///
    /// A new `Evm` instance with the specified inspector enabled.
    pub fn with_inspector<I>(self, inspector: I) -> MegaEvm<DB, I, ExtEnvs> {
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

impl<DB: Database, INSP, ExtEnvs: ExternalEnvs> MegaEvm<DB, INSP, ExtEnvs> {
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

impl<DB: Database, ExtEnvs: ExternalEnvs> PrecompileProvider<MegaContext<DB, ExtEnvs>>
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
        context: &mut MegaContext<DB, ExtEnvs>,
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

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::handler::EvmTr for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
{
    type Context = MegaContext<DB, ExtEnvs>;

    type Instructions = MegaInstructions<DB, ExtEnvs>;

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
        frame_init: <Self::Frame as revm::handler::FrameTr>::FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        let is_mini_rex_enabled = self.ctx().spec.is_enabled(MegaSpecId::MINI_REX);
        let additional_limit = self.ctx().additional_limit.clone();
        if is_mini_rex_enabled &&
            additional_limit.borrow_mut().before_frame_init(&frame_init).exceeded_limit()
        {
            // if the limit is exceeded, create an error frame result and return it directly
            let (gas_limit, return_memory_offset) = match &frame_init.frame_input {
                FrameInput::Create(inputs) => (inputs.gas_limit, None),
                FrameInput::Call(inputs) => {
                    (inputs.gas_limit, Some(inputs.return_memory_offset.clone()))
                }
                FrameInput::Empty => unreachable!(),
            };
            return Ok(FrameInitResult::Result(create_exceeding_limit_frame_result(
                gas_limit,
                return_memory_offset,
            )));
        }

        // call the inner frame_init function to initialize the frame
        let init_result = self.inner.frame_init(frame_init)?;

        // Apply the additional limits only when the `MINI_REX` spec is enabled.
        if is_mini_rex_enabled {
            if let ItemOrResult::Item(frame) = &init_result {
                additional_limit.borrow_mut().after_frame_init_on_frame(frame);
            }
        }

        Ok(init_result)
    }

    /// This method copies the logic from `revm::handler::EvmTr::frame_run` to and add additional
    /// logic before `process_next_action` to handle the additional limit.
    #[inline]
    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        let frame = self.inner.frame_stack.get();
        let context = &mut self.inner.ctx;
        let instructions = &mut self.inner.instruction;

        let mut action = frame.interpreter.run_plain(instructions.instruction_table(), context);

        // Apply the additional limits and gas cost only when the `MINI_REX` spec is enabled.
        if context.spec.is_enabled(MegaSpecId::MINI_REX) {
            if let InterpreterAction::Return(interpreter_result) = &mut action {
                // charge additional gas cost for the number of bytes
                if frame.data.is_create() && interpreter_result.is_ok() {
                    // if the creation is successful, charge the additional gas cost for the
                    // number of bytes. The EVM's original `CODEDEPOSIT` gas cost will be
                    // charged later in `process_next_action`. We only charge the difference
                    // here.
                    let additional_code_deposit_gas =
                        constants::mini_rex::CODEDEPOSIT_ADDITIONAL_GAS *
                            interpreter_result.output.len() as u64;
                    if !interpreter_result.gas.record_cost(additional_code_deposit_gas) {
                        // if out of gas, set the instruction result to OOG
                        interpreter_result.result = InstructionResult::OutOfGas;
                    }
                }

                // update additional limits
                let mut additional_limit = context.additional_limit.borrow_mut();
                if frame.data.is_create() &&
                    additional_limit.after_create_frame_run(interpreter_result).exceeded_limit()
                {
                    // if exceeded the limit, set the instruction result
                    mark_interpreter_result_as_exceeding_limit(interpreter_result);
                }
            }
        }

        frame.process_next_action(context, action).inspect(|i| {
            if i.is_result() {
                frame.set_finished(true);
            }
        })
    }

    fn frame_return_result(
        &mut self,
        mut result: <Self::Frame as revm::handler::FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as revm::handler::FrameTr>::FrameResult>,
        ContextDbError<Self::Context>,
    > {
        let ctx = self.ctx_ref();
        let is_mini_rex = ctx.spec.is_enabled(MegaSpecId::MINI_REX);
        // Apply the additional limits only when the `MINI_REX` spec is enabled.
        if is_mini_rex {
            // Return early if the limit is already exceeded before processing the child frame
            // return result.
            if ctx
                .additional_limit
                .borrow_mut()
                .is_exceeding_limit_result(result.instruction_result())
            {
                return Ok(Some(result));
            }

            // call the `on_frame_return` function to update the `AdditionalLimit` if the limit is
            // exceeded, return the error frame result
            if ctx
                .additional_limit
                .borrow_mut()
                .before_frame_return_result(&result, false)
                .exceeded_limit()
            {
                match &mut result {
                    FrameResult::Call(call_outcome) => {
                        mark_interpreter_result_as_exceeding_limit(&mut call_outcome.result)
                    }
                    FrameResult::Create(create_outcome) => {
                        mark_interpreter_result_as_exceeding_limit(&mut create_outcome.result)
                    }
                }
                return Ok(Some(result));
            }
        }

        // If the volatile data tracker already accessed, the gas in the returned frame must have
        // already been limited by the volatile data tracker's global limited gas. The remaining
        // gas in the returned frame should be recorded so that we can know how much
        // gas is left and how we should limit the parent frame's gas.
        let volatile_data_tracker = ctx.volatile_data_tracker.clone();
        let mut volatile_data_tracker = volatile_data_tracker.borrow_mut();
        let accessed_sensitive_data = volatile_data_tracker.accessed();
        if is_mini_rex && accessed_sensitive_data {
            let gas_remaining = result.gas().remaining();
            volatile_data_tracker.update_remained_gas(gas_remaining);
        }

        // call the inner frame_return_result function to return the frame result
        let mut inner_result = self.inner.frame_return_result(result);

        // After processing the frame return, we also to limit the parent frame's gas if oracle was
        // accessed This needs to happen AFTER inner.frame_return_result() because that's
        // when the parent frame has accured the remaining gas from the returned child frame and
        // becomes the current frame again.
        if is_mini_rex && accessed_sensitive_data {
            // Now the parent frame is the current frame
            if let Some(_index) = self.frame_stack().index() {
                let current_frame = self.frame_stack().get();
                volatile_data_tracker.detain_gas(&mut current_frame.interpreter.gas);
            } else {
                // if the current frame is the top-level transaction, limit the gas
                if let Ok(Some(inner_result)) = &mut inner_result {
                    volatile_data_tracker.detain_gas_in_frame_result(inner_result);
                }
            }
        }

        inner_result
    }
}

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::inspector::InspectorEvmTr for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
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
impl<DB, INSP, ExtEnvs: ExternalEnvs> alloy_evm::Evm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
{
    type DB = DB;
    type Tx = MegaTransaction;
    type Error = EVMError<DB::Error, MegaTransactionError>;
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

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::ExecuteEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
{
    type Tx = MegaTransaction;
    type Block = BlockEnv;
    type State = EvmState;
    type Error = EVMError<DB::Error, MegaTransactionError>;
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

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::ExecuteCommitEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.ctx().db_mut().commit(state);
    }
}

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::InspectEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
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

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::InspectCommitEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
{
}

impl<DB, INSP, ExtEnvs: ExternalEnvs> revm::SystemCallEvm for MegaEvm<DB, INSP, ExtEnvs>
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
