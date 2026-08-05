use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use op_revm::L1BlockInfo;
use revm::{context::result::EVMError, Inspector};

use crate::{
    DynPrecompilesBuilder, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvFactory, MegaContext,
    MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
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
    // Mirrors `alloy_evm::Evm::Error` on `MegaEvm`: the transaction error must implement
    // `alloy_evm::InvalidTxError`, which only the `alloy-op-evm` newtype provides.
    type Error<DBError: revm::context::DBErrorMarker + core::error::Error + Send + Sync + 'static> =
        EVMError<DBError, alloy_op_evm::OpTxError>;
    type HaltReason = MegaHaltReason;
    type Spec = MegaSpecId;
    type BlockEnv = revm::context::BlockEnv;
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

    use alloy_evm::{Evm as _, EvmFactory as _};
    use alloy_primitives::{address, keccak256, Address, Bytes, U256};
    use revm::{
        bytecode::Bytecode,
        context::{tx::TxEnvBuilder, BlockEnv, CfgEnv},
        context_interface::cfg::{GasId, GasParams},
        primitives::hardfork::SpecId,
        state::AccountInfo,
    };

    use crate::test_utils::MemoryDatabase;

    const CHAIN_ID: u64 = 6342;
    const SENDER: Address = address!("0000000000000000000000000000000000000f00");
    const TARGET: Address = address!("0000000000000000000000000000000000000f01");
    /// revm's mainnet cost per calldata token.
    const MAINNET_TX_TOKEN_COST: u64 = 4;
    /// The per-token cost an embedder installs in place of the mainnet one.
    const CUSTOM_TX_TOKEN_COST: u64 = 40;
    /// revm's mainnet EIP-7623 floor cost per calldata token.
    const MAINNET_TX_FLOOR_COST_PER_TOKEN: u64 = 10;
    /// revm's mainnet base cost of a transaction, the constant part of the EIP-7623 floor.
    const MAINNET_TX_BASE_STIPEND: u64 = 21_000;
    /// Zero calldata bytes are one EIP-7623 token each. This many keeps the transaction above
    /// every gas floor, so its gas used tracks the per-token cost of the schedule directly.
    const UNFLOORED_CALLDATA_TOKENS: u64 = 100;
    /// Enough calldata tokens that the EIP-7623 floor rises above the transaction's own cost
    /// (`MegaETH`'s calldata storage gas included), so the floor decides the gas used.
    const FLOOR_BINDING_CALLDATA_TOKENS: u64 = 2_000;
    /// `PUSH1 1 PUSH1 1 SSTORE STOP` — writes a non-zero value into a previously empty slot.
    /// Under EIP-8037 the fresh-slot part of that `SSTORE` is charged as state gas on top of its
    /// regular cost, so this transaction's gas moves as soon as the flag is live.
    const SSTORE_FRESH_SLOT_CODE: [u8; 6] = [0x60, 0x01, 0x60, 0x01, 0x55, 0x00];

    #[test]
    fn test_external_env_factory_getter() {
        let factory = MegaEvmFactory::new().with_external_env_factory(EmptyExternalEnv);

        let got: &EmptyExternalEnv = factory.external_env_factory();

        // Verify the getter returns a stable reference to the same field.
        assert!(core::ptr::eq(got, factory.external_env_factory()));
    }

    /// The mainnet `PRAGUE` schedule with the calldata token cost moved off its mainnet value —
    /// the kind of override an embedder installs for its own chain.
    fn embedder_gas_params(tx_token_cost: u64) -> GasParams {
        let mut gas_params = GasParams::new_spec(SpecId::PRAGUE);
        gas_params.override_gas([(GasId::tx_token_cost(), tx_token_cost)]);
        gas_params
    }

    fn embedder_cfg(tx_token_cost: u64, disable_eip7623: bool) -> CfgEnv<MegaSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX6);
        cfg.chain_id = CHAIN_ID;
        cfg.gas_params = embedder_gas_params(tx_token_cost);
        cfg.disable_eip7623 = disable_eip7623;
        cfg
    }

    fn evm_env(cfg: CfgEnv<MegaSpecId>) -> EvmEnv<MegaSpecId> {
        EvmEnv::new(
            cfg,
            BlockEnv {
                number: U256::from(1),
                timestamp: U256::from(1_800_000_000),
                gas_limit: 30_000_000,
                basefee: 0,
                ..Default::default()
            },
        )
    }

    /// Executes one calldata-only transaction through a factory-built EVM and returns its gas
    /// used. Everything except the config is held fixed, so a gas delta between two runs can
    /// only come from the config the embedder handed to the factory.
    fn run_calldata_tx(cfg: CfgEnv<MegaSpecId>, calldata_tokens: u64) -> u64 {
        let mut db = MemoryDatabase::default();
        let mut evm = MegaEvmFactory::new().create_evm(&mut db, evm_env(cfg));
        evm.ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });

        let tx = TxEnvBuilder::new()
            .caller(SENDER)
            .call(TARGET)
            .chain_id(Some(CHAIN_ID))
            .data(Bytes::from(vec![0u8; calldata_tokens as usize]))
            .gas_limit(1_000_000)
            .build_fill();
        let mut tx = alloy_op_evm::OpTx(op_revm::OpTransaction::new(tx));
        tx.enveloped_tx = Some(Bytes::new());

        let result = evm.transact_raw(tx).expect("probe transaction must execute");
        assert!(result.result.is_success(), "probe transaction must succeed: {:?}", result.result);
        result.result.tx_gas_used()
    }

    /// An embedder's `CfgEnv` must reach the EVM and come back out unchanged: `create_evm`
    /// converts it to revm's `OpSpecId` shape and `cfg_env` / `finish` convert it back, and
    /// neither leg may silently reset a field to its revm default.
    #[test]
    fn test_create_evm_round_trips_embedder_cfg() {
        let cfg = embedder_cfg(40, true);
        let db = MemoryDatabase::default();

        let evm = MegaEvmFactory::new().create_evm(db, evm_env(cfg.clone()));

        // Read back through the `MegaSpecId`-typed view the embedder sees.
        let read_back = evm.cfg_env();
        assert_eq!(read_back.spec, MegaSpecId::REX6);
        assert_eq!(read_back.chain_id, CHAIN_ID);
        assert_eq!(read_back.gas_params, cfg.gas_params, "custom gas schedule must survive");
        assert!(read_back.disable_eip7623, "revm 40 switches must survive");

        // And through `finish`, which hands the config back to the embedder.
        let (_db, evm_env) = evm.finish();
        assert_eq!(evm_env.cfg_env.spec, MegaSpecId::REX6);
        assert_eq!(evm_env.cfg_env.chain_id, CHAIN_ID);
        assert_eq!(evm_env.cfg_env.gas_params, cfg.gas_params);
        assert!(evm_env.cfg_env.disable_eip7623);
    }

    /// The production path — `create_evm` routing the embedder's `EvmEnv` through `with_cfg` —
    /// pins the chain-id gate off however the config arrives: revm 40 defaults the flag to
    /// `true`, and every frozen `MegaETH` spec ran without the gate.
    #[test]
    fn test_create_evm_pins_chain_id_check_off() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX6);
        cfg.chain_id = CHAIN_ID;
        assert!(cfg.tx_chain_id_check, "revm 40 must default the gate on for this test to bite");

        let evm = MegaEvmFactory::new().create_evm(MemoryDatabase::default(), evm_env(cfg));

        assert!(
            !evm.ctx.cfg.tx_chain_id_check,
            "a factory-built EVM must run with the revm-27 chain-id semantics"
        );
    }

    /// Carrying the config is not enough — it must also drive execution. A custom per-token
    /// calldata cost moves the same transaction's gas by exactly its per-token delta.
    #[test]
    fn test_embedder_gas_schedule_takes_effect_on_gas() {
        let mainnet_schedule =
            run_calldata_tx(embedder_cfg(MAINNET_TX_TOKEN_COST, true), UNFLOORED_CALLDATA_TOKENS);
        let custom_schedule =
            run_calldata_tx(embedder_cfg(CUSTOM_TX_TOKEN_COST, true), UNFLOORED_CALLDATA_TOKENS);

        assert_eq!(
            custom_schedule - mainnet_schedule,
            (CUSTOM_TX_TOKEN_COST - MAINNET_TX_TOKEN_COST) * UNFLOORED_CALLDATA_TOKENS,
            "the embedder's per-token calldata cost must price the transaction"
        );
    }

    /// Same for the `disable_eip7623` switch: with enough calldata for the floor to bind, turning
    /// EIP-7623 off removes exactly revm's floor cost from the transaction's gas.
    #[test]
    fn test_embedder_eip7623_switch_takes_effect_on_gas() {
        let with_eip7623 = run_calldata_tx(
            embedder_cfg(MAINNET_TX_TOKEN_COST, false),
            FLOOR_BINDING_CALLDATA_TOKENS,
        );
        let without_eip7623 = run_calldata_tx(
            embedder_cfg(MAINNET_TX_TOKEN_COST, true),
            FLOOR_BINDING_CALLDATA_TOKENS,
        );

        assert_eq!(
            with_eip7623 - without_eip7623,
            MAINNET_TX_BASE_STIPEND +
                MAINNET_TX_FLOOR_COST_PER_TOKEN * FLOOR_BINDING_CALLDATA_TOKENS,
            "disabling EIP-7623 must drop revm's calldata floor from the transaction's gas"
        );
    }

    /// A config whose gas schedule prices state gas: the mainnet `PRAGUE` table with Amsterdam's
    /// charge for setting a fresh storage slot dropped in. An embedder can install exactly this,
    /// which is what leaves EIP-8037 one flag away from repricing a frozen spec.
    fn state_gas_priced_cfg() -> CfgEnv<MegaSpecId> {
        let amsterdam_sstore_set_state_gas =
            GasParams::new_spec(SpecId::AMSTERDAM).get(GasId::sstore_set_state_gas());
        assert_ne!(amsterdam_sstore_set_state_gas, 0, "state gas must be priced for this probe");

        let mut cfg = embedder_cfg(MAINNET_TX_TOKEN_COST, true);
        cfg.gas_params
            .override_gas([(GasId::sstore_set_state_gas(), amsterdam_sstore_set_state_gas)]);
        cfg
    }

    /// Executes one transaction into [`SSTORE_FRESH_SLOT_CODE`] planted at [`TARGET`] and returns
    /// its gas used.
    fn run_sstore_tx(cfg: CfgEnv<MegaSpecId>) -> u64 {
        let mut db = MemoryDatabase::default();
        let code = Bytes::from_static(&SSTORE_FRESH_SLOT_CODE);
        db.insert_account_info(
            TARGET,
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );

        let mut evm = MegaEvmFactory::new().create_evm(&mut db, evm_env(cfg));
        evm.ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let tx = TxEnvBuilder::new()
            .caller(SENDER)
            .call(TARGET)
            .chain_id(Some(CHAIN_ID))
            .gas_limit(1_000_000)
            .build_fill();
        let mut tx = alloy_op_evm::OpTx(op_revm::OpTransaction::new(tx));
        tx.enveloped_tx = Some(Bytes::new());

        let result = evm.transact_raw(tx).expect("probe transaction must execute");
        assert!(result.result.is_success(), "probe transaction must succeed: {:?}", result.result);
        result.result.tx_gas_used()
    }

    /// EIP-8037 is the one `CfgEnv` field an embedder does not own. `MegaETH`'s gas accounting
    /// assumes no state-gas split exists, so the flag is forced off before the EVM is built and
    /// again before every transaction, reads back off, and setting it changes nothing about what a
    /// transaction costs.
    ///
    /// The probe is a fresh-slot `SSTORE` under a schedule that prices state gas —
    /// `state_gas_priced_cfg` asserts the Amsterdam charge it installs is non-zero, so a live
    /// split would land on this transaction. There is deliberately no "forced past the pin"
    /// control any more: the force now happens inside the transaction, after any window a test
    /// could write the flag in, which is the property being asserted.
    #[test]
    fn test_embedder_cannot_enable_amsterdam_eip8037() {
        let mut cfg = state_gas_priced_cfg();
        cfg.enable_amsterdam_eip8037 = true;

        let evm = MegaEvmFactory::new().create_evm(MemoryDatabase::default(), evm_env(cfg.clone()));

        // The config the EVM runs with, and the one the embedder reads back, both say off.
        assert!(
            !evm.ctx.inner.cfg.enable_amsterdam_eip8037,
            "the config handed to revm must have EIP-8037 pinned off"
        );
        assert!(
            !evm.cfg_env().enable_amsterdam_eip8037,
            "`cfg_env` must report the flag the EVM actually runs with"
        );
        let (_db, read_back) = evm.finish();
        assert!(
            !read_back.cfg_env.enable_amsterdam_eip8037,
            "`finish` must report the flag the EVM actually runs with"
        );

        // And execution never enters state-gas accounting: a fresh-slot `SSTORE` costs the same
        // whether or not the embedder asked for EIP-8037, even on a schedule that prices state
        // gas.
        assert_eq!(
            run_sstore_tx(cfg),
            run_sstore_tx(state_gas_priced_cfg()),
            "an embedder's EIP-8037 request must not reprice a fresh-slot SSTORE"
        );

        // And the state-gas price in the schedule is inert on its own: installing it changes
        // nothing either, so no part of execution reads the state-gas table.
        assert_eq!(
            run_sstore_tx(state_gas_priced_cfg()),
            run_sstore_tx(embedder_cfg(MAINNET_TX_TOKEN_COST, true)),
            "a schedule that prices state gas must not reprice a transaction while the split is off"
        );
    }
}
