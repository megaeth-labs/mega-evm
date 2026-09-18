//! The execution context of the Satin engine.

use delegate::delegate;
use op_revm::{L1BlockInfo, OpSpecId};
use revm::{
    context::{BlockEnv, CfgEnv, Context, ContextError, ContextSetters, ContextTr, LocalContext},
    context_interface::{
        cfg::{GasId, GasParams, StateGasCharge, StateGasSite},
        context::{SStoreResult, SelfDestructResult, StateLoad},
        host::LoadError,
        journaled_state::{AccountInfoLoad, AccountLoad},
    },
    primitives::{Address, Bytes, Log, StorageKey, StorageValue, B256, U256},
    Database, Journal,
};

use crate::{
    constants, EmptyExternalEnv, ExternalEnvTypes, ExternalEnvs, MegaSpecId, MegaTransaction,
};

/// The revm context the Satin engine runs on: op-revm's context shape with the `MegaETH`
/// transaction type.
pub(crate) type MegaInnerContext<DB> =
    Context<BlockEnv, MegaTransaction, CfgEnv<OpSpecId>, DB, Journal<DB>, L1BlockInfo>;

/// Execution context of the Satin engine.
///
/// It wraps op-revm's context and adds what `MegaETH` execution needs on top: the `MegaETH`
/// spec and the external environments (SALT, oracle). The configuration is kept twice: the
/// [`MegaSpecId`] view that callers see and the [`OpSpecId`] view op-revm executes on. Both are
/// written together, only through [`MegaContext::with_cfg`], so they cannot drift apart.
///
/// Every [`Host`](revm::interpreter::Host) method and every context accessor delegates to the
/// wrapped context; later changes override the ones `MegaETH` prices or meters differently.
#[derive(Debug)]
pub struct MegaContext<DB: Database, ExtEnvs: ExternalEnvTypes = EmptyExternalEnv> {
    inner: MegaInnerContext<DB>,
    cfg: CfgEnv<MegaSpecId>,
    external_envs: ExternalEnvs<ExtEnvs>,
}

impl<DB: Database> MegaContext<DB, EmptyExternalEnv> {
    /// Creates a context over `db` that executes `spec`, without external environments.
    pub fn new(db: DB, spec: MegaSpecId) -> Self {
        Self::new_with_external_envs(db, spec, ExternalEnvs::default())
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Creates a context over `db` that executes `spec` with the given external environments.
    pub fn new_with_external_envs(
        db: DB,
        spec: MegaSpecId,
        external_envs: ExternalEnvs<ExtEnvs>,
    ) -> Self {
        let cfg = spec_cfg(CfgEnv::new_with_spec(spec));
        let inner = Context::new(db, spec.into_op_spec()).with_cfg(op_cfg(&cfg));
        Self { inner, cfg, external_envs }
    }

    /// Replaces the configuration.
    ///
    /// The fields the spec fixes are set from the spec, whatever `cfg` holds: the gas table, the
    /// EIP-8037 and EIP-2780 switches, the execution cap, the EIP-7708 switch and the system-call
    /// state-gas margin. Every other field (chain id, limits, disabled checks) is taken from
    /// `cfg`.
    pub fn with_cfg(mut self, cfg: CfgEnv<MegaSpecId>) -> Self {
        let cfg = spec_cfg(cfg);
        self.inner = self.inner.with_cfg(op_cfg(&cfg));
        self.cfg = cfg;
        self
    }

    /// Replaces the block environment.
    pub fn with_block(mut self, block: BlockEnv) -> Self {
        self.inner.block = block;
        self
    }

    /// Replaces the transaction.
    pub fn with_tx(mut self, tx: MegaTransaction) -> Self {
        self.inner.tx = tx;
        self
    }

    /// Replaces the L1 block info.
    pub fn with_chain(mut self, chain: L1BlockInfo) -> Self {
        self.inner.chain = chain;
        self
    }

    /// Modifies the L1 block info in place.
    pub fn modify_chain(&mut self, f: impl FnOnce(&mut L1BlockInfo)) {
        f(&mut self.inner.chain);
    }

    /// The spec this context executes.
    pub const fn spec(&self) -> MegaSpecId {
        self.cfg.spec
    }

    /// The configuration as callers see it, keyed by [`MegaSpecId`].
    ///
    /// [`ContextTr::cfg`] returns the [`OpSpecId`] view op-revm executes on.
    pub const fn mega_cfg(&self) -> &CfgEnv<MegaSpecId> {
        &self.cfg
    }

    /// The external environments (SALT, oracle) of this context.
    pub const fn external_envs(&self) -> &ExternalEnvs<ExtEnvs> {
        &self.external_envs
    }

    /// Consumes the context and returns the database, the configuration and the block.
    pub fn into_parts(self) -> (DB, CfgEnv<MegaSpecId>, BlockEnv) {
        let Context { block, journaled_state, .. } = self.inner;
        (journaled_state.database, self.cfg, block)
    }
}

/// Sets the configuration fields the spec fixes.
///
/// Satin runs on the Osaka gas table of its Karst base, with EIP-8037 state gas and the EIP-2780
/// intrinsic cost switched on and gas above the execution cap going to the state-gas reservoir.
/// The EIP-7708 transfer logs and the system-call reservoir margin stay off until the Satin gas
/// table and the system-call reservoir split switch them on.
fn spec_cfg(mut cfg: CfgEnv<MegaSpecId>) -> CfgEnv<MegaSpecId> {
    cfg.gas_params = GasParams::new_spec(cfg.spec.into_eth_spec());
    cfg.enable_amsterdam_eip8037 = true;
    cfg.enable_amsterdam_eip2780 = true;
    cfg.tx_gas_limit_cap = Some(constants::TX_GAS_LIMIT_CAP);
    cfg.enable_amsterdam_eip7708 = false;
    cfg.system_call_state_gas_margin_in_reservoir = false;
    cfg
}

/// The op-revm view of `cfg`: the same fields, keyed by the Optimism spec.
fn op_cfg(cfg: &CfgEnv<MegaSpecId>) -> CfgEnv<OpSpecId> {
    cfg.clone().with_spec_and_gas_params(cfg.spec.into_op_spec(), cfg.gas_params.clone())
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> ContextTr for MegaContext<DB, ExtEnvs> {
    type Block = BlockEnv;
    type Tx = MegaTransaction;
    type Cfg = CfgEnv<OpSpecId>;
    type Db = DB;
    type Journal = Journal<DB>;
    type Chain = L1BlockInfo;
    type Local = LocalContext;

    delegate! {
        to self.inner {
            fn all(
                &self,
            ) -> (
                &BlockEnv,
                &MegaTransaction,
                &CfgEnv<OpSpecId>,
                &DB,
                &Journal<DB>,
                &L1BlockInfo,
                &LocalContext,
            );
            fn all_mut(
                &mut self,
            ) -> (
                &BlockEnv,
                &MegaTransaction,
                &CfgEnv<OpSpecId>,
                &mut Journal<DB>,
                &mut L1BlockInfo,
                &mut LocalContext,
            );
            fn error(&mut self) -> &mut Result<(), ContextError<DB::Error>>;
        }
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> ContextSetters for MegaContext<DB, ExtEnvs> {
    delegate! {
        to self.inner {
            fn set_tx(&mut self, tx: MegaTransaction);
            fn set_block(&mut self, block: BlockEnv);
        }
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> revm::context_interface::Host
    for MegaContext<DB, ExtEnvs>
{
    delegate! {
        to self.inner {
            fn basefee(&self) -> U256;
            fn blob_gasprice(&self) -> U256;
            fn gas_limit(&self) -> U256;
            fn difficulty(&self) -> U256;
            fn prevrandao(&self) -> Option<U256>;
            fn block_number(&self) -> U256;
            fn timestamp(&self) -> U256;
            fn beneficiary(&self) -> Address;
            fn slot_num(&self) -> U256;
            fn chain_id(&self) -> U256;
            fn effective_gas_price(&self) -> U256;
            fn caller(&self) -> Address;
            fn blob_hash(&self, number: usize) -> Option<U256>;
            fn max_initcode_size(&self) -> usize;
            fn gas_params(&self) -> &GasParams;
            fn is_amsterdam_eip8037_enabled(&self) -> bool;
            fn state_gas_price(&mut self, id: GasId, site: StateGasSite) -> Option<u64>;
            fn state_gas_charge(&mut self, charge: StateGasCharge) -> Option<u64>;
            fn block_hash(&mut self, number: u64) -> Option<B256>;
            fn selfdestruct(
                &mut self,
                address: Address,
                target: Address,
                skip_cold_load: bool,
            ) -> Result<StateLoad<SelfDestructResult>, LoadError>;
            fn log(&mut self, log: Log);
            fn sstore_skip_cold_load(
                &mut self,
                address: Address,
                key: StorageKey,
                value: StorageValue,
                skip_cold_load: bool,
            ) -> Result<StateLoad<SStoreResult>, LoadError>;
            fn sstore(
                &mut self,
                address: Address,
                key: StorageKey,
                value: StorageValue,
            ) -> Option<StateLoad<SStoreResult>>;
            fn sload_skip_cold_load(
                &mut self,
                address: Address,
                key: StorageKey,
                skip_cold_load: bool,
            ) -> Result<StateLoad<StorageValue>, LoadError>;
            fn sload(&mut self, address: Address, key: StorageKey) -> Option<StateLoad<StorageValue>>;
            fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue);
            fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue;
            fn load_account_info_skip_cold_load(
                &mut self,
                address: Address,
                load_code: bool,
                skip_cold_load: bool,
            ) -> Result<AccountInfoLoad<'_>, LoadError>;
            fn balance(&mut self, address: Address) -> Option<StateLoad<U256>>;
            fn load_account_delegated(&mut self, address: Address) -> Option<StateLoad<AccountLoad>>;
            fn load_account_code(&mut self, address: Address) -> Option<StateLoad<Bytes>>;
            fn load_account_code_hash(&mut self, address: Address) -> Option<StateLoad<B256>>;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EthSpecId, SaltEnv, TestExternalEnvs};
    use core::convert::Infallible;
    use revm::database::EmptyDB;

    /// Asserts both configuration views carry the Satin switches.
    fn assert_satin_switches(ctx: &MegaContext<EmptyDB, impl ExternalEnvTypes>) {
        let (mega, op) = (ctx.mega_cfg(), ctx.cfg());
        assert_eq!(mega.spec, MegaSpecId::SATIN);
        assert_eq!(op.spec, OpSpecId::KARST);
        for (eip8037, eip2780, cap, eip7708, margin) in [
            (
                mega.enable_amsterdam_eip8037,
                mega.enable_amsterdam_eip2780,
                mega.tx_gas_limit_cap,
                mega.enable_amsterdam_eip7708,
                mega.system_call_state_gas_margin_in_reservoir,
            ),
            (
                op.enable_amsterdam_eip8037,
                op.enable_amsterdam_eip2780,
                op.tx_gas_limit_cap,
                op.enable_amsterdam_eip7708,
                op.system_call_state_gas_margin_in_reservoir,
            ),
        ] {
            assert!(eip8037, "EIP-8037 must be on");
            assert!(eip2780, "EIP-2780 must be on");
            assert_eq!(cap, Some(200_000_000), "execution cap");
            assert!(!eip7708, "EIP-7708 stays off until the Satin gas table");
            assert!(
                !margin,
                "the system-call reservoir margin stays off until the reservoir split"
            );
        }
        let osaka = GasParams::new_spec(EthSpecId::OSAKA);
        assert_eq!(mega.gas_params.table(), osaka.table());
        assert_eq!(op.gas_params.table(), osaka.table());
    }

    #[test]
    fn test_new_context_carries_the_satin_switches() {
        let ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN);
        assert_eq!(ctx.spec(), MegaSpecId::SATIN);
        assert_satin_switches(&ctx);
    }

    /// A caller's configuration cannot switch off what the spec fixes; every other field is
    /// taken from it.
    #[test]
    fn test_with_cfg_keeps_the_spec_switches() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
        cfg.chain_id = 4326;
        cfg.enable_amsterdam_eip8037 = false;
        cfg.enable_amsterdam_eip2780 = false;
        cfg.tx_gas_limit_cap = Some(1 << 24);
        cfg.enable_amsterdam_eip7708 = true;
        cfg.system_call_state_gas_margin_in_reservoir = true;
        cfg.gas_params = GasParams::new_spec(EthSpecId::AMSTERDAM);

        let ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN).with_cfg(cfg);

        assert_satin_switches(&ctx);
        assert_eq!(ctx.mega_cfg().chain_id, 4326);
        assert_eq!(ctx.cfg().chain_id, 4326);
    }

    /// The op-revm view is the `MegaETH` view keyed by the base spec, field for field.
    #[test]
    fn test_with_cfg_spec_consistency() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
        cfg.chain_id = 6343;
        cfg.limit_contract_code_size = Some(512 * 1024);
        cfg.disable_nonce_check = true;

        let ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN).with_cfg(cfg);
        let (mega, op) = (ctx.mega_cfg(), ctx.cfg());

        assert_eq!(op.spec, mega.spec.into_op_spec());
        assert_eq!(op.chain_id, 6343);
        assert_eq!(op.limit_contract_code_size, Some(512 * 1024));
        assert!(op.disable_nonce_check);
        assert_eq!(
            mega.clone().with_spec_and_gas_params(op.spec, mega.gas_params.clone()),
            op.clone()
        );
    }

    #[test]
    fn test_modify_chain_edits_the_l1_block_info() {
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN);
        ctx.modify_chain(|chain| chain.l2_block = Some(U256::from(42)));
        assert_eq!(ctx.chain().l2_block, Some(U256::from(42)));
    }

    /// The external environments given at construction are the ones the context exposes.
    #[test]
    fn test_new_with_ext_envs_builds_over_configurable_env() {
        let env = TestExternalEnvs::<Infallible>::new().with_bucket_capacity(7, 1_024);
        let ctx = MegaContext::new_with_external_envs(
            EmptyDB::default(),
            MegaSpecId::SATIN,
            ExternalEnvs::from(env),
        );

        assert_eq!(ctx.spec(), MegaSpecId::SATIN);
        assert_eq!(ctx.external_envs().salt_env.get_bucket_capacity(7).unwrap(), 1_024);
        assert_satin_switches(&ctx);
    }
}
