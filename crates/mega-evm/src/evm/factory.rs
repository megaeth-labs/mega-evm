use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use alloy_primitives::U256;
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
        // The handler reloads the L1 block info from state only when `l2_block` differs from the
        // executing block number. `L1BlockInfo::default()` has `l2_block == 0`, which would pass as
        // already loaded for block 0 and leave the Isthmus operator-fee fields unset, so
        // `U256::MAX` marks the info as not yet loaded for every block number.
        let mut l1_block_info = L1BlockInfo::default();
        l1_block_info.l2_block = U256::MAX;
        let ctx = MegaContext::new(db, spec_id)
            .with_external_envs(self.external_env_factory.external_envs(block_number))
            .with_tx(MegaTransaction::default())
            .with_block(evm_env.block_env)
            .with_cfg(evm_env.cfg_env)
            .with_chain(l1_block_info)
            .with_tx_runtime_limits(runtime_limits);
        MegaEvm::new(ctx).with_dyn_precompiles(
            self.dyn_precompiles_builder
                .as_ref()
                .map_or_else(Default::default, |builder| builder(spec_id)),
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
    fn test_non_deposit_tx_at_block_zero_loads_l1_block_info_from_state() {
        use alloy_evm::{Evm as _, EvmFactory as _};
        use alloy_primitives::{address, Bytes, TxKind};
        use op_revm::constants::{
            L1_BLOCK_CONTRACT, OPERATOR_FEE_CONSTANT_OFFSET, OPERATOR_FEE_SCALARS_SLOT,
        };
        use revm::context::TxEnv;

        const CALLER: alloy_primitives::Address =
            address!("0x1000000000000000000000000000000000000001");
        const CALLEE: alloy_primitives::Address =
            address!("0x2000000000000000000000000000000000000002");
        const OPERATOR_FEE_CONSTANT: u64 = 1_234;
        let initial_balance = U256::from(1_000_000_000u64);

        // Only the operator fee constant is set, so the tx's whole cost is that constant.
        let mut operator_fee_scalars = [0u8; 32];
        operator_fee_scalars[OPERATOR_FEE_CONSTANT_OFFSET..OPERATOR_FEE_CONSTANT_OFFSET + 8]
            .copy_from_slice(&OPERATOR_FEE_CONSTANT.to_be_bytes());

        for spec in [MegaSpecId::EQUIVALENCE, MegaSpecId::MINI_REX, MegaSpecId::REX6] {
            let db = crate::test_utils::MemoryDatabase::default()
                .account_balance(CALLER, initial_balance)
                .account_storage(
                    L1_BLOCK_CONTRACT,
                    OPERATOR_FEE_SCALARS_SLOT,
                    U256::from_be_bytes(operator_fee_scalars),
                );
            let mut evm_env = EvmEnv::<MegaSpecId>::default();
            evm_env.cfg_env.spec = spec;
            assert_eq!(evm_env.block_env.number, U256::ZERO);

            let mut evm = MegaEvmFactory::new().create_evm(db, evm_env);
            let mut tx = MegaTransaction::new(TxEnv {
                caller: CALLER,
                kind: TxKind::Call(CALLEE),
                gas_limit: 100_000,
                ..Default::default()
            });
            tx.enveloped_tx = Some(Bytes::new());

            let result = evm.transact_raw(tx).expect("tx at block 0 executes");
            assert!(result.result.is_success(), "{spec:?}: {:?}", result.result);
            assert_eq!(
                result.state[&CALLER].info.balance,
                initial_balance - U256::from(OPERATOR_FEE_CONSTANT),
                "{spec:?}: caller is charged the operator fee from the L1Block state"
            );
        }
    }
}
