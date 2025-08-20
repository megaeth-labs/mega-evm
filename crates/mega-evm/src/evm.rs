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
        EthFrame, EvmTr, FrameInitOrResult, PrecompileProvider, SystemCallTx,
    },
    inspector::{InspectorHandler, NoOpInspector},
    interpreter::{
        interpreter::EthInterpreter, InputsImpl, Interpreter, InterpreterResult, InterpreterTypes,
    },
    primitives::{Address, TxKind},
    state::EvmState,
    DatabaseCommit, ExecuteEvm, InspectEvm, Inspector, Journal, SystemCallEvm,
};

use crate::{
    Context, HaltReason, Handler, Instructions, IntoMegaethCfgEnv, Precompiles, SpecId,
    Transaction, TransactionError, TxType,
};

/// Factory producing [`MegaethEvm`]s.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct EvmFactory;

impl alloy_evm::EvmFactory for EvmFactory {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = Evm<DB, I>;
    type Context<DB: Database> = Context<DB>;
    type Tx = Transaction;
    type Error<DBError: core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, TransactionError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        evm_env: EvmEnv<Self::Spec>,
    ) -> Self::Evm<DB, revm::inspector::NoOpInspector> {
        let spec = evm_env.cfg_env().spec();
        let ctx = Context::new(db, spec)
            .with_tx(Transaction::default())
            .with_block(evm_env.block_env)
            .with_cfg(evm_env.cfg_env)
            .with_chain(L1BlockInfo::default());
        Evm::new(ctx, NoOpInspector)
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

/// `MegaethEvm` is the EVM implementation for `MegaETH`.
/// `MegaethEvm` wraps the `OpEvm` with customizations.
#[allow(missing_debug_implementations)]
pub struct Evm<DB: Database, INSP> {
    inner: revm::context::Evm<
        Context<DB>,
        INSP,
        Instructions<DB>,
        PrecompilesMap,
        EthFrame<EthInterpreter>,
    >,
    inspect: bool,
    /// Whether to disable the post-transaction reward to beneficiary in the [`Handler`].
    disable_beneficiary: bool,
}

impl<DB: Database, INSP> core::fmt::Debug for Evm<DB, INSP> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethEvm").field("inspect", &self.inspect).finish_non_exhaustive()
    }
}

impl<DB: Database, INSP> core::ops::Deref for Evm<DB, INSP> {
    type Target = revm::context::Evm<
        Context<DB>,
        INSP,
        Instructions<DB>,
        PrecompilesMap,
        EthFrame<EthInterpreter>,
    >;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<DB: Database, INSP> core::ops::DerefMut for Evm<DB, INSP> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<DB: Database, INSP> Evm<DB, INSP> {
    /// Creates a new [`MegaethEvm`] instance.
    pub fn new(context: Context<DB>, inspect: INSP) -> Self {
        let spec = context.megaeth_spec();
        let op_spec = context.cfg().spec();
        Self {
            inner: revm::context::Evm::new_with_inspector(
                context,
                inspect,
                Instructions::new(spec),
                PrecompilesMap::from_static(Precompiles::new_with_spec(op_spec).precompiles()),
            ),
            inspect: false,
            disable_beneficiary: false,
        }
    }

    /// Creates a new [`MegaethEvm`] instance with the given inspector enabled at runtime.
    pub fn with_inspector<I>(self, inspector: I) -> Evm<DB, I> {
        let disable_beneficiary = self.disable_beneficiary;
        let inner = revm::context::Evm::new_with_inspector(
            self.inner.ctx,
            inspector,
            self.inner.instruction,
            self.inner.precompiles,
        );
        Evm { inner, inspect: true, disable_beneficiary }
    }

    /// Enables inspector at runtime.
    pub fn enable_inspect(&mut self) {
        self.inspect = true;
    }

    /// Disables inspector at runtime.
    pub fn disable_inspect(&mut self) {
        self.inspect = false;
    }

    /// Disables the beneficiary reward.
    pub fn disable_beneficiary(&mut self) {
        self.disable_beneficiary = true;
    }
}

impl<DB: Database, INSP> Evm<DB, INSP> {
    /// Provides a reference to the block environment.
    #[inline]
    pub fn block_env_ref(&self) -> &BlockEnv {
        &self.ctx_ref().block
    }

    /// Provides a mutable reference to the block environment.
    #[inline]
    pub fn block_env_mut(&mut self) -> &mut BlockEnv {
        &mut self.ctx().block
    }

    /// Provides a reference to the journaled state.
    #[inline]
    pub fn journaled_state(&self) -> &Journal<DB> {
        &self.ctx_ref().journaled_state
    }

    /// Provides a mutable reference to the journaled state.
    #[inline]
    pub fn journaled_state_mut(&mut self) -> &mut Journal<DB> {
        &mut self.ctx().journaled_state
    }

    /// Consumes self and returns the journaled state.
    #[inline]
    pub fn into_journaled_state(self) -> Journal<DB> {
        self.inner.ctx.inner.journaled_state
    }
}

impl<DB: Database> PrecompileProvider<Context<DB>> for PrecompilesMap {
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: OpSpecId) -> bool {
        PrecompileProvider::<OpContext<DB>>::set_spec(self, spec)
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut Context<DB>,
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

impl<DB, INSP> revm::handler::EvmTr for Evm<DB, INSP>
where
    DB: Database,
{
    type Context = Context<DB>;

    type Instructions = Instructions<DB>;

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
        self.inner.frame_init(frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as revm::handler::FrameTr>::FrameResult,
    ) -> Result<
        Option<<Self::Frame as revm::handler::FrameTr>::FrameResult>,
        ContextDbError<Self::Context>,
    > {
        self.inner.frame_return_result(result)
    }
}

impl<DB, INSP> revm::inspector::InspectorEvmTr for Evm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<Context<DB>>,
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

impl<DB, INSP> alloy_evm::Evm for Evm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<Context<DB>>,
{
    type DB = DB;
    type Tx = Transaction;
    type Error = EVMError<DB::Error, TransactionError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
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
    /// Note: this funtion copies the logic in `alloy_op_evm::OpEvm::transact_system_call`.
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
        let spec = self.inner.ctx.megaeth_spec();
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

impl<DB, INSP> revm::ExecuteEvm for Evm<DB, INSP>
where
    DB: Database,
{
    type Tx = Transaction;
    type Block = BlockEnv;
    type State = EvmState;
    type Error = EVMError<DB::Error, TransactionError>;
    type ExecutionResult = ExecutionResult<HaltReason>;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.ctx().set_tx(tx);
        let mut h = Handler::<_, _, EthFrame<EthInterpreter>>::new(self.disable_beneficiary);
        revm::handler::Handler::run(&mut h, self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let mut h = Handler::<_, _, EthFrame<EthInterpreter>>::new(self.disable_beneficiary);
        revm::handler::Handler::run(&mut h, self).map(|result| {
            let state = self.finalize();
            ExecResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> revm::ExecuteCommitEvm for Evm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.ctx().db_mut().commit(state);
    }
}

impl<DB, INSP> revm::InspectEvm for Evm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<Context<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.ctx().set_tx(tx);
        let mut h = Handler::<_, _, EthFrame<EthInterpreter>>::new(self.disable_beneficiary);
        revm::inspector::InspectorHandler::inspect_run(&mut h, self)
    }
}

impl<DB, INSP> revm::InspectCommitEvm for Evm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<Context<DB>>,
{
}

impl<DB, INSP> revm::SystemCallEvm for Evm<DB, INSP>
where
    DB: Database,
{
    fn transact_system_call_with_caller(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.ctx().set_tx(<Transaction as SystemCallTx>::new_system_tx_with_caller(
            caller, contract, data,
        ));
        let mut h = Handler::<_, _, EthFrame<EthInterpreter>>::new(self.disable_beneficiary);
        revm::handler::Handler::run(&mut h, self)
    }
}
