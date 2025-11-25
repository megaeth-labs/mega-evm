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
use salt::BucketId;
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

use crate::{ExternalEnvs, LimitUsage, MegaTransaction};

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
    #[deprecated(note = "Use `into_parts` instead")]
    pub fn into_journaled_state(self) -> Journal<DB> {
        self.inner.ctx.inner.journaled_state
    }

    /// Consumes the `MegaEvm` instance and returns the inner EVM.
    ///
    /// This method is typically used after EVM execution when you need to access
    /// the underlying EVM components and no longer require the `MegaEvm` wrapper.
    #[inline]
    pub fn into_parts(
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
    ExtEnvs: ExternalEnvs,
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

impl<DB: Database + BlockHashes, INSP, ExtEnvs: ExternalEnvs> MegaEvm<DB, INSP, ExtEnvs> {
    /// Get the block hashes used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the block hashes used during transaction execution.
    pub fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.db_ref().get_accessed_block_hashes()
    }
}
