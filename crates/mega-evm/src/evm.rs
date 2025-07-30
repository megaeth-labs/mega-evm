#[cfg(not(feature = "std"))]
use alloc::vec::Vec;
use alloy_evm::{Database, EvmEnv};
use alloy_primitives::{Bytes, U256};
use op_revm::L1BlockInfo;
use revm::{
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        BlockEnv, Cfg, ContextSetters, ContextTr, TxEnv,
    },
    handler::{instructions::InstructionProvider, EthFrame, EvmTr},
    inspector::{InspectorHandler, NoOpInspector},
    interpreter::{Interpreter, InterpreterTypes},
    primitives::TxKind,
    DatabaseCommit, ExecuteEvm, InspectEvm, Inspector, Journal,
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
    inner: revm::context::Evm<Context<DB>, INSP, Instructions<DB>, Precompiles>,
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
    type Target = revm::context::Evm<Context<DB>, INSP, Instructions<DB>, Precompiles>;

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
        Self {
            inner: revm::context::Evm::new_with_inspector(
                context,
                inspect,
                Instructions::new(spec),
                Precompiles::default(),
            ),
            inspect: false,
            disable_beneficiary: false,
        }
    }

    /// Creates a new [`MegaethEvm`] instance with the given inspector enabled at runtime.
    pub fn with_inspector<I>(self, inspector: I) -> Evm<DB, I> {
        let disable_beneficiary = self.disable_beneficiary;
        let inner = revm::context::Evm::new_with_inspector(
            self.inner.data.ctx,
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
        self.inner.data.ctx.inner.journaled_state
    }
}

impl<DB, INSP> revm::handler::EvmTr for Evm<DB, INSP>
where
    DB: Database,
{
    type Context = Context<DB>;

    type Instructions = Instructions<DB>;

    type Precompiles = Precompiles;

    fn run_interpreter(
        &mut self,
        interpreter: &mut Interpreter<
            <Self::Instructions as InstructionProvider>::InterpreterTypes,
        >,
    ) -> <<Self::Instructions as InstructionProvider>::InterpreterTypes as InterpreterTypes>::Output
    {
        let result = interpreter
            .run_plain(self.inner.instruction.instruction_table(), &mut self.inner.data.ctx);
        result
    }

    #[inline]
    fn ctx(&mut self) -> &mut Self::Context {
        &mut self.inner.data.ctx
    }

    #[inline]
    fn ctx_ref(&self) -> &Self::Context {
        &self.inner.data.ctx
    }

    #[inline]
    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        (&mut self.inner.data.ctx, &mut self.inner.instruction)
    }

    #[inline]
    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        (&mut self.inner.data.ctx, &mut self.inner.precompiles)
    }
}

impl<DB, INSP> revm::inspector::InspectorEvmTr for Evm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<Context<DB>>,
{
    type Inspector = INSP;

    fn inspector(&mut self) -> &mut Self::Inspector {
        &mut self.inner.data.inspector
    }

    fn ctx_inspector(&mut self) -> (&mut Self::Context, &mut Self::Inspector) {
        (&mut self.inner.data.ctx, &mut self.inner.data.inspector)
    }

    fn run_inspect_interpreter(
        &mut self,
        interpreter: &mut Interpreter<
            <Self::Instructions as InstructionProvider>::InterpreterTypes,
        >,
    ) -> <<Self::Instructions as InstructionProvider>::InterpreterTypes as InterpreterTypes>::Output
    {
        self.inner.run_inspect_interpreter(interpreter)
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

    fn block(&self) -> &BlockEnv {
        self.block_env_ref()
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<revm::context::result::ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            self.set_tx(tx);
            self.inspect_replay()
        } else {
            revm::ExecuteEvm::transact(self, tx)
        }
    }

    /// Transact a system call.
    ///
    /// Note: this funtion copies the logic in `alloy_op_evm::OpEvm::transact_system_call`.
    fn transact_system_call(
        &mut self,
        caller: revm::primitives::Address,
        contract: revm::primitives::Address,
        data: revm::primitives::Bytes,
    ) -> Result<revm::context::result::ResultAndState<Self::HaltReason>, Self::Error> {
        let tx = Transaction {
            base: TxEnv {
                caller,
                kind: TxKind::Call(contract),
                // Explicitly set nonce to 0 so revm does not do any nonce checks
                nonce: 0,
                gas_limit: 30_000_000,
                value: U256::ZERO,
                data,
                // Setting the gas price to zero enforces that no value is transferred as part of
                // the call, and that the call will not count against the block's
                // gas limit
                gas_price: 0,
                // The chain ID check is not relevant here and is disabled if set to None
                chain_id: None,
                // Setting the gas priority fee to None ensures the effective gas price is derived
                // from the `gas_price` field, which we need to be zero
                gas_priority_fee: None,
                access_list: Default::default(),
                // blob fields can be None for this tx
                blob_hashes: Vec::new(),
                max_fee_per_blob_gas: 0,
                tx_type: TxType::Deposit as u8,
                authorization_list: Default::default(),
            },
            // The L1 fee is not charged for the EIP-4788 transaction, submit zero bytes for the
            // enveloped tx size.
            enveloped_tx: Some(Bytes::default()),
            deposit: Default::default(),
        };

        let mut gas_limit = tx.base.gas_limit;
        let mut basefee = 0;
        let mut disable_nonce_check = true;

        // ensure the block gas limit is >= the tx
        core::mem::swap(&mut self.block_env_mut().gas_limit, &mut gas_limit);
        // disable the base fee check for this call by setting the base fee to zero
        core::mem::swap(&mut self.block_env_mut().basefee, &mut basefee);
        // disable the nonce check
        core::mem::swap(&mut self.ctx().cfg.disable_nonce_check, &mut disable_nonce_check);

        let mut res = alloy_evm::Evm::transact(self, tx);

        // swap back to the previous gas limit
        core::mem::swap(&mut self.block_env_mut().gas_limit, &mut gas_limit);
        // swap back to the previous base fee
        core::mem::swap(&mut self.block_env_mut().basefee, &mut basefee);
        // swap back to the previous nonce check flag
        core::mem::swap(&mut self.ctx().cfg.disable_nonce_check, &mut disable_nonce_check);

        // NOTE: We assume that only the contract storage is modified. Revm currently marks the
        // caller and block beneficiary accounts as "touched" when we do the above transact calls,
        // and includes them in the result.
        //
        // We're doing this state cleanup to make sure that changeset only includes the changed
        // contract storage.
        if let Ok(res) = &mut res {
            res.state.retain(|addr, _| *addr == contract);
        }

        res
    }

    fn db_mut(&mut self) -> &mut Self::DB {
        &mut self.journaled_state_mut().database
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec>)
    where
        Self: Sized,
    {
        let spec = self.inner.data.ctx.megaeth_spec();
        let revm::Context { block: block_env, cfg: cfg_env, journaled_state, .. } =
            self.inner.data.ctx.into_inner();
        let cfg_env = cfg_env.into_megaeth_cfg(spec);
        (journaled_state.database, EvmEnv { block_env, cfg_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }
}

impl<DB, INSP> revm::ExecuteEvm for Evm<DB, INSP>
where
    DB: Database,
{
    type Output = Result<
        ResultAndState<HaltReason>,
        EVMError<<DB as revm::Database>::Error, TransactionError>,
    >;
    type Tx = Transaction;
    type Block = BlockEnv;

    fn set_tx(&mut self, tx: Self::Tx) {
        self.inner.data.ctx.set_tx(tx);
    }

    fn set_block(&mut self, block: Self::Block) {
        self.inner.data.ctx.set_block(block);
    }

    fn replay(&mut self) -> Self::Output {
        let spec = self.ctx().megaeth_spec();
        let mut h = Handler::<_, _, EthFrame<_, _, _>>::new(spec, self.disable_beneficiary);
        revm::handler::Handler::run(&mut h, self)
    }
}

impl<DB, INSP> revm::ExecuteCommitEvm for Evm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    type CommitOutput = Result<
        ExecutionResult<HaltReason>,
        EVMError<<DB as revm::Database>::Error, TransactionError>,
    >;

    fn replay_commit(&mut self) -> Self::CommitOutput {
        self.replay().map(|r| {
            self.ctx().db().commit(r.state);
            r.result
        })
    }
}

impl<DB, INSP> revm::InspectEvm for Evm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<Context<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.data.inspector = inspector;
    }

    fn inspect_replay(&mut self) -> Self::Output {
        let spec = self.ctx().megaeth_spec();
        let mut h = Handler::<_, _, EthFrame<_, _, _>>::new(spec, self.disable_beneficiary);
        h.inspect_run(self)
    }
}

impl<DB, INSP> revm::InspectCommitEvm for Evm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<Context<DB>>,
{
    fn inspect_replay_commit(&mut self) -> Self::CommitOutput {
        self.inspect_replay().map(|r| {
            self.ctx().db().commit(r.state);
            r.result
        })
    }
}
