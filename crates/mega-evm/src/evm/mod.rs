//! The Satin EVM.
//!
//! [`MegaEvm`] runs transactions on a [`MegaContext`] configured for the Satin spec through
//! [`MegaHandler`], which wraps op-revm's handler. The EVM implements revm's frame start and
//! return itself (see [`execution`](self::execution)), which is where `MegaETH`'s frame-level
//! mechanisms plug in.
//!
//! # Inspectors
//!
//! An inspector runs as revm runs it: through revm's inspected handler and frame lifecycle,
//! unchanged, which start a frame, hand over the result of its run and return it through the same
//! three methods the plain path does. The engine does nothing for an inspector, and an inspector
//! keeps its own consistency.
//!
//! - **A rewrite is taken as given.** revm commits or reverts a frame's journal checkpoint before
//!   the inspector sees its result, and the caller settles the result the inspector hands back —
//!   its classification, output and gas, reservoir included, the start ledger settling the start by
//!   the status the inspector left and revm's flags — and an answer the inspector gives in place of
//!   a frame never reaches the engine's frame start: the depth guard, the interceptors and gas
//!   detention see none of it, and it settles on the gas it is built on, at unlimited budgets and
//!   with no start charge back.
//! - **A callback's writes to gas are the inspector's.** A charge a frame cannot pay is refused,
//!   and leaves its record on the frame's gas as an opcode's does: a crossing of the withheld part
//!   stops nothing in a frame that ends without an out-of-gas, and a later halt that records
//!   nothing of its own is read off the record. An inspector that overwrites the interpreter's gas
//!   overwrites gas detention's split with it, until detention next holds the frame; the frame's
//!   budgets and its history allowance are on its start on the start ledger, which a gas it
//!   restores, or an interpreter it replaces, does not reach.
//! - **What an inspector sees.** A frame's end as the engine settled it: a frame that crossed gas
//!   detention's compute limit ends with the stop, which the engine makes of its out-of-gas once
//!   its run returns and before the inspector's `call_end`, `create_end` and `frame_end` see it, as
//!   the caller and the receipt do. A frame whose start crossed a limit ends with the stop before
//!   it runs, and is seen to start and end as an answer is. A `keylessDeploy` call's frame runs no
//!   instruction: the action made for it before each of its runs is set on its interpreter, and
//!   revm's inspected run returns an action set before it runs without a `step`, and hands a frame
//!   built with one to no `initialize_interp`, so the inspector sees the call start, its creation,
//!   and the call end. A frame a child returned into over a limit returns the stop the same way,
//!   with no `step` after the child's end.
//! - **Where an inspector runs.** Every transaction, the system-address transaction included. A
//!   pre-block call runs on the handler's plain system-call path, with no callback; a tool runs a
//!   system call through revm's `InspectSystemCallEvm` to watch it. The end of a block runs no
//!   code.

mod context;
mod execution;
mod factory;
mod frame;
mod history;
mod host;
mod instructions;
mod precompiles;
mod prices;
mod result;
mod schedule;
mod spec;
mod state;
mod validation;

pub use context::*;
pub use execution::*;
pub use factory::*;
pub use frame::*;
pub use history::*;
pub use precompiles::*;
pub use prices::*;
pub use result::*;
pub use schedule::*;
pub use spec::*;
pub use state::*;
pub use validation::*;

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{collections::BTreeMap, vec::Vec};

use alloy_evm::{
    precompiles::{DynPrecompile, PrecompilesMap},
    Database, EvmEnv,
};
use alloy_op_evm::map_op_err;
use op_revm::{OpHaltReason, OpTransactionError};
use revm::{
    context::{
        result::{EVMError, ExecResultAndState, ExecutionResult, ResultAndState},
        BlockEnv, CfgEnv, ContextSetters, ContextTr, FrameStack, JournalTr,
    },
    handler::{
        instructions::EthInstructions, system_call::SystemCallEvm, EthFrame, Handler, SystemCallTx,
    },
    inspector::{
        InspectCommitEvm, InspectEvm, InspectSystemCallEvm, Inspector, InspectorHandler,
        NoOpInspector,
    },
    interpreter::interpreter::EthInterpreter,
    primitives::{Address, Bytes, HashMap, B256},
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

use crate::{BucketId, EmptyExternalEnv, ExternalEnvTypes, MegaTransaction, MegaTransactionError};

/// The instruction table of the Satin engine.
pub(crate) type MegaInstructions<DB, ExtEnvs> =
    EthInstructions<EthInterpreter, MegaContext<DB, ExtEnvs>>;

/// The revm EVM a [`MegaEvm`] wraps.
///
/// It runs the Satin precompile set, carried as an alloy-evm map so a node can add its own
/// entries (see the `precompiles` module).
pub(crate) type MegaInnerEvm<DB, INSP, ExtEnvs> = revm::context::Evm<
    MegaContext<DB, ExtEnvs>,
    INSP,
    MegaInstructions<DB, ExtEnvs>,
    PrecompilesMap,
    EthFrame<EthInterpreter>,
>;

/// The Satin EVM.
///
/// Executes transactions through [`MegaHandler`] on a [`MegaContext`]. It implements revm's
/// execution traits ([`ExecuteEvm`], [`InspectEvm`], [`SystemCallEvm`] and their commit
/// variants) and alloy-evm's [`Evm`](alloy_evm::Evm), which is what a node's block executor
/// drives.
///
/// # Reuse after an error
///
/// A call that fails with an error — a read the database could not serve, a transaction
/// validation refuses — leaves nothing of itself in the EVM, whichever entry point ran it: the
/// handler's fatal cleanup ([`MegaHandler`]'s `catch_error`) drops the error a load recorded and
/// nothing took, takes back the call's journal transaction — its nonce bump and fee, and the writes
/// of its frames, those that completed before the error included — and clears the per-transaction
/// caches and the frame stack. What the next call reads of the context is prepared afresh when it
/// starts. So an EVM is reused after an error as after a success, and the next call runs as on a
/// fresh EVM over the same database.
///
/// Every entry point that takes the state off the journal on a success does so on an error too:
/// [`execute_transaction`](Self::execute_transaction), `transact`, `transact_many`, `replay`, the
/// `inspect` methods, the `_commit` variants, the system calls that return state, alloy-evm's
/// methods and the pre-block calls. The `_one` entry points leave the journal to the caller on both
/// outcomes, so the calls before a failed one stay in it for the caller to finalize, and none of
/// the failed call's writes do. A failed deposit is a result, not an error: its nonce bump and mint
/// stay.
///
/// A caller of the handler's `run_without_catch_error` that drops an error makes the same cleanup
/// first, through `catch_error`. A panic unwinds past the cleanup: an EVM a panic unwound out of is
/// not reused.
#[allow(missing_debug_implementations)]
pub struct MegaEvm<DB: Database, INSP, ExtEnvs: ExternalEnvTypes = EmptyExternalEnv> {
    inner: MegaInnerEvm<DB, INSP, ExtEnvs>,
    /// Whether [`alloy_evm::Evm::transact_raw`] runs the inspector.
    inspect: bool,
    /// Which precompile calls gas detention may price from the Satin table.
    priced_precompiles: PricedPrecompiles,
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, NoOpInspector, ExtEnvs> {
    /// Creates an EVM over `ctx`, without an inspector.
    pub fn new(ctx: MegaContext<DB, ExtEnvs>) -> Self {
        let spec = ctx.cfg().spec;
        let inner = revm::context::Evm {
            ctx,
            inspector: NoOpInspector,
            instruction: instructions::mega_instructions(spec.into()),
            precompiles: satin_precompiles_map(),
            frame_stack: FrameStack::new_prealloc(8),
        };
        Self { inner, inspect: false, priced_precompiles: PricedPrecompiles::default() }
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs> {
    /// Replaces the inspector with a tool's. The new inspector runs on every alloy-evm
    /// [`transact`](alloy_evm::Evm::transact) until it is disabled again.
    ///
    /// The inspector may rewrite what execution produces (see the module documentation). The engine
    /// does not refuse one in block execution: a node decides which inspectors run on block
    /// production.
    pub fn with_inspector<I>(self, inspector: I) -> MegaEvm<DB, I, ExtEnvs> {
        MegaEvm {
            inner: self.inner.with_inspector(inspector),
            inspect: true,
            priced_precompiles: self.priced_precompiles,
        }
    }

    /// The execution context.
    pub const fn ctx(&self) -> &MegaContext<DB, ExtEnvs> {
        &self.inner.ctx
    }

    /// The execution context, mutably.
    pub const fn ctx_mut(&mut self) -> &mut MegaContext<DB, ExtEnvs> {
        &mut self.inner.ctx
    }

    /// The inspector.
    pub const fn inspector(&self) -> &INSP {
        &self.inner.inspector
    }

    /// Whether alloy-evm's [`transact`](alloy_evm::Evm::transact) runs the inspector.
    pub const fn is_inspecting(&self) -> bool {
        self.inspect
    }

    /// Adds `dyn_precompiles` on top of the Satin set, replacing an entry whose address is
    /// already taken.
    ///
    /// A node's RPC builds these; the chain's own set is the one [`MegaEvm::new`] installs. A
    /// Satin address replaced here is no longer priced from the Satin table: gas detention runs a
    /// call to it on the allowance, as it does every precompile it cannot price (see the
    /// `precompiles` module).
    pub fn with_dyn_precompiles(
        mut self,
        dyn_precompiles: HashMap<Address, DynPrecompile>,
    ) -> Self {
        for (address, dyn_precompile) in dyn_precompiles {
            self.priced_precompiles.record_replaced(address);
            self.inner.precompiles.apply_precompile(&address, move |_| Some(dyn_precompile));
        }
        self
    }

    /// Replaces the whole precompile set with `precompiles`, a set that is not the Satin one: the
    /// neutral configuration's, the fixture fork's own. Gas detention prices nothing from the
    /// Satin table from then on.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn replace_precompile_set(&mut self, precompiles: PrecompilesMap) {
        self.inner.precompiles = precompiles;
        self.priced_precompiles.record_foreign();
    }

    /// Enforces `limits` on every transaction this EVM runs from now on.
    ///
    /// Every EVM already runs under its spec's limits
    /// ([`ProtocolLimits::for_spec`](crate::ProtocolLimits::for_spec)), and block execution
    /// installs them again this way before every transaction, so a block's transaction runs under
    /// them whatever the caller configured on the EVM. Outside block execution — a tool, a test —
    /// this is how a caller runs a counterfactual under others.
    #[must_use]
    pub fn with_tx_runtime_limits(mut self, limits: crate::EvmTxRuntimeLimits) -> Self {
        self.set_tx_runtime_limits(limits);
        self
    }

    /// Enforces `limits` on every transaction this EVM runs from now on.
    pub const fn set_tx_runtime_limits(&mut self, limits: crate::EvmTxRuntimeLimits) {
        self.inner.ctx.set_tx_runtime_limits(limits);
    }

    /// The limits every transaction this EVM runs is held to.
    pub const fn tx_runtime_limits(&self) -> &crate::EvmTxRuntimeLimits {
        self.inner.ctx.additional_limit().limits()
    }

    /// Consumes the EVM and returns the revm EVM it wraps.
    pub(crate) fn into_inner(self) -> MegaInnerEvm<DB, INSP, ExtEnvs> {
        self.inner
    }
}

impl<DB, INSP, ExtEnvs> MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    ExtEnvs: ExternalEnvTypes,
{
    /// The block hashes execution has read on this EVM so far.
    ///
    /// `BLOCKHASH` reads bypass the journal, so this is where a stateless witness learns of them.
    /// The record starts empty and is emptied again when block execution starts a block, so it
    /// holds what this EVM read and not what its database cached earlier.
    pub fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.ctx().block_hash_record().hashes().clone()
    }

    /// Forgets the block hashes read so far, so the next reads are attributable to one
    /// transaction. The record decides nothing, so clearing it changes no execution result.
    pub fn clear_accessed_block_hashes(&mut self) {
        self.ctx_mut().clear_block_hash_record();
    }

    /// The SALT buckets the SALT environment answered with a valid capacity on this EVM so far,
    /// in ascending order.
    ///
    /// A bucket's capacity is read through a side channel no database sees, so this is where a
    /// stateless witness learns which buckets it must prove. The record holds what this EVM
    /// executed since it was last cleared, dropped candidates included and lookups that failed
    /// left out: it starts empty, block execution empties it when a block starts, and nothing
    /// empties it between the transactions of a block, as the per-transaction multiplier cache
    /// is. An EVM reused across blocks outside a block executor reports every block it ran since
    /// it was last cleared; a node that executes on several EVMs takes the union of their records.
    pub fn get_accessed_bucket_ids(&self) -> Vec<BucketId> {
        self.ctx().bucket_record().to_vec()
    }

    /// Forgets the SALT buckets asked about so far, so the next asks are attributable to one
    /// transaction. The record decides nothing, so clearing it changes no execution result.
    pub fn clear_accessed_bucket_ids(&mut self) {
        self.ctx_mut().clear_bucket_record();
    }

    /// The reads of the Oracle's storage the running (or last) transaction or system call made
    /// through the oracle service, in order, each with the service's answer. It is emptied before
    /// every transaction and system call; a transaction's outcome carries a copy.
    pub fn oracle_reads(&self) -> &[OracleRead] {
        self.ctx().oracle_reads().reads()
    }
}

impl<DB, INSP, ExtEnvs> MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
    ExtEnvs: ExternalEnvTypes,
{
    /// Executes `tx`, through the inspector when one is enabled, and returns its outcome: the
    /// result and state, the gas by ledger, the usage the common execution layer counted, the limit
    /// that stopped the transaction, if any, and the reads of the Oracle's storage it made
    /// through the oracle service. Nothing is committed.
    pub fn execute_transaction(
        &mut self,
        tx: MegaTransaction,
    ) -> Result<MegaTransactionOutcome, EVMError<DB::Error, MegaTransactionError>> {
        let result_and_state = self.run_transaction(tx)?;
        let layer = &self.inner.ctx.additional_limit;
        let gas = MegaGasUsage::new(
            result_and_state.result.gas(),
            layer.history_gas_spent(),
            layer.history_bytes(),
            layer.reservoir_remaining(),
        );
        Ok(MegaTransactionOutcome {
            result_and_state,
            gas,
            usage: self.inner.ctx.limit_usage(),
            limit_exceeded: layer.limit_exceeded().copied(),
            oracle_reads: self.inner.ctx.oracle_reads().reads().to_vec(),
        })
    }

    /// Runs `tx` through the inspector when one is enabled, without committing.
    fn run_transaction(
        &mut self,
        tx: MegaTransaction,
    ) -> Result<ResultAndState<OpHaltReason>, EVMError<DB::Error, MegaTransactionError>> {
        let result = if self.inspect {
            InspectEvm::inspect_tx(self, tx)
        } else {
            ExecuteEvm::transact(self, tx)
        };
        result.map_err(map_op_err)
    }
}

/// The error a [`MegaEvm`] reports through revm's execution traits.
pub type MegaEvmError<DBError> = EVMError<DBError, OpTransactionError>;

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> ExecuteEvm for MegaEvm<DB, INSP, ExtEnvs> {
    type ExecutionResult = ExecutionResult<OpHaltReason>;
    type State = EvmState;
    type Error = MegaEvmError<DB::Error>;
    type Tx = MegaTransaction;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        MegaHandler::<_, Self::Error, _>::new().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    /// Runs the transaction set on the context again, and finalizes the journal whether or not
    /// it succeeded, as [`transact`](ExecuteEvm::transact) does (see [`MegaEvm`] on the entry
    /// points and the journal).
    fn replay(
        &mut self,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let output_or_error = MegaHandler::<_, Self::Error, _>::new().run(self);
        let state = self.finalize();
        Ok(ExecResultAndState::new(output_or_error?, state))
    }
}

impl<DB: Database + DatabaseCommit, INSP, ExtEnvs: ExternalEnvTypes> ExecuteCommitEvm
    for MegaEvm<DB, INSP, ExtEnvs>
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
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
        self.inner.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        MegaHandler::<_, Self::Error, _>::new().inspect_run(self)
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
        self.run_system_call(MegaTransaction::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ))
    }

    /// Runs a system call and finalizes the journal whether or not it succeeded (see
    /// [`MegaEvm`] on the entry points and the journal). The trait's own returns before it
    /// finalizes on an error.
    fn system_call_with_caller(
        &mut self,
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Result<ExecResultAndState<Self::ExecutionResult, Self::State>, Self::Error> {
        let output_or_error =
            self.system_call_one_with_caller(caller, system_contract_address, data);
        let state = self.finalize();
        Ok(ExecResultAndState::new(output_or_error?, state))
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs> {
    /// Runs a pre-block system call from `caller` to `contract` with `data`, and returns its result
    /// and state. Nothing is committed.
    ///
    /// It is [`transact_system_call`](alloy_evm::Evm::transact_system_call) on the pre-block
    /// budget, [`PRE_BLOCK_CALL_GAS_LIMIT`], instead of revm's [`SYSTEM_CALL_GAS_LIMIT`]: at most
    /// [`SYSTEM_CALL_REGULAR_GAS_LIMIT`] of it is regular gas, as of every system call, and the
    /// rest is the call's state-gas reservoir. What the call left of its reservoir is
    /// [`AdditionalLimit::reservoir_remaining`](crate::AdditionalLimit::reservoir_remaining). The
    /// journal is finalized whether or not the call succeeded, as `transact_system_call`'s.
    ///
    /// [`PRE_BLOCK_CALL_GAS_LIMIT`]: crate::constants::PRE_BLOCK_CALL_GAS_LIMIT
    /// [`SYSTEM_CALL_GAS_LIMIT`]: revm::handler::SYSTEM_CALL_GAS_LIMIT
    /// [`SYSTEM_CALL_REGULAR_GAS_LIMIT`]: crate::SYSTEM_CALL_REGULAR_GAS_LIMIT
    pub(crate) fn transact_pre_block_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<OpHaltReason>, EVMError<DB::Error, MegaTransactionError>> {
        let mut tx = MegaTransaction::new_system_tx_with_caller(caller, contract, data);
        tx.0.base.gas_limit = crate::constants::PRE_BLOCK_CALL_GAS_LIMIT;
        let output_or_error = self.run_system_call(tx);
        let state = ExecuteEvm::finalize(self);
        Ok(ResultAndState::new(output_or_error.map_err(map_op_err)?, state))
    }

    /// Runs `tx` as a system call: the protocol's own work, prepared as such (see
    /// [`MegaContext`]'s system-call preparation) and run through the handler's system-call path.
    fn run_system_call(
        &mut self,
        tx: MegaTransaction,
    ) -> Result<ExecutionResult<OpHaltReason>, MegaEvmError<DB::Error>> {
        self.inner.ctx.set_tx(tx);
        self.inner.ctx.on_new_system_call();
        MegaHandler::<_, MegaEvmError<DB::Error>, _>::new().run_system_call(self)
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
        self.inner.ctx.set_tx(MegaTransaction::new_system_tx_with_caller(
            caller,
            system_contract_address,
            data,
        ));
        self.inner.ctx.on_new_system_call();
        MegaHandler::<_, Self::Error, _>::new().inspect_run_system_call(self)
    }
}

impl<DB, INSP, ExtEnvs> alloy_evm::Evm for MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>, EthInterpreter>,
    ExtEnvs: ExternalEnvTypes,
{
    type DB = DB;
    type Tx = MegaTransaction;
    type Error = EVMError<DB::Error, MegaTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = MegaSpecId;
    type BlockEnv = BlockEnv;
    /// The Satin precompile set, with whatever a node added to it.
    type Precompiles = PrecompilesMap;
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
        self.run_transaction(tx)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        SystemCallEvm::system_call_with_caller(self, caller, contract, data).map_err(map_op_err)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        let (db, cfg_env, block_env) = self.into_inner().ctx.into_parts();
        (db, EvmEnv { cfg_env, block_env })
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspect = enabled;
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        let evm = &self.inner;
        (evm.ctx.db(), &evm.inspector, &evm.precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        let evm = &mut self.inner;
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
        context::{result::InvalidTransaction, ContextSetters, Transaction, TxEnv},
        database::State,
        handler::system_call::SYSTEM_CALL_GAS_LIMIT,
        DatabaseRef,
    };
    use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

    const CALLER: Address = address!("0x4000000000000000000000000000000000000001");
    const CALLEE: Address = address!("0x5000000000000000000000000000000000000001");

    fn context<DB: Database>(db: DB) -> MegaContext<DB> {
        MegaContext::new(db, MegaSpecId::SATIN).with_chain(zero_fee_l1_block_info())
    }

    /// Replacing the whole precompile set installs the set it is given, and gas detention prices
    /// nothing from the Satin table after it: the fixture fork's set is not the Satin one.
    #[test]
    fn test_a_replaced_precompile_set_is_dispatched_and_not_priced() {
        use revm::{handler::PrecompileProvider, precompile::Precompiles};
        type Ctx = MegaContext<MemoryDatabase>;
        let mut evm = MegaEvm::new(context(MemoryDatabase::default()));
        let kzg = crate::kzg_point_evaluation::ADDRESS;
        let price = |evm: &MegaEvm<MemoryDatabase, NoOpInspector>| {
            evm.priced_precompiles.price(&evm.inner.precompiles, &kzg, &[])
        };
        assert_eq!(price(&evm), Some(crate::kzg_point_evaluation::GAS_COST));

        let osaka = crate::test_utils::neutral_precompiles(crate::EthSpecId::OSAKA).unwrap();
        evm.replace_precompile_set(osaka);
        let dispatched = PrecompileProvider::<Ctx>::warm_addresses(&evm.inner.precompiles);
        assert!(core::ptr::eq(dispatched, Precompiles::osaka().addresses_set()), "Osaka's set");
        assert_eq!(price(&evm), None);
    }

    /// A whole set put in the map's place through the mutable reference `EvmTr::all_mut` hands
    /// out is not seen: an address whose dispatched entry carries the id of the Satin table's
    /// entry there is still priced from the Satin table, whatever the new set charges for it.
    #[test]
    fn test_a_set_replaced_through_a_mutable_reference_is_priced_where_its_ids_match() {
        use revm::{
            handler::{EvmTr, PrecompileProvider},
            precompile::Precompiles,
        };
        type Ctx = MegaContext<MemoryDatabase>;
        let mut evm = MegaEvm::new(context(MemoryDatabase::default()));
        let kzg = crate::kzg_point_evaluation::ADDRESS;

        let osaka = crate::test_utils::neutral_precompiles(crate::EthSpecId::OSAKA).unwrap();
        *EvmTr::all_mut(&mut evm).2 = osaka;
        let dispatched = PrecompileProvider::<Ctx>::warm_addresses(&evm.inner.precompiles);
        assert!(core::ptr::eq(dispatched, Precompiles::osaka().addresses_set()), "Osaka's set");
        let osaka_kzg = Precompiles::osaka().get(&kzg).unwrap();
        assert_ne!(osaka_kzg.required_gas(&[]), Some(crate::kzg_point_evaluation::GAS_COST));

        let price = evm.priced_precompiles.price(&evm.inner.precompiles, &kzg, &[]);
        assert_eq!(price, Some(crate::kzg_point_evaluation::GAS_COST), "the Satin table's price");
    }

    /// A call to `CALLEE`, with room for the state gas a value transfer to it draws: `CALLEE`
    /// holds nothing, so a transfer creates it and pays the new account's state gas. The room is
    /// counted at the byte prices in effect, with the history of the body and of the recipient's
    /// write record on top of 100,000 of regular gas.
    fn tx(value: U256) -> MegaTransaction {
        use revm::context_interface::cfg::GasId;
        let gas_limit = 100_000 +
            crate::satin_gas_params().get(GasId::new_account_state_gas()) +
            crate::history_gas(crate::TX_BODY_SIZE + crate::WRITE_RECORD_SIZE).unwrap();
        OpTx(op_transaction(TxEnv {
            caller: CALLER,
            gas_limit,
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
        assert_eq!(inner.ctx.spec(), MegaSpecId::SATIN);
    }

    #[test]
    fn test_alloy_evm_interface_methods_execute_transactions() {
        let mut db = funded_db();
        let mut evm = MegaEvm::new(
            context(&mut db).with_block(BlockEnv { gas_limit: 2_222_222, ..Default::default() }),
        );

        assert_eq!(evm.chain_id(), evm.ctx().cfg().chain_id);
        assert_eq!(evm.cfg_env().spec, MegaSpecId::SATIN);
        assert_eq!(evm.block().gas_limit, 2_222_222);

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
        assert_eq!(evm_env.block_env.gas_limit, 2_222_222);
    }

    #[test]
    fn test_revm_execute_one_finalize_commit_works() {
        let mut db = funded_db();
        let mut evm = MegaEvm::new(context(&mut db));
        ExecuteEvm::set_block(&mut evm, BlockEnv { gas_limit: 2_222_222, ..Default::default() });
        assert_eq!(evm.block().gas_limit, 2_222_222);

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

    /// A system call from any caller runs through revm's system-call entry point.
    #[test]
    fn test_revm_system_call_with_caller_works() {
        let mut db = funded_db();
        let mut evm = MegaEvm::new(context(&mut db));
        let result =
            SystemCallEvm::system_call_one_with_caller(&mut evm, CALLER, CALLEE, Bytes::new())
                .unwrap();
        assert!(result.is_success());
        assert_eq!(evm.ctx().tx().caller(), CALLER);
        assert_eq!(evm.ctx().tx().kind(), TxKind::Call(CALLEE));
    }

    /// A pre-block call runs on the pre-block budget whatever the block's gas limit: 30M of
    /// regular gas and the rest of the budget as its reservoir.
    #[test]
    fn test_a_pre_block_call_runs_on_the_pre_block_budget() {
        use crate::constants::PRE_BLOCK_CALL_GAS_LIMIT;
        for block_gas_limit in [1_000_000, 10_000_000_000] {
            let mut db = funded_db();
            let block = BlockEnv { gas_limit: block_gas_limit, ..Default::default() };
            let mut evm = MegaEvm::new(context(&mut db).with_block(block));
            let result = evm.transact_pre_block_system_call(CALLER, CALLEE, Bytes::new()).unwrap();
            assert!(result.result.is_success());
            assert_eq!(evm.ctx().tx().gas_limit(), PRE_BLOCK_CALL_GAS_LIMIT, "{block_gas_limit}");
            assert_eq!(
                evm.ctx().additional_limit().reservoir_remaining(),
                PRE_BLOCK_CALL_GAS_LIMIT - SYSTEM_CALL_REGULAR_GAS_LIMIT,
                "an empty callee draws nothing from the reservoir",
            );
        }
    }

    /// The default system-call entry point runs on revm's system-call gas limit whatever the
    /// block's gas limit is: its regular budget is the base 30M, and the margin above it is the
    /// reservoir.
    #[test]
    fn test_default_system_call_keeps_the_30m_regular_budget() {
        let mut db = funded_db();
        let mut evm = MegaEvm::new(
            context(&mut db).with_block(BlockEnv { gas_limit: 100_000_000, ..Default::default() }),
        );

        let result =
            SystemCallEvm::system_call_with_caller(&mut evm, CALLER, CALLEE, Bytes::new()).unwrap();
        assert!(result.result.is_success());
        // The literals, not revm's constants: this pins what revm's default is.
        assert_eq!(evm.ctx().tx().gas_limit(), 31_566_720);
        assert_eq!(SYSTEM_CALL_GAS_LIMIT, 31_566_720);
        assert_eq!(
            evm.ctx().additional_limit().reservoir_remaining(),
            31_566_720 - 30_000_000,
            "30M of it is regular gas",
        );
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

    /// The EVM reports the block hashes its Host served, which is where a stateless witness
    /// learns of a `BLOCKHASH` read. What the database cached before is not a read of this EVM.
    #[test]
    fn test_mega_evm_exposes_the_block_hashes_it_read() {
        let mut db = MemoryDatabase::default();
        let mut state = State::builder().with_database(&mut db).build();
        state.block_hashes.insert(1, B256::from([1_u8; 32]));

        let mut evm = MegaEvm::new(context(&mut state));
        assert!(evm.get_accessed_block_hashes().is_empty(), "the cache is not a read");

        let served = revm::context_interface::Host::block_hash(evm.ctx_mut(), 7)
            .expect("the database serves the hash");
        assert_eq!(evm.get_accessed_block_hashes().get(&7), Some(&served));
        assert_eq!(evm.get_accessed_block_hashes().len(), 1, "and only the read");

        evm.clear_accessed_block_hashes();
        assert!(evm.get_accessed_block_hashes().is_empty());
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
