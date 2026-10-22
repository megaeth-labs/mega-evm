//! Factory of Satin EVMs for alloy-evm consumers.

use alloy_evm::{Database, EvmEnv};
use op_revm::{precompiles::OpPrecompiles, OpHaltReason};
use revm::{
    context::{result::EVMError, BlockEnv, DBErrorMarker},
    inspector::NoOpInspector,
    Inspector,
};

use crate::{
    EmptyExternalEnv, ExternalEnvFactory, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};

/// Creates [`MegaEvm`]s for alloy-evm consumers such as a node's block executor.
///
/// The factory holds the [`ExternalEnvFactory`] that supplies each EVM with the SALT and oracle
/// environments of the block it executes.
#[derive(Clone, Debug, Default)]
pub struct MegaEvmFactory<ExtEnvFactory = EmptyExternalEnv> {
    external_env_factory: ExtEnvFactory,
}

impl MegaEvmFactory<EmptyExternalEnv> {
    /// Creates a factory whose EVMs have no external environments.
    pub const fn new() -> Self {
        Self { external_env_factory: EmptyExternalEnv }
    }
}

impl<ExtEnvFactory> MegaEvmFactory<ExtEnvFactory> {
    /// The external environment factory.
    pub const fn external_env_factory(&self) -> &ExtEnvFactory {
        &self.external_env_factory
    }

    /// Replaces the external environment factory.
    pub fn with_external_env_factory<F: ExternalEnvFactory>(
        self,
        external_env_factory: F,
    ) -> MegaEvmFactory<F> {
        MegaEvmFactory { external_env_factory }
    }
}

impl<ExtEnvFactory: ExternalEnvFactory> alloy_evm::EvmFactory for MegaEvmFactory<ExtEnvFactory> {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> =
        MegaEvm<DB, I, ExtEnvFactory::EnvTypes>;
    type Context<DB: Database> = MegaContext<DB, ExtEnvFactory::EnvTypes>;
    type Tx = MegaTransaction;
    type Error<DBError: DBErrorMarker> = EVMError<DBError, MegaTransactionError>;
    type HaltReason = OpHaltReason;
    type Spec = MegaSpecId;
    type BlockEnv = BlockEnv;
    /// op-revm's precompile set for the base spec.
    ///
    /// Provisional: the Satin precompile provider replaces this type when it lands, and code that
    /// names `OpPrecompiles` through this associated type has no source-compatibility promise
    /// across that change.
    type Precompiles = OpPrecompiles;

    /// Creates an EVM for the block in `evm_env`, with the external environments of that block.
    ///
    /// The configuration fields the spec fixes are set from the spec (see
    /// [`MegaContext::with_cfg`]).
    fn create_evm<DB: Database>(
        &self,
        db: DB,
        evm_env: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let EvmEnv { cfg_env, block_env } = evm_env;
        let external_envs =
            self.external_env_factory.external_envs(block_env.number.saturating_to());
        let ctx = MegaContext::new_with_external_envs(db, cfg_env.spec, external_envs)
            .with_cfg(cfg_env)
            .with_block(block_env);
        MegaEvm::new(ctx)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        self.create_evm(db, input).with_inspector(inspector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, ExternalEnvs, SaltEnv, TestExternalEnvs};
    use alloy_evm::{Evm, EvmFactory};
    use alloy_primitives::{BlockNumber, U256};
    use core::cell::Cell;
    use revm::context::CfgEnv;

    #[test]
    fn test_external_env_factory_getter() {
        let factory = MegaEvmFactory::new()
            .with_external_env_factory(TestExternalEnvs::new().with_bucket_capacity(7, 1_024));

        let got: &TestExternalEnvs = factory.external_env_factory();

        assert_eq!(got.get_bucket_capacity(7).unwrap(), 1_024);
    }

    /// Whatever configuration the caller passes, the EVM runs with the switches the spec fixes.
    #[test]
    fn test_create_evm_applies_the_spec_switches() {
        let mut cfg_env = CfgEnv::new_with_spec(MegaSpecId::SATIN);
        cfg_env.chain_id = 4326;
        cfg_env.tx_gas_limit_cap = Some(1 << 24);
        cfg_env.enable_amsterdam_eip8037 = false;
        let block_env = BlockEnv { number: U256::from(5), ..Default::default() };

        let evm = MegaEvmFactory::new()
            .create_evm(MemoryDatabase::default(), EvmEnv { cfg_env, block_env });

        assert_eq!(evm.chain_id(), 4326);
        assert_eq!(evm.cfg_env().tx_gas_limit_cap, Some(crate::constants::TX_GAS_LIMIT_CAP));
        assert!(evm.cfg_env().enable_amsterdam_eip8037);
        assert_eq!(evm.block().number, U256::from(5));
    }

    /// Records the block number each EVM's external environments are created for.
    #[derive(Debug, Default)]
    struct RecordingFactory(Cell<Option<BlockNumber>>);

    impl ExternalEnvFactory for RecordingFactory {
        type EnvTypes = EmptyExternalEnv;

        fn external_envs(&self, block: BlockNumber) -> ExternalEnvs<Self::EnvTypes> {
            self.0.set(Some(block));
            ExternalEnvs::default()
        }
    }

    #[test]
    fn test_create_evm_takes_the_external_envs_of_the_block() {
        let factory = MegaEvmFactory::new().with_external_env_factory(RecordingFactory::default());
        let block_env = BlockEnv { number: U256::from(1_234), ..Default::default() };

        let evm = factory.create_evm_with_inspector(
            MemoryDatabase::default(),
            EvmEnv { cfg_env: CfgEnv::new_with_spec(MegaSpecId::SATIN), block_env },
            NoOpInspector,
        );

        assert_eq!(factory.external_env_factory().0.get(), Some(1_234));
        assert!(evm.is_inspecting());
    }
}
