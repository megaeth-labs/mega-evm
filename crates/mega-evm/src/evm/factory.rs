use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use core::convert::Infallible;
use op_revm::L1BlockInfo;
use revm::{context::result::EVMError, inspector::NoOpInspector, Inspector};

use crate::{
    DefaultExternalEnvs, ExternalEnvs, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, MegaTransactionError,
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
#[derive(Debug, Clone)]
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

impl<ExtEnvs> MegaEvmFactory<ExtEnvs>
where
    ExtEnvs: ExternalEnvs + Clone,
{
    /// Creates a new `MegaEvm` instance with the given configuration.
    pub fn create_evm_with_config<DB: Database>(
        &self,
        db: DB,
        config: MegaEvmConfig,
    ) -> MegaEvm<DB, NoOpInspector, ExtEnvs> {
        let ctx = MegaContext::new(db, config.evm_env.cfg_env().spec, self.external_envs.clone())
            .with_data_limit(config.data_limit)
            .with_kv_update_limit(config.kv_update_limit)
            .with_compute_gas_limit(config.compute_gas_limit);
        MegaEvm::new(ctx)
    }
}

/// Configuration for the `MegaEvm`. This struct provides a collective settings to configure the
/// `MegaContext`.
#[derive(Debug, Clone)]
pub struct MegaEvmConfig {
    /// The EVM environment.
    pub evm_env: EvmEnv<MegaSpecId>,
    /// The data limit for one transaction.
    pub data_limit: u64,
    /// The KV update limit for one transaction.
    pub kv_update_limit: u64,
    /// The compute gas limit for one transaction.
    pub compute_gas_limit: u64,
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
