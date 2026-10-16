//! Factory of Satin EVMs for alloy-evm consumers.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::sync::Arc;

use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use alloy_primitives::BlockTimestamp;
use core::fmt;
use op_revm::OpHaltReason;
use revm::{
    context::{result::EVMError, BlockEnv, DBErrorMarker},
    inspector::NoOpInspector,
    Inspector,
};

use crate::{
    DynPrecompilesBuilder, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvFactory,
    HardforkParams, MegaContext, MegaEvm, MegaHardforks, MegaSpecId, MegaTransaction,
    MegaTransactionError, ProtocolLimits,
};

/// Reads the limits a chain's schedule carries at a block's timestamp.
type ProtocolLimitsResolver = Arc<dyn Fn(BlockTimestamp) -> Option<ProtocolLimits> + Send + Sync>;

/// Where the EVMs a factory creates take their per-transaction limits from.
#[derive(Clone, Default)]
enum TxRuntimeLimitsSource {
    /// The protocol's defaults, [`ProtocolLimits::DEFAULT`]: the factory holds no schedule.
    #[default]
    ProtocolDefault,
    /// The chain's, read from its schedule at the block's timestamp.
    Schedule(ProtocolLimitsResolver),
    /// The caller's, whatever the block.
    Fixed(EvmTxRuntimeLimits),
}

impl fmt::Debug for TxRuntimeLimitsSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtocolDefault => f.write_str("ProtocolDefault"),
            Self::Schedule(_) => f.write_str("Schedule"),
            Self::Fixed(limits) => f.debug_tuple("Fixed").field(limits).finish(),
        }
    }
}

/// Creates [`MegaEvm`]s for alloy-evm consumers such as a node's block executor.
///
/// The factory holds the [`ExternalEnvFactory`] that supplies each EVM with the SALT and oracle
/// environments of the block it executes, the optional builder of the dynamic precompiles a node
/// adds on top of the Satin set, and where the EVMs take their per-transaction limits from.
///
/// # The limits an EVM runs under
///
/// An EVM the factory creates runs under the limits block execution would hold its block's
/// transactions to, so an RPC call, a simulation or a tool built on the factory stops what the
/// chain stops without having to be told:
///
/// - given the chain's schedule ([`with_schedule`](Self::with_schedule)), the limits the schedule
///   carries at the block's timestamp ([`MegaHardforks::protocol_limits`]);
/// - without one, the protocol's defaults ([`ProtocolLimits::DEFAULT`]), which is what block
///   execution runs on a chain that keeps them;
/// - where the schedule carries no limits at the timestamp, or limits their own check refuses
///   ([`HardforkParams::validate`]), the protocol's defaults too: block execution refuses such a
///   block, and an EVM cannot refuse to be built.
///
/// [`with_tx_runtime_limits`](Self::with_tx_runtime_limits) is the explicit opt-out: every EVM
/// runs under exactly the limits it is given, as the execution-spec gate and tests need. Block
/// execution installs the chain's limits on the EVM whatever it was created with.
#[derive(Clone, Default)]
pub struct MegaEvmFactory<ExtEnvFactory = EmptyExternalEnv> {
    external_env_factory: ExtEnvFactory,
    dyn_precompiles_builder: Option<DynPrecompilesBuilder>,
    tx_runtime_limits: TxRuntimeLimitsSource,
}

impl<ExtEnvFactory: fmt::Debug> fmt::Debug for MegaEvmFactory<ExtEnvFactory> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MegaEvmFactory")
            .field("external_env_factory", &self.external_env_factory)
            .field("dyn_precompiles_builder", &self.dyn_precompiles_builder.is_some())
            .field("tx_runtime_limits", &self.tx_runtime_limits)
            .finish()
    }
}

impl MegaEvmFactory<EmptyExternalEnv> {
    /// Creates a factory whose EVMs have no external environments and run under the protocol's
    /// default limits.
    pub const fn new() -> Self {
        Self {
            external_env_factory: EmptyExternalEnv,
            dyn_precompiles_builder: None,
            tx_runtime_limits: TxRuntimeLimitsSource::ProtocolDefault,
        }
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
            tx_runtime_limits: self.tx_runtime_limits,
        }
    }

    /// Adds a builder of dynamic precompiles. Every EVM the factory creates runs the Satin set
    /// with what the builder returns applied on top.
    pub fn with_dyn_precompiles_builder(mut self, builder: DynPrecompilesBuilder) -> Self {
        self.dyn_precompiles_builder = Some(builder);
        self
    }

    /// Gives the factory the chain's schedule: every EVM it creates runs under the limits the
    /// schedule carries at the block's timestamp, as block execution holds that block's
    /// transactions to them. It replaces limits set with
    /// [`with_tx_runtime_limits`](Self::with_tx_runtime_limits).
    ///
    /// A node hands it the schedule it hands its block executor factory, so the EVMs it builds
    /// outside block execution — an RPC call, a simulation — stop what the chain stops. A schedule
    /// held in an `Arc` is handed over as a clone of the `Arc`: [`MegaHardforks`] holds for an
    /// `Arc` or a `Box` of a schedule.
    pub fn with_schedule<Spec>(mut self, spec: Spec) -> Self
    where
        Spec: MegaHardforks + Send + Sync + 'static,
    {
        self.tx_runtime_limits = TxRuntimeLimitsSource::Schedule(Arc::new(move |timestamp| {
            spec.protocol_limits(timestamp)
        }));
        self
    }

    /// Runs every EVM the factory creates under exactly `limits`, whatever the block and whatever
    /// the chain's schedule says: the explicit opt-out, for the execution-spec gate's equivalence
    /// mode ([`EvmTxRuntimeLimits::no_limits`]) and for tests. It replaces a schedule given with
    /// [`with_schedule`](Self::with_schedule).
    pub fn with_tx_runtime_limits(mut self, limits: EvmTxRuntimeLimits) -> Self {
        self.tx_runtime_limits = TxRuntimeLimitsSource::Fixed(limits);
        self
    }

    /// The limits an EVM the factory creates for a block at `timestamp` runs under.
    pub fn tx_runtime_limits(&self, timestamp: BlockTimestamp) -> EvmTxRuntimeLimits {
        let protocol_default = ProtocolLimits::DEFAULT.tx_runtime_limits;
        match &self.tx_runtime_limits {
            TxRuntimeLimitsSource::ProtocolDefault => protocol_default,
            TxRuntimeLimitsSource::Schedule(resolve) => resolve(timestamp)
                .filter(|limits| limits.validate().is_ok())
                .map_or(protocol_default, |limits| limits.tx_runtime_limits),
            TxRuntimeLimitsSource::Fixed(limits) => *limits,
        }
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

    /// Creates an EVM for the block in `evm_env`, with the external environments of that block,
    /// running under the limits the factory resolves for the block's timestamp
    /// ([`tx_runtime_limits`](MegaEvmFactory::tx_runtime_limits)).
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
        let limits = self.tx_runtime_limits(block_env.timestamp.saturating_to());
        let spec = cfg_env.spec;
        let ctx = MegaContext::new_with_external_envs(db, spec, external_envs)
            .with_cfg(cfg_env)
            .with_block(block_env)
            .with_tx_runtime_limits(limits);
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
        ExternalEnvs, MegaHardforkConfig, SaltEnv, TestExternalEnvs,
    };
    use alloy_evm::{precompiles::DynPrecompile, Evm, EvmFactory};
    use alloy_op_evm::OpTx;
    use alloy_primitives::{address, Address, BlockNumber, Bytes, TxKind, U256};
    use core::{
        cell::Cell,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use revm::{
        context::{CfgEnv, ContextTr, TxEnv},
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

    /// The configuration [`spec_cfg`](crate::spec_cfg) returns is the one an EVM created from the
    /// same env runs with, in both views, before and after it runs a transaction: a node that
    /// mirrors it in its `EvmEnv` reads what the EVM executes on. A deposit runs on the schedule
    /// that prices no history, and the next transaction that pays history is back on the spec's.
    #[test]
    fn test_the_exported_cfg_is_the_one_the_evm_runs_with() {
        let mut cfg_env = CfgEnv::new_with_spec(MegaSpecId::SATIN);
        cfg_env.chain_id = 4326;
        cfg_env.disable_nonce_check = true;
        cfg_env.tx_gas_limit_cap = Some(1 << 24);
        cfg_env.enable_amsterdam_eip8037 = false;
        cfg_env.enable_amsterdam_eip2780 = false;
        cfg_env.enable_amsterdam_eip7708 = false;
        cfg_env.limit_contract_code_size = None;
        let exported = crate::spec_cfg(cfg_env.clone());
        assert_eq!(crate::spec_cfg(exported.clone()), exported, "applying it twice is a no-op");
        assert_eq!(exported.chain_id, 4326, "the caller's own fields are kept");
        assert!(exported.disable_nonce_check);
        let op_view = exported.clone().with_spec_and_gas_params(
            MegaSpecId::SATIN.into_op_spec(),
            exported.gas_params.clone(),
        );

        let mut evm = MegaEvmFactory::new().create_evm(
            MemoryDatabase::default(),
            EvmEnv { cfg_env, block_env: evm_env().block_env },
        );
        let runs_with = |evm: &MegaEvm<MemoryDatabase, NoOpInspector>| {
            (evm.ctx().mega_cfg().clone(), evm.ctx().cfg().clone())
        };
        assert_eq!(runs_with(&evm), (exported.clone(), op_view.clone()));
        let on_chain = |mut tx: MegaTransaction| {
            tx.0.base.chain_id = Some(4326);
            tx
        };

        evm.transact_raw(on_chain(call(CUSTOM))).unwrap();
        assert_eq!(runs_with(&evm), (exported.clone(), op_view.clone()));

        let mut deposit = on_chain(call(CUSTOM));
        deposit.0.deposit.source_hash = alloy_primitives::B256::repeat_byte(0x11);
        evm.transact_raw(deposit).unwrap();
        let (mega, _) = runs_with(&evm);
        assert_eq!(
            mega.gas_params,
            crate::satin_gas_params_history_exempt(),
            "a deposit runs the schedule that prices no history",
        );
        assert_eq!(
            mega.with_spec_and_gas_params(exported.spec, exported.gas_params.clone()),
            exported,
            "and nothing else of the configuration moves",
        );

        evm.transact_raw(on_chain(call(CUSTOM))).unwrap();
        assert_eq!(runs_with(&evm), (exported, op_view));
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
        call_with_gas_limit(to, 1_000_000)
    }

    /// A call from `CALLER` to `to` on `gas_limit`.
    fn call_with_gas_limit(to: Address, gas_limit: u64) -> MegaTransaction {
        OpTx(op_transaction(TxEnv {
            caller: CALLER,
            kind: TxKind::Call(to),
            gas_limit,
            ..Default::default()
        }))
    }

    /// The factory prints its external environment factory, whether a precompile builder is
    /// installed and where its EVMs take their limits from. A closure has no `Debug`, so the
    /// builder and the schedule are reported by name rather than dropped: a reader of a node's log
    /// can tell the configurations apart.
    #[test]
    fn test_debug_reports_the_factorys_configuration() {
        let factory = MegaEvmFactory::new();
        assert_eq!(
            format!("{factory:?}"),
            "MegaEvmFactory { external_env_factory: EmptyExternalEnv, \
             dyn_precompiles_builder: false, tx_runtime_limits: ProtocolDefault }"
        );

        let with_builder = factory.with_dyn_precompiles_builder(Arc::new(|_| HashMap::default()));
        assert_eq!(
            format!("{with_builder:?}"),
            "MegaEvmFactory { external_env_factory: EmptyExternalEnv, \
             dyn_precompiles_builder: true, tx_runtime_limits: ProtocolDefault }"
        );

        let on_schedule = with_builder.with_schedule(MegaHardforkConfig::default());
        assert!(format!("{on_schedule:?}").ends_with("tx_runtime_limits: Schedule }"));
        let fixed = on_schedule.with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
        assert!(format!("{fixed:?}").contains("tx_runtime_limits: Fixed(EvmTxRuntimeLimits {"));
    }

    /// A timestamp from which a chain's schedule activates Satin in these tests.
    const SATIN_AT: u64 = 1_000;

    /// A chain that activates Satin at [`SATIN_AT`] and holds its transactions to `limits`,
    /// attached unchecked so a test may hand it limits no chain may carry.
    fn chain(limits: ProtocolLimits) -> MegaHardforkConfig {
        MegaHardforkConfig::default()
            .with(crate::MegaHardfork::Satin, alloy_hardforks::ForkCondition::Timestamp(SATIN_AT))
            .with_params_unchecked(limits)
    }

    /// A chain's own limits, which no default carries.
    fn chain_limits() -> ProtocolLimits {
        ProtocolLimits::DEFAULT.with_tx_runtime_limits(
            ProtocolLimits::DEFAULT
                .tx_runtime_limits
                .with_tx_data_size_limit(crate::TX_BODY_SIZE + 40)
                .with_tx_kv_update_limit(7),
        )
    }

    /// An EVM the factory creates for a block at `timestamp`.
    fn evm_at(factory: &MegaEvmFactory, timestamp: u64) -> MegaEvm<MemoryDatabase, NoOpInspector> {
        let block_env = BlockEnv { timestamp: U256::from(timestamp), ..evm_env().block_env };
        factory
            .create_evm(MemoryDatabase::default(), EvmEnv { cfg_env: evm_env().cfg_env, block_env })
    }

    /// Without a schedule the factory's EVMs run under the protocol's default limits, which is
    /// what block execution runs on a chain that keeps them — not a bare context's, which leave
    /// the data size unlimited.
    #[test]
    fn test_without_a_schedule_an_evm_runs_under_the_protocols_defaults() {
        let evm = evm_at(&MegaEvmFactory::new(), SATIN_AT);
        assert_eq!(*evm.tx_runtime_limits(), ProtocolLimits::DEFAULT.tx_runtime_limits);
        assert_ne!(*evm.tx_runtime_limits(), EvmTxRuntimeLimits::default());
    }

    /// A schedule shared behind an `Arc`, as a node holds its chain spec, is handed over as a
    /// clone of the `Arc`, and resolves the limits the schedule it points to carries.
    #[test]
    fn test_a_schedule_behind_an_arc_is_a_schedule() {
        let shared = Arc::new(chain(chain_limits()));
        let factory = MegaEvmFactory::new().with_schedule(Arc::clone(&shared));
        assert_eq!(factory.tx_runtime_limits(SATIN_AT), chain_limits().tx_runtime_limits);
        assert_eq!(
            factory.tx_runtime_limits(SATIN_AT),
            shared.protocol_limits(SATIN_AT).unwrap().tx_runtime_limits
        );
    }

    /// Given the chain's schedule, the factory's EVMs run under the limits it carries at the
    /// block's timestamp; where it carries none, or limits their own check refuses — both of which
    /// block execution refuses — under the protocol's defaults.
    #[test]
    fn test_an_evm_runs_under_the_limits_the_schedule_carries_at_its_block() {
        let factory = MegaEvmFactory::new().with_schedule(chain(chain_limits()));
        assert_eq!(
            *evm_at(&factory, SATIN_AT).tx_runtime_limits(),
            chain_limits().tx_runtime_limits
        );
        assert_eq!(factory.tx_runtime_limits(SATIN_AT + 1), chain_limits().tx_runtime_limits);
        assert_eq!(
            factory.tx_runtime_limits(SATIN_AT - 1),
            ProtocolLimits::DEFAULT.tx_runtime_limits,
            "before Satin the schedule carries no limits"
        );

        let refused = MegaEvmFactory::new().with_schedule(chain(ProtocolLimits::no_limits()));
        assert_eq!(
            *evm_at(&refused, SATIN_AT).tx_runtime_limits(),
            ProtocolLimits::DEFAULT.tx_runtime_limits,
            "limits their own check refuses run no EVM"
        );
    }

    /// The opt-out: limits given to the factory are what every EVM runs under, whatever the
    /// schedule says; the last of the two settings made is the one that holds.
    #[test]
    fn test_limits_given_to_the_factory_replace_the_schedules() {
        let factory = MegaEvmFactory::new()
            .with_schedule(chain(chain_limits()))
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
        for timestamp in [SATIN_AT - 1, SATIN_AT] {
            assert_eq!(
                *evm_at(&factory, timestamp).tx_runtime_limits(),
                EvmTxRuntimeLimits::no_limits()
            );
        }

        let factory = factory.with_schedule(chain(chain_limits()));
        assert_eq!(
            *evm_at(&factory, SATIN_AT).tx_runtime_limits(),
            chain_limits().tx_runtime_limits
        );
    }

    /// And the EVM enforces them, as block execution does: a call that keeps two writes where the
    /// chain allows one is stopped on an EVM from a factory given the schedule, and keeps both on
    /// one from a factory without it.
    #[test]
    fn test_an_evm_from_the_factory_stops_what_the_chain_stops() {
        use crate::{test_utils::BytecodeBuilder, LimitCheck, LimitKind};
        use revm::context_interface::cfg::GasId;
        const WRITER: Address = address!("0x0000000000000000000000000000000000077700");
        // Below the execution cap the two fresh slots' state gas spills onto regular gas, and the
        // body and the two write records pay history: 1,000,000 of regular room on top of what
        // they cost at the byte prices in effect, so the second write is made and its record
        // counted whatever a byte costs.
        let gas_limit = 1_000_000 +
            2 * crate::satin_gas_params().get(GasId::sstore_set_state_gas()) +
            crate::history_gas(crate::TX_BODY_SIZE + 2 * crate::WRITE_RECORD_SIZE).unwrap();
        let run = |factory: &MegaEvmFactory| {
            let mut db = MemoryDatabase::default();
            db.set_account_code(
                WRITER,
                BytecodeBuilder::default()
                    .sstore(U256::from(1), U256::from(1))
                    .sstore(U256::from(2), U256::from(1))
                    .stop()
                    .build(),
            );
            let block_env = BlockEnv { timestamp: U256::from(SATIN_AT), ..evm_env().block_env };
            let mut evm = factory.create_evm(db, EvmEnv { cfg_env: evm_env().cfg_env, block_env });
            evm.execute_transaction(call_with_gas_limit(WRITER, gas_limit))
                .expect("the call is valid")
        };

        let stopped = run(&MegaEvmFactory::new().with_schedule(chain(chain_limits())));
        assert_eq!(
            stopped.limit_exceeded,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit: crate::TX_BODY_SIZE + 40,
                used: crate::TX_BODY_SIZE + 2 * 40,
                frame_local: false,
            })
        );

        let kept = run(&MegaEvmFactory::new());
        assert_eq!(kept.limit_exceeded, None);
        assert!(kept.result.is_success());
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
