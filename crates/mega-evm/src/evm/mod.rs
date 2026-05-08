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
//! `MegaETH` supports multiple EVM specifications:
//!
//! - **`EQUIVALENCE`**: Maintains equivalence with Optimism Isthmus EVM
//! - **`MINI_REX`**: Enhanced version with quadratic LOG costs and disabled SELFDESTRUCT
//! - **`REX`**: Fixes `MiniRex` call opcode inconsistencies and refines storage gas
//! - **`REX1`**: Resets compute gas limits between transactions
//! - **`REX2`**: Re-enables SELFDESTRUCT with EIP-6780 semantics
//! - **`REX3`**: Increases oracle gas limit to 20M, moves oracle detention to SLOAD-based, tracks
//!   keyless deploy compute gas
//! - **`REX4`**: Per-call-frame resource budgets, relative gas detention, storage gas stipend,
//!   `MegaAccessControl` and `MegaLimitControl` system contracts

mod context;
mod execution;
mod factory;
mod host;
mod instructions;
mod interfaces;
mod limit;
mod precompiles;
mod result;
mod spec;
mod state;

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{collections::BTreeMap, vec::Vec};

use alloy_primitives::{Address, B256};
pub use context::*;
pub use execution::*;
pub use factory::*;
pub use host::*;
pub use instructions::*;
#[allow(unused_imports, unreachable_pub)]
pub use interfaces::*;
pub use limit::*;
pub use precompiles::*;
pub use result::*;
pub use spec::*;
pub use state::*;

use alloy_evm::{
    precompiles::{DynPrecompile, PrecompilesMap},
    Database,
};
use revm::{
    context::{result::ResultAndState, BlockEnv, ContextTr},
    handler::{EthFrame, EvmTr},
    inspector::NoOpInspector,
    interpreter::interpreter::EthInterpreter,
    primitives::HashMap,
    ExecuteEvm, InspectEvm, Inspector, Journal,
};

use crate::{BucketId, ExternalEnvTypes, LimitUsage, MegaTransaction};

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
pub struct MegaEvm<DB: Database, INSP, ExtEnvTypes: ExternalEnvTypes> {
    inner: revm::context::Evm<
        MegaContext<DB, ExtEnvTypes>,
        INSP,
        MegaInstructions<DB, ExtEnvTypes>,
        PrecompilesMap,
        EthFrame<EthInterpreter>,
    >,
    /// Whether to enable the inspector at runtime.
    inspect: bool,
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> core::fmt::Debug
    for MegaEvm<DB, INSP, ExtEnvs>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MegaethEvm").field("inspect", &self.inspect).finish_non_exhaustive()
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> core::ops::Deref
    for MegaEvm<DB, INSP, ExtEnvs>
{
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

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> core::ops::DerefMut
    for MegaEvm<DB, INSP, ExtEnvs>
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, NoOpInspector, ExtEnvs> {
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

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs> {
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

    /// Creates a new `MegaETH` EVM instance with the inspector disabled at runtime.
    ///
    /// # Returns
    ///
    /// A new `Evm` instance with the inspector disabled.
    pub fn without_inspector(self) -> MegaEvm<DB, NoOpInspector, ExtEnvs> {
        let inner = revm::context::Evm::new_with_inspector(
            self.inner.ctx,
            NoOpInspector,
            self.inner.instruction,
            self.inner.precompiles,
        );
        MegaEvm { inner, inspect: false }
    }

    /// Sets the transaction runtime limits for the EVM.
    pub fn with_tx_runtime_limits(self, tx_limits: EvmTxRuntimeLimits) -> Self {
        let inner = revm::context::Evm {
            ctx: self.inner.ctx.with_tx_runtime_limits(tx_limits),
            inspector: self.inner.inspector,
            instruction: self.inner.instruction,
            precompiles: self.inner.precompiles,
            frame_stack: self.inner.frame_stack,
        };
        Self { inner, inspect: self.inspect }
    }

    /// Adds or overrides dynamic precompiles in the EVM.
    ///
    /// # Parameters
    ///
    /// - `dyn_precompiles`: The dynamic precompiles to add to the EVM, overriding the existing
    ///   precompiles if they already exist.
    ///
    /// # Returns
    ///
    /// A new `Evm` instance with the dynamic precompiles added.
    fn with_dyn_precompiles(self, dyn_precompiles: HashMap<Address, DynPrecompile>) -> Self {
        let mut precompiles = self.inner.precompiles;
        // Apply the dynamic precompiles to the precompiles map. If the precompile already exists,
        // it will be overridden with the dynamic precompile.
        for (address, dyn_precompile) in dyn_precompiles {
            precompiles.apply_precompile(&address, move |_| Some(dyn_precompile));
        }
        let inner = revm::context::Evm {
            ctx: self.inner.ctx,
            inspector: self.inner.inspector,
            instruction: self.inner.instruction,
            precompiles,
            frame_stack: self.inner.frame_stack,
        };
        Self { inner, inspect: self.inspect }
    }
}

impl<DB: Database, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs> {
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
    #[deprecated(note = "Use `into_inner` instead")]
    pub fn into_journaled_state(self) -> Journal<DB> {
        self.inner.ctx.inner.journaled_state
    }

    /// Consumes the `MegaEvm` instance and returns the inner EVM.
    ///
    /// This method is typically used after EVM execution when you need to access
    /// the underlying EVM components and no longer require the `MegaEvm` wrapper.
    #[inline]
    #[allow(clippy::type_complexity)]
    pub fn into_inner(
        self,
    ) -> revm::context::Evm<
        MegaContext<DB, ExtEnvs>,
        INSP,
        MegaInstructions<DB, ExtEnvs>,
        PrecompilesMap,
        EthFrame<EthInterpreter>,
    > {
        self.inner
    }
}

impl<DB, INSP, ExtEnvs> MegaEvm<DB, INSP, ExtEnvs>
where
    DB: Database,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
    ExtEnvs: ExternalEnvTypes,
{
    /// Execute a transaction and return the outcome. If the inspector is set, it will be used to
    /// inspect the transaction.
    /// Users can use [`MegaEvm::with_inspector`] to set up a custom inspector.
    /// Users can use [`MegaEvm::without_inspector`] to disable the inspector.
    ///
    /// # Parameters
    ///
    /// - `tx`: The transaction to execute
    ///
    /// # Returns
    ///
    /// The outcome of the transaction.
    pub fn execute_transaction(
        &mut self,
        tx: MegaTransaction,
    ) -> Result<MegaTransactionOutcome, EVMError<DB::Error, MegaTransactionError>> {
        let ResultAndState { result, state } = if self.inspect {
            InspectEvm::inspect_tx(self, tx)?
        } else {
            ExecuteEvm::transact(self, tx)?
        };
        let additional_limit = self.ctx().additional_limit.borrow();
        let LimitUsage { data_size, kv_updates, compute_gas, state_growth } =
            additional_limit.get_usage();
        Ok(MegaTransactionOutcome {
            result,
            state,
            data_size,
            kv_updates,
            compute_gas_used: compute_gas,
            state_growth_used: state_growth,
        })
    }

    /// Inspect a transaction and return the outcome. The inspector used is the one set up already
    /// in the EVM. Use [`MegaEvm::with_inspector`] to set up a custom inspector.
    ///
    /// # Parameters
    ///
    /// - `tx`: The transaction to inspect
    ///
    /// # Returns
    ///
    /// The outcome of the transaction.
    #[deprecated(
        since = "1.0.2",
        note = "Use `MegaEvm::execute_transaction` instead, which will automatically use the inspector if it is set up"
    )]
    pub fn inspect_transaction(
        &mut self,
        tx: MegaTransaction,
    ) -> Result<MegaTransactionOutcome, EVMError<DB::Error, MegaTransactionError>> {
        let ResultAndState { result, state } = InspectEvm::inspect_tx(self, tx)?;
        let additional_limit = self.ctx().additional_limit.borrow();
        let LimitUsage { data_size, kv_updates, compute_gas, state_growth } =
            additional_limit.get_usage();
        Ok(MegaTransactionOutcome {
            result,
            state,
            data_size,
            kv_updates,
            compute_gas_used: compute_gas,
            state_growth_used: state_growth,
        })
    }

    /// Get the bucket IDs used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the bucket IDs used during transaction execution.
    pub fn get_accessed_bucket_ids(&self) -> Vec<BucketId> {
        self.ctx_ref().dynamic_storage_gas_cost.borrow().get_bucket_ids()
    }
}

impl<DB: Database + BlockHashes, INSP, ExtEnvs: ExternalEnvTypes> MegaEvm<DB, INSP, ExtEnvs> {
    /// Get the block hashes used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the block hashes used during transaction execution.
    pub fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.db_ref().get_accessed_block_hashes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};
    use alloy_primitives::{address, Bytes, U256};
    use revm::{
        context::{
            result::{ExecResultAndState, ExecutionResult},
            ContextSetters, TxEnv,
        },
        database::State,
        inspector::NoOpInspector,
        state::EvmState,
        ExecuteCommitEvm, ExecuteEvm, InspectEvm, SystemCallEvm,
    };

    const CALLER: Address = address!("4000000000000000000000000000000000000001");
    const CALLEE: Address = address!("5000000000000000000000000000000000000001");

    fn configure_context<DB: Database>(db: DB) -> MegaContext<DB, EmptyExternalEnv> {
        let mut context = MegaContext::new(db, MegaSpecId::REX4);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        context
    }

    fn tx_env() -> TxEnv {
        TxEnv {
            caller: CALLER,
            gas_limit: 100_000,
            kind: alloy_primitives::TxKind::Call(CALLEE),
            value: U256::ZERO,
            data: Bytes::new(),
            ..Default::default()
        }
    }

    fn mega_tx() -> MegaTransaction {
        let mut tx = MegaTransaction::new(tx_env());
        tx.enveloped_tx = Some(Bytes::new());
        tx
    }

    #[test]
    fn test_mega_evm_builder_chain_produces_working_evm() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let context = configure_context(&mut db);
        let evm = MegaEvm::new(context);

        let limits = EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(777);
        let evm = evm.with_tx_runtime_limits(limits);
        assert_eq!(evm.ctx_ref().additional_limit.borrow().limits.tx_compute_gas_limit, 777);

        let evm = evm.with_dyn_precompiles(HashMap::default());
        let evm = evm.with_inspector(NoOpInspector);
        let evm = evm.without_inspector();

        let inner = evm.into_inner();
        assert_eq!(inner.ctx.block.gas_limit, u64::MAX);
    }

    #[test]
    fn test_alloy_evm_interface_methods_execute_transactions() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));

        assert_eq!(alloy_evm::Evm::chain_id(&evm), evm.ctx_ref().cfg.chain_id);
        assert_eq!(alloy_evm::Evm::block(&evm).gas_limit, evm.ctx_ref().block.gas_limit);

        alloy_evm::Evm::set_inspector_enabled(&mut evm, true);
        assert!(evm.inspect);
        let (_db_ref, _inspector, _precompiles) = alloy_evm::Evm::components(&evm);
        let (_db_ref_mut, _inspector_mut, _precompiles_mut) =
            alloy_evm::Evm::components_mut(&mut evm);

        let result = alloy_evm::Evm::transact_raw(&mut evm, mega_tx()).unwrap();
        assert!(result.result.is_success());

        let system_call =
            alloy_evm::Evm::transact_system_call(&mut evm, CALLER, CALLEE, Bytes::new()).unwrap();
        assert!(system_call.result.is_success());

        let (_db_back, evm_env) = alloy_evm::Evm::finish(evm);
        assert_eq!(evm_env.cfg_env.spec, MegaSpecId::REX4);
    }

    #[test]
    fn test_revm_execute_one_finalize_commit_works() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));
        let block = revm::context::BlockEnv { gas_limit: 222_222, ..Default::default() };
        ExecuteEvm::set_block(&mut evm, block);
        assert_eq!(evm.block_env_ref().gas_limit, 222_222);

        let one: ExecutionResult<MegaHaltReason> =
            ExecuteEvm::transact_one(&mut evm, mega_tx()).unwrap();
        assert!(one.is_success());
        let state = ExecuteEvm::finalize(&mut evm);
        ExecuteCommitEvm::commit(&mut evm, state);
    }

    #[test]
    fn test_revm_replay_works() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));
        evm.ctx().set_tx(mega_tx());
        let replay: ExecResultAndState<ExecutionResult<MegaHaltReason>, EvmState> =
            ExecuteEvm::replay(&mut evm).unwrap();
        assert!(replay.result.is_success());
    }

    #[test]
    fn test_revm_inspect_one_tx_works() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));
        InspectEvm::set_inspector(&mut evm, NoOpInspector);
        let inspected: ExecutionResult<MegaHaltReason> =
            InspectEvm::inspect_one_tx(&mut evm, mega_tx()).unwrap();
        assert!(inspected.is_success());
    }

    #[test]
    fn test_revm_system_call_with_caller_works() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));
        let system_call: ExecutionResult<MegaHaltReason> =
            SystemCallEvm::transact_system_call_with_caller(&mut evm, CALLER, CALLEE, Bytes::new())
                .unwrap();
        assert!(system_call.is_success());
    }

    #[test]
    fn test_transact_system_call_with_gas_limit_uses_passed_value() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));

        let result = evm
            .transact_system_call_with_gas_limit(CALLER, CALLEE, Bytes::new(), 123_456_789)
            .unwrap();
        assert!(result.result.is_success());
        // The custom gas limit must be applied to the underlying tx.
        assert_eq!(evm.inner.ctx.tx.base.gas_limit, 123_456_789);
    }

    #[test]
    fn test_default_system_call_keeps_upstream_30m_gas_limit() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        context.block.gas_limit = 100_000_000;
        let mut evm = MegaEvm::new(context);

        // The default system-call entry point must NOT be widened by REX5 — only the
        // explicit `transact_system_call_with_gas_limit` path should pick up the live
        // block budget. This preserves byte-level behavior of EIP-2935 / EIP-4788
        // pre-block calls across all specs.
        SystemCallEvm::transact_system_call_with_caller(&mut evm, CALLER, CALLEE, Bytes::new())
            .unwrap();
        // Literal, not `SYSTEM_CALL_GAS_LIMIT_FLOOR`: this assertion verifies revm's
        // upstream hardcoded default. If upstream ever drifts from our floor, this
        // test should fail loudly rather than be auto-aligned by our constant.
        assert_eq!(evm.inner.ctx.tx.base.gas_limit, 30_000_000);
    }

    #[test]
    fn test_mega_evm_exposes_state_wrapper_block_hashes() {
        let mut db = MemoryDatabase::default();
        let mut state = State::builder().with_database(&mut db).build();
        state.block_hashes.insert(7, B256::from([7_u8; 32]));

        let evm = MegaEvm::new(configure_context(&mut state));
        assert_eq!(evm.get_accessed_block_hashes().get(&7), Some(&B256::from([7_u8; 32])));
    }

    #[test]
    fn test_convenience_execution_methods_work() {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db)).with_inspector(NoOpInspector);

        let executed = evm.execute_transaction(mega_tx()).unwrap();
        assert!(executed.result.is_success());

        #[allow(deprecated)]
        let inspected = evm.inspect_transaction(mega_tx()).unwrap();
        assert!(inspected.result.is_success());
    }

    #[test]
    fn test_execute_transaction_fails_with_insufficient_balance() {
        let mut db = MemoryDatabase::default().account_code(CALLEE, Bytes::new());
        let mut evm = MegaEvm::new(configure_context(&mut db));

        let mut tx = MegaTransaction::new(TxEnv {
            caller: CALLER,
            gas_limit: 100_000,
            kind: alloy_primitives::TxKind::Call(CALLEE),
            value: U256::from(1_000_000),
            data: Bytes::new(),
            ..Default::default()
        });
        tx.enveloped_tx = Some(Bytes::new());

        let result = evm.execute_transaction(tx);
        assert!(result.is_err());
    }
}
