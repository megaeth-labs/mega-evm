//! The Satin EVM.
//!
//! [`MegaEvm`] wraps op-revm's [`OpEvm`] over a [`MegaContext`] configured for the Satin spec.
//! It adds no `MegaETH` behavior yet: SALT pricing, history gas, resource limits, detention and
//! the system contracts arrive with T3 through T8.

mod context;
mod factory;
mod spec;

pub use context::*;
pub use factory::*;
pub use spec::*;

use alloy_evm::EvmEnv;
use alloy_op_evm::map_op_err;
use op_revm::{precompiles::OpPrecompiles, OpEvm, OpHaltReason, OpTransactionError};
use revm::{
    context::{
        result::{EVMError, ExecResultAndState, ExecutionResult, ResultAndState},
        BlockEnv, CfgEnv, ContextTr,
    },
    handler::{instructions::EthInstructions, system_call::SystemCallEvm},
    inspector::{InspectCommitEvm, InspectEvm, InspectSystemCallEvm, Inspector, NoOpInspector},
    interpreter::interpreter::EthInterpreter,
    primitives::{Address, Bytes},
    state::EvmState,
    Database, DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

use crate::{EmptyExternalEnv, ExternalEnvTypes, MegaTransaction, MegaTransactionError};

/// The instruction table of the Satin engine.
pub(crate) type MegaInstructions<DB, ExtEnvs> = EthInstructions<EthInterpreter, MegaContext<DB, ExtEnvs>>;

/// The op-revm EVM a [`MegaEvm`] wraps.
///
/// It runs op-revm's precompile set for the base spec; T3.1 replaces it with the Satin set.
pub(crate) type MegaInnerEvm<DB, INSP, ExtEnvs> =
    OpEvm<MegaContext<DB, ExtEnvs>, INSP, MegaInstructions<DB, ExtEnvs>, OpPrecompiles>;

/// The Satin EVM.
///
/// Executes transactions through op-revm's handler on a [`MegaContext`]. It implements revm's
/// execution traits ([`ExecuteEvm`], [`InspectEvm`], [`SystemCallEvm`] and their commit
/// variants) and alloy-evm's [`Evm`](alloy_evm::Evm), which is what a node's block executor
/// drives.
#[allow(missing_debug_implementations)]
pub struct MegaEvm<DB: Database, INSP, ExtEnvs: ExternalEnvTypes = EmptyExternalEnv> {
    inner: MegaInnerEvm<DB, INSP, ExtEnvs>,
    /// Whether [`alloy_evm::Evm::transact_raw`] runs the inspector.
    inspect: bool,
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, NoOpInspector, ExtEnvs> {
    /// Creates an EVM over `ctx`, without an inspector.
    pub fn new(ctx: MegaContext<DB, ExtEnvs>) -> Self {
        Self { inner: OpEvm::new(ctx, NoOpInspector), inspect: false }
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs> {
    /// Replaces the inspector. The new inspector runs on every alloy-evm
    /// [`transact`](alloy_evm::Evm::transact) until it is disabled again.
    pub fn with_inspector<I>(self, inspector: I) -> MegaEvm<DB, I, ExtEnvs> {
        MegaEvm { inner: self.inner.with_inspector(inspector), inspect: true }
    }

    /// The execution context.
    pub const fn ctx(&self) -> &MegaContext<DB, ExtEnvs> {
        &self.inner.0.ctx
    }

    /// The execution context, mutably.
    pub const fn ctx_mut(&mut self) -> &mut MegaContext<DB, ExtEnvs> {
        &mut self.inner.0.ctx
    }

    /// Whether alloy-evm's [`transact`](alloy_evm::Evm::transact) runs the inspector.
    pub const fn is_inspecting(&self) -> bool {
        self.inspect
    }

    /// Consumes the EVM and returns the op-revm EVM it wraps.
    pub(crate) fn into_inner(self) -> MegaInnerEvm<DB, INSP, ExtEnvs> {
        self.inner
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> ExecuteEvm for MegaEvm<DB, INSP, ExtEnvs> {
    type ExecutionResult = ExecutionResult<OpHaltReason>;
    type State = EvmState;
    type Error = EVMError<DB::Error, OpTransactionError>;
    type Tx = MegaTransaction;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.transact_one(tx)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        self.inner.replay()
    }
}

impl<DB: Database + DatabaseCommit, INSP, ExtEnvs: ExternalEnvTypes> ExecuteCommitEvm
    for MegaEvm<DB, INSP, ExtEnvs>
{
    fn commit(&mut self, state: Self::State) {
        self.inner.commit(state);
    }
}

impl<DB, INSP, ExtEnvs> InspectEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
    ExtEnvs: ExternalEnvTypes,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.inspect_one_tx(tx)
    }
}

impl<DB, INSP, ExtEnvs> InspectCommitEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
    ExtEnvs: ExternalEnvTypes,
{
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> SystemCallEvm for MegaEvm<DB, INSP, ExtEnvs> {
    fn system_call_one_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.system_call_one_with_caller(caller, system_contract_address, data)
    }
}

impl<DB, INSP, ExtEnvs> InspectSystemCallEvm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
    ExtEnvs: ExternalEnvTypes,
{
    fn inspect_one_system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.inspect_one_system_call_with_caller(caller, system_contract_address, data)
    }
}

impl<DB, INSP, ExtEnvs> alloy_evm::Evm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: alloy_evm::Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
    ExtEnvs: ExternalEnvTypes,
{
    type DB = DB;
    type Tx = MegaTransaction;
    type Error = EVMError<DB::Error, MegaTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = MegaSpecId;
    type BlockEnv = BlockEnv;
    /// op-revm's precompile set for the base spec.
    ///
    /// Provisional: the Satin precompile provider replaces this type when it lands, and code that
    /// names `OpPrecompiles` through this associated type has no source-compatibility promise
    /// across that change.
    type Precompiles = OpPrecompiles;
    type Inspector = INSP;

    fn block(&self) -> &BlockEnv {
        self.ctx().block()
    }

    fn cfg_env(&self) -> &CfgEnv<MegaSpecId> {
        self.ctx().mega_cfg()
    }

    fn chain_id(&self) -> u64 {
        self.ctx().mega_cfg().chain_id
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        let result = if self.inspect {
            InspectEvm::inspect_tx(&mut self.inner, tx)
        } else {
            ExecuteEvm::transact(&mut self.inner, tx)
        };
        result.map_err(map_op_err)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.system_call_with_caller(caller, contract, data).map_err(map_op_err)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        let (db, cfg_env, block_env) = self.into_inner().0.ctx.into_parts();
        (db, EvmEnv { cfg_env, block_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        let evm = &self.inner.0;
        (evm.ctx.db(), &evm.inspector, &evm.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        let evm = &mut self.inner.0;
        (evm.ctx.db_mut(), &mut evm.inspector, &mut evm.precompiles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{op_transaction, zero_fee_l1_block_info, MemoryDatabase};
    use alloy_evm::{Evm, EvmError};
    use alloy_op_evm::OpTx;
    use alloy_primitives::{address, TxKind, U256};
    use revm::{
        context::{result::InvalidTransaction, ContextSetters, TxEnv},
        database::State,
        DatabaseRef,
    };
    use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

    const CALLER: Address = address!("0x4000000000000000000000000000000000000001");
    const CALLEE: Address = address!("0x5000000000000000000000000000000000000001");

    fn context<DB: Database>(db: DB) -> MegaContext<DB> {
        MegaContext::new(db, MegaSpecId::SATIN).with_chain(zero_fee_l1_block_info())
    }

    fn tx(value: U256) -> MegaTransaction {
        OpTx(op_transaction(TxEnv {
            caller: CALLER,
            gas_limit: 100_000,
            kind: TxKind::Call(CALLEE),
            value,
            ..Default::default()
        }))
    }

    fn funded_db() -> MemoryDatabase {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new())
    }

    #[test]
    fn test_mega_evm_builder_chain_produces_working_evm() {
        let mut db = funded_db();
        let evm = MegaEvm::new(context(&mut db));
        assert!(!evm.is_inspecting());

        let mut evm = evm.with_inspector(NoOpInspector);
        assert!(evm.is_inspecting());
        assert!(evm.transact_raw(tx(U256::ZERO)).unwrap().result.is_success());

        let inner = evm.into_inner();
        assert_eq!(inner.0.ctx.spec(), MegaSpecId::SATIN);
    }

    #[test]
    fn test_alloy_evm_interface_methods_execute_transactions() {
        let mut db = funded_db();
        let mut evm = MegaEvm::new(
            context(&mut db).with_block(BlockEnv { gas_limit: 222_222, ..Default::default() }),
        );

        assert_eq!(evm.chain_id(), evm.ctx().cfg().chain_id);
        assert_eq!(evm.cfg_env().spec, MegaSpecId::SATIN);
        assert_eq!(evm.block().gas_limit, 222_222);

        evm.set_inspector_enabled(true);
        assert!(evm.is_inspecting());
        evm.set_inspector_enabled(false);
        let (_db, _inspector, _precompiles) = evm.components();
        let (_db, _inspector, _precompiles) = evm.components_mut();

        assert!(evm.transact_raw(tx(U256::ZERO)).unwrap().result.is_success());
        let system_call = Evm::transact_system_call(&mut evm, CALLER, CALLEE, Bytes::new());
        assert!(system_call.unwrap().result.is_success());

        let (_db, evm_env) = evm.finish();
        assert_eq!(evm_env.cfg_env.spec, MegaSpecId::SATIN);
        assert_eq!(evm_env.cfg_env.tx_gas_limit_cap, Some(crate::constants::TX_GAS_LIMIT_CAP));
        assert_eq!(evm_env.block_env.gas_limit, 222_222);
    }

    #[test]
    fn test_revm_execute_one_finalize_commit_works() {
        let mut db = funded_db();
        let mut evm = MegaEvm::new(context(&mut db));
        ExecuteEvm::set_block(&mut evm, BlockEnv { gas_limit: 222_222, ..Default::default() });
        assert_eq!(evm.block().gas_limit, 222_222);

        let result = ExecuteEvm::transact_one(&mut evm, tx(U256::from(7))).unwrap();
        assert!(result.is_success());
        let state = ExecuteEvm::finalize(&mut evm);
        ExecuteCommitEvm::commit(&mut evm, state);
        drop(evm);

        assert_eq!(db.basic_ref(CALLEE).unwrap().unwrap().balance, U256::from(7));
    }

    #[test]
    fn test_revm_replay_works() {
        let mut db = funded_db();
        let mut evm = MegaEvm::new(context(&mut db));
        evm.ctx_mut().set_tx(tx(U256::ZERO));
        let replay = ExecuteEvm::replay(&mut evm).unwrap();
        assert!(replay.result.is_success());
        assert!(replay.state.contains_key(&CALLER));
    }

    #[test]
    fn test_revm_inspect_one_tx_works() {
        let mut db = funded_db();
        let mut evm = MegaEvm::new(context(&mut db))
            .with_inspector(TracingInspector::new(TracingInspectorConfig::default_parity()));
        let result = InspectEvm::inspect_one_tx(&mut evm, tx(U256::ZERO)).unwrap();
        assert!(result.is_success());

        // The arena starts with one placeholder root node; the call fills it in.
        let traces = evm.inspector().traces();
        assert_eq!(traces.nodes().len(), 1, "one call frame");
        assert_eq!(traces.nodes()[0].trace.address, CALLEE);

        // A fresh inspector replaces the one that recorded the call.
        InspectEvm::set_inspector(
            &mut evm,
            TracingInspector::new(TracingInspectorConfig::default_parity()),
        );
        assert_eq!(evm.inspector().traces().nodes()[0].trace.address, Address::ZERO);
    }

    #[test]
    fn test_execute_transaction_fails_with_insufficient_balance() {
        let mut db = MemoryDatabase::default().account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(context(&mut db));

        let err = evm.transact_raw(tx(U256::from(1_000_000))).unwrap_err();
        let invalid =
            err.as_invalid_tx_err().and_then(alloy_evm::InvalidTxError::as_invalid_tx_err);
        assert!(matches!(invalid, Some(InvalidTransaction::LackOfFundForMaxFee { .. })), "{err:?}");
    }

    #[test]
    fn test_finish_returns_the_database() {
        let mut state = State::builder().with_database(funded_db()).build();
        let mut evm = MegaEvm::new(context(&mut state));
        assert!(Evm::transact_commit(&mut evm, tx(U256::from(5))).unwrap().is_success());
        let (db, _) = evm.finish();
        assert_eq!(
            db.cache.accounts[&CALLEE].account.as_ref().unwrap().info.balance,
            U256::from(5)
        );
    }
}
