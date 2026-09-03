use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use op_revm::L1BlockInfo;
use revm::{context::result::EVMError, Inspector};

use crate::{
    DynPrecompilesBuilder, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvFactory, MegaContext,
    MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError,
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

    /// Returns a reference to the external environment factory.
    ///
    /// This is useful for inspecting or cloning the factory after construction,
    /// since the field is private and the struct is `#[non_exhaustive]`.
    pub fn external_env_factory(&self) -> &ExtEnvFactory {
        &self.external_env_factory
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

impl<ExtEnvFactory: ExternalEnvFactory + Clone> alloy_evm::EvmFactory
    for MegaEvmFactory<ExtEnvFactory>
{
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> =
        MegaEvm<DB, I, ExtEnvFactory::EnvTypes>;
    type Context<DB: Database> = MegaContext<DB, ExtEnvFactory::EnvTypes>;
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
        let spec_id = *evm_env.spec_id();
        let block_number = evm_env.block_env.number.to();
        let runtime_limits = EvmTxRuntimeLimits::from_spec(spec_id);
        let ctx = MegaContext::new(db, spec_id)
            .with_external_envs(self.external_env_factory.external_envs(block_number))
            .with_tx(MegaTransaction::default())
            .with_block(evm_env.block_env)
            .with_cfg(evm_env.cfg_env)
            .with_chain(L1BlockInfo::default())
            .with_tx_runtime_limits(runtime_limits);
        // The builder is an external closure with no exhaustive match over `MegaSpecId`, so it
        // receives the behavior projection: dynamic precompiles are execution semantics, and a
        // builder keyed on exact specs must not see an alias rung during a rollback window. The
        // context above keeps the raw rung.
        MegaEvm::new(ctx).with_dyn_precompiles(
            self.dyn_precompiles_builder
                .as_ref()
                .map_or_else(Default::default, |builder| builder(spec_id.behavior())),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_external_env_factory_getter() {
        let factory = MegaEvmFactory::new().with_external_env_factory(EmptyExternalEnv);

        let got: &EmptyExternalEnv = factory.external_env_factory();

        // Verify the getter returns a stable reference to the same field.
        assert!(core::ptr::eq(got, factory.external_env_factory()));
    }

    #[test]
    fn test_dyn_precompiles_builder_receives_the_behavior_spec() {
        use alloy_evm::EvmFactory as _;
        use core::sync::atomic::{AtomicU8, Ordering};

        // The builder must see the behavior projection, never an alias rung: an external
        // builder keyed on exact specs would otherwise install a different precompile set
        // during a rollback window.
        static SEEN_SPEC: AtomicU8 = AtomicU8::new(u8::MAX);

        let factory =
            MegaEvmFactory::new().with_dyn_precompiles_builder(std::sync::Arc::new(|spec| {
                SEEN_SPEC.store(spec as u8, Ordering::SeqCst);
                revm::primitives::HashMap::default()
            }));

        let mut evm_env = EvmEnv::<MegaSpecId>::default();
        evm_env.cfg_env.spec = MegaSpecId::MINI_REX_1;
        let _evm = factory.create_evm(crate::test_utils::MemoryDatabase::default(), evm_env);

        assert_eq!(SEEN_SPEC.load(Ordering::SeqCst), MegaSpecId::EQUIVALENCE as u8);
    }
}
