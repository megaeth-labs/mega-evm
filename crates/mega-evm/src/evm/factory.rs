use alloy_evm::EvmEnv;
use op_revm::L1BlockInfo;
use revm::Inspector;

use crate::{
    DynPrecompilesBuilder, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvFactory, MegaContext,
    MegaDatabase, MegaEvm, MegaSpecId, MegaTransaction,
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
/// use alloy_evm::EvmEnv;
/// use mega_evm::{MegaEvmFactory, MegaSpecId};
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
#[derive(derive_more::Debug, Clone)]
#[non_exhaustive]
pub struct MegaEvmFactory<ExtEnvFactory> {
    /// The `external_envs` service to provide deterministic external information during EVM
    /// execution.
    external_env_factory: ExtEnvFactory,

    /// A builder function to build dynamic precompiles for the EVM.
    #[debug(ignore)]
    dyn_precompiles_builder: Option<DynPrecompilesBuilder>,
}

impl Default for MegaEvmFactory<EmptyExternalEnv> {
    /// Creates a new [`EvmFactory`] instance with the default [`DefaultExternalEnvs`].
    ///
    /// This is the recommended way to create a factory when no custom `external_envs` is needed.
    /// The `DefaultExternalEnvs` provides a no-operation implementation that doesn't perform
    /// any external environment queries.
    fn default() -> Self {
        Self::new()
    }
}

impl MegaEvmFactory<EmptyExternalEnv> {
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
    pub fn new() -> Self {
        Self { external_env_factory: EmptyExternalEnv, dyn_precompiles_builder: None }
    }
}

impl<ExtEnvFactory> MegaEvmFactory<ExtEnvFactory> {
    /// Sets the builder function to build dynamic precompiles for the EVM.
    pub fn with_dyn_precompiles_builder(
        mut self,
        dyn_precompiles_builder: DynPrecompilesBuilder,
    ) -> Self {
        self.dyn_precompiles_builder = Some(dyn_precompiles_builder);
        self
    }

    /// Sets the external environment factory for the EVM.
    ///
    /// # Parameters
    ///
    /// - `external_env_factory`: The external environment factory to use for the EVM.
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_external_env_factory<NewExtEnvFactory: ExternalEnvFactory>(
        self,
        external_env_factory: NewExtEnvFactory,
    ) -> MegaEvmFactory<NewExtEnvFactory> {
        MegaEvmFactory {
            external_env_factory,
            dyn_precompiles_builder: self.dyn_precompiles_builder,
        }
    }
}

// Note: We do NOT implement alloy_evm::EvmFactory because it requires DB: Database
// in associated types, but our MegaEvm requires DB: MegaDatabase. Instead, we provide
// our own factory methods that work with MegaDatabase.
impl<ExtEnvFactory: ExternalEnvFactory + Clone> MegaEvmFactory<ExtEnvFactory> {
    /// Creates a new `Evm` instance with the provided database and EVM environment.
    ///
    /// This method constructs a new `Context` using the given database, the specification from the
    /// EVM environment, and the factory's external environments. It then sets up the transaction,
    /// block, config, and chain environment for the context, and finally returns a new `Evm`
    /// instance using the [`NoOpInspector`] as the default inspector.
    ///
    /// # Parameters
    ///
    /// - `db`: The database to use for EVM state.
    /// - `evm_env`: The EVM environment, including block and config environments.
    ///
    /// # Returns
    ///
    /// A new [`Evm`] instance configured with the provided database and environment.
    pub fn create_evm<DB: MegaDatabase>(
        &self,
        db: DB,
        evm_env: EvmEnv<MegaSpecId>,
    ) -> MegaEvm<DB, revm::inspector::NoOpInspector, ExtEnvFactory::EnvTypes> {
        let spec_id = *evm_env.spec_id();
        let runtime_limits = EvmTxRuntimeLimits::from_spec(spec_id);
        let ctx = MegaContext::new(db, spec_id)
            .with_external_envs(self.external_env_factory.external_envs())
            .with_tx(MegaTransaction::default())
            .with_block(evm_env.block_env)
            .with_cfg(evm_env.cfg_env)
            .with_chain(L1BlockInfo::default())
            .with_tx_runtime_limits(runtime_limits);
        MegaEvm::new(ctx).with_dyn_precompiles(
            self.dyn_precompiles_builder
                .as_ref()
                .map_or_else(Default::default, |builder| builder(spec_id)),
        )
    }

    /// Creates a new `Evm` instance with the provided database, EVM environment, and inspector.
    ///
    /// # Parameters
    ///
    /// - `db`: The database to use for EVM state.
    /// - `evm_env`: The EVM environment, including block and config environments.
    /// - `inspector`: The inspector to use for debugging and monitoring.
    ///
    /// # Returns
    ///
    /// A new [`Evm`] instance configured with the provided database, environment, and inspector.
    pub fn create_evm_with_inspector<
        DB: MegaDatabase,
        I: Inspector<MegaContext<DB, ExtEnvFactory::EnvTypes>>,
    >(
        &self,
        db: DB,
        evm_env: EvmEnv<MegaSpecId>,
        inspector: I,
    ) -> MegaEvm<DB, I, ExtEnvFactory::EnvTypes> {
        self.create_evm(db, evm_env).with_inspector(inspector)
    }
}
