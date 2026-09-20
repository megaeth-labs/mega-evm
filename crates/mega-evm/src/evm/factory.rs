//! Factory of Satin EVMs for alloy-evm consumers.

use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use core::fmt;
use op_revm::OpHaltReason;
use revm::{
    context::{result::EVMError, BlockEnv, DBErrorMarker},
    inspector::NoOpInspector,
    Inspector,
};

use crate::{
    DynPrecompilesBuilder, EmptyExternalEnv, ExternalEnvFactory, MegaContext, MegaEvm, MegaSpecId,
    MegaTransaction, MegaTransactionError,
};

/// Creates [`MegaEvm`]s for alloy-evm consumers such as a node's block executor.
///
/// The factory holds the [`ExternalEnvFactory`] that supplies each EVM with the SALT and oracle
/// environments of the block it executes, and the optional builder of the dynamic precompiles a
/// node adds on top of the Satin set.
#[derive(Clone, Default)]
pub struct MegaEvmFactory<ExtEnvFactory = EmptyExternalEnv> {
    external_env_factory: ExtEnvFactory,
    dyn_precompiles_builder: Option<DynPrecompilesBuilder>,
}

impl<ExtEnvFactory: fmt::Debug> fmt::Debug for MegaEvmFactory<ExtEnvFactory> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MegaEvmFactory")
            .field("external_env_factory", &self.external_env_factory)
            .field("dyn_precompiles_builder", &self.dyn_precompiles_builder.is_some())
            .finish()
    }
}

impl MegaEvmFactory<EmptyExternalEnv> {
    /// Creates a factory whose EVMs have no external environments.
    pub const fn new() -> Self {
        Self { external_env_factory: EmptyExternalEnv, dyn_precompiles_builder: None }
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
        MegaEvmFactory {
            external_env_factory,
            dyn_precompiles_builder: self.dyn_precompiles_builder,
        }
    }

    /// Adds a builder of dynamic precompiles. Every EVM the factory creates runs the Satin set
    /// with what the builder returns applied on top.
    pub fn with_dyn_precompiles_builder(mut self, builder: DynPrecompilesBuilder) -> Self {
        self.dyn_precompiles_builder = Some(builder);
        self
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
    /// The Satin precompile set, with whatever the factory's builder added to it.
    type Precompiles = PrecompilesMap;

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
        let spec = cfg_env.spec;
        let ctx = MegaContext::new_with_external_envs(db, spec, external_envs)
            .with_cfg(cfg_env)
            .with_block(block_env);
        let evm = MegaEvm::new(ctx);
        match &self.dyn_precompiles_builder {
            Some(builder) => evm.with_dyn_precompiles(builder(spec)),
            None => evm,
        }
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
    #[cfg(not(feature = "std"))]
    use alloc as std;
    use std::sync::Arc;

    use super::*;
    use crate::{
        satin_precompiles,
        test_utils::{op_transaction, MemoryDatabase},
        ExternalEnvs, SaltEnv, TestExternalEnvs,
    };
    use alloy_evm::{precompiles::DynPrecompile, Evm, EvmFactory};
    use alloy_op_evm::OpTx;
    use alloy_primitives::{address, Address, BlockNumber, Bytes, TxKind, U256};
    use core::{
        cell::Cell,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use revm::{
        context::{CfgEnv, TxEnv},
        precompile::{PrecompileId, PrecompileOutput},
        primitives::HashMap,
    };

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

    const CUSTOM: Address = address!("0x00000000000000000000000000000000000c0de0");
    const CALLER: Address = address!("0x00000000000000000000000000000000000ca11e");

    /// A dynamic precompile that returns its own name for a flat price.
    fn custom_precompile() -> DynPrecompile {
        DynPrecompile::new(PrecompileId::Custom("custom".into()), |input| {
            Ok(PrecompileOutput::new(1_234, Bytes::from_static(b"custom"), input.reservoir))
        })
    }

    fn evm_env() -> EvmEnv<MegaSpecId, BlockEnv> {
        EvmEnv {
            cfg_env: CfgEnv::new_with_spec(MegaSpecId::SATIN),
            block_env: BlockEnv { gas_limit: 10_000_000, ..Default::default() },
        }
    }

    /// A call from `CALLER` to `to`.
    fn call(to: Address) -> MegaTransaction {
        OpTx(op_transaction(TxEnv {
            caller: CALLER,
            kind: TxKind::Call(to),
            gas_limit: 1_000_000,
            ..Default::default()
        }))
    }

    /// The builder runs once per EVM, is handed the spec the EVM executes, and what it returns
    /// answers a call to its address.
    #[test]
    fn test_dyn_precompiles_builder_receives_the_spec() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let factory = MegaEvmFactory::new().with_dyn_precompiles_builder(Arc::new(|spec| {
            CALLS.fetch_add(1, Ordering::SeqCst);
            assert_eq!(spec, MegaSpecId::SATIN, "the builder is handed the EVM's spec");
            HashMap::from_iter([(CUSTOM, custom_precompile())])
        }));

        let mut evm = factory.create_evm(MemoryDatabase::default(), evm_env());

        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "built once");
        let result = evm.transact_raw(call(CUSTOM)).unwrap().result;
        assert_eq!(result.output().map(Bytes::as_ref), Some(b"custom".as_slice()));

        let (_db, _inspector, precompiles) = evm.components();
        assert!(precompiles.get(&CUSTOM).is_some(), "the builder's entry is in the set");
        for address in satin_precompiles().addresses() {
            assert!(precompiles.get(address).is_some(), "{address} survived the addition");
        }
    }

    /// Without a builder the EVM runs the Satin set and nothing else: the address answers as an
    /// empty account.
    #[test]
    fn test_without_a_builder_the_set_is_the_satin_one() {
        let mut evm = MegaEvmFactory::new().create_evm(MemoryDatabase::default(), evm_env());

        let result = evm.transact_raw(call(CUSTOM)).unwrap().result;
        assert!(result.is_success());
        assert_eq!(result.output().map(Bytes::as_ref), Some(b"".as_slice()));

        let (_db, _inspector, precompiles) = evm.components();
        assert!(precompiles.get(&CUSTOM).is_none());
        for address in satin_precompiles().addresses() {
            assert!(precompiles.get(address).is_some(), "{address} is in the set");
        }
    }

    /// A dynamic entry at an address the Satin set already holds replaces it.
    #[test]
    fn test_a_dyn_precompile_replaces_a_satin_entry() {
        let kzg = crate::kzg_point_evaluation::ADDRESS;
        let factory = MegaEvmFactory::new().with_dyn_precompiles_builder(Arc::new(move |_| {
            HashMap::from_iter([(kzg, custom_precompile())])
        }));

        let mut evm = factory.create_evm(MemoryDatabase::default(), evm_env());

        // Upstream's KZG refuses an empty input; the replacement answers it.
        let result = evm.transact_raw(call(kzg)).unwrap().result;
        assert_eq!(result.output().map(Bytes::as_ref), Some(b"custom".as_slice()));
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
