use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use alloy_op_evm::{map_op_err, OpTxError};
use alloy_primitives::{Address, Bytes};
use revm::{
    context::{
        result::{EVMError, ExecResultAndState, ExecutionResult, ResultAndState},
        BlockEnv, ContextSetters, ContextTr,
    },
    handler::{EthFrame, EvmTr, SystemCallTx},
    interpreter::interpreter::EthInterpreter,
    state::EvmState,
    DatabaseCommit, InspectEvm, Inspector, SystemCallEvm,
};

use crate::{
    constants, ExternalEnvTypes, IntoMegaethCfgEnv, MegaContext, MegaEvm, MegaHaltReason,
    MegaHandler, MegaSpecId, MegaTransaction, MegaTransactionError,
};

/// Implementation of [`alloy_evm::Evm`] for `MegaETH` EVM.
///
/// This implementation provides the core EVM interface required by the Alloy EVM framework,
/// enabling seamless integration with Alloy-based applications while providing `MegaETH`-specific
/// customizations and optimizations.
impl<DB, INSP, ExtEnvs: ExternalEnvTypes> alloy_evm::Evm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
{
    type DB = DB;
    type Tx = MegaTransaction;
    // `alloy_evm::Evm::Error` requires the transaction error to implement
    // `alloy_evm::InvalidTxError`, which cannot be implemented for the foreign
    // `OpTransactionError`. `alloy-op-evm` exposes the `OpTxError` newtype for exactly this, so
    // the internal handler error is mapped at this boundary.
    type Error = EVMError<DB::Error, OpTxError>;
    type HaltReason = MegaHaltReason;
    type Spec = MegaSpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = INSP;

    fn block(&self) -> &BlockEnv {
        self.block_env_ref()
    }

    fn cfg_env(&self) -> &revm::context::CfgEnv<Self::Spec> {
        self.cfg_env_ref()
    }

    fn chain_id(&self) -> u64 {
        self.ctx_ref().cfg.chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        if self.inspect {
            InspectEvm::inspect_tx(self, tx).map_err(map_op_err)
        } else {
            revm::ExecuteEvm::transact(self, tx).map_err(map_op_err)
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
    /// to maintain compatibility with the Optimism EVM system call interface. The
    /// transaction's gas limit is `MegaETH`'s frozen pre-REX5 30M
    /// ([`constants::PRE_REX5_SYSTEM_CALL_GAS_LIMIT`]), not upstream revm's
    /// `SYSTEM_CALL_GAS_LIMIT` (which drifted to `31_566_720`). Callers that need the live
    /// block gas budget — e.g. REX5 pre-block helpers whose cost is sensitive to dynamic
    /// storage gas — should use [`MegaEvm::transact_system_call_with_gas_limit`] instead.
    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.system_call_with_caller(caller, contract, data).map_err(map_op_err)
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

impl<DB, INSP, ExtEnvs: ExternalEnvTypes> revm::ExecuteEvm for MegaEvm<DB, INSP, ExtEnvs>
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

impl<DB, INSP, ExtEnvs: ExternalEnvTypes> revm::ExecuteCommitEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.ctx().db_mut().commit(state);
    }
}

impl<DB, INSP, ExtEnvs: ExternalEnvTypes> revm::InspectEvm for MegaEvm<DB, INSP, ExtEnvs>
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

impl<DB, INSP, ExtEnvs: ExternalEnvTypes> revm::InspectCommitEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
{
}

impl<DB, INSP, ExtEnvs: ExternalEnvTypes> revm::SystemCallEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
{
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        // Override upstream `SystemCallTx` gas limit: revm 40's
        // `SYSTEM_CALL_GAS_LIMIT` is 31_566_720 (30M + bal-devnet-7 state-gas
        // reservoir). Frozen MegaETH keeps the historical 30M.
        let mut tx =
            <MegaTransaction as SystemCallTx>::new_system_tx_with_caller(caller, contract, data);
        tx.base.gas_limit = constants::PRE_REX5_SYSTEM_CALL_GAS_LIMIT;
        self.ctx().set_tx(tx);
        let mut h = MegaHandler::<_, _, EthFrame<EthInterpreter>>::new();
        revm::handler::Handler::run_system_call(&mut h, self)
    }
}

impl<DB, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
{
    /// Transact a system call with an explicit gas limit and finalize.
    ///
    /// Behaves like [`alloy_evm::Evm::transact_system_call`] but lets the caller specify
    /// the gas limit instead of `MegaETH`'s frozen pre-REX5 30M default. This is intended
    /// for REX5+ pre-block helpers (e.g. `transact_apply_pending_changes` in
    /// `system::sequencer_registry`) whose real cost is sensitive to dynamic storage gas
    /// and is no longer guaranteed to fit within 30M on activation blocks.
    ///
    /// The recommended argument is
    /// `block.gas_limit.max(crate::constants::rex5::SYSTEM_CALL_GAS_LIMIT_FLOOR)`.
    pub fn transact_system_call_with_gas_limit(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
        gas_limit: u64,
    ) -> Result<ResultAndState<MegaHaltReason>, EVMError<DB::Error, MegaTransactionError>> {
        let mut tx =
            <MegaTransaction as SystemCallTx>::new_system_tx_with_caller(caller, contract, data);
        tx.base.gas_limit = gas_limit;
        self.ctx().set_tx(tx);
        let mut h = MegaHandler::<_, _, EthFrame<EthInterpreter>>::new();
        revm::handler::Handler::run_system_call(&mut h, self).map(|result| {
            let state = self.inner.ctx.journal_mut().finalize();
            ResultAndState { result, state }
        })
    }
}
