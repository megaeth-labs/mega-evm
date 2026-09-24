//! The execution context of the Satin engine.

use delegate::delegate;
use op_revm::{transaction::deposit::DEPOSIT_TRANSACTION_TYPE, L1BlockInfo, OpSpecId};
use revm::{
    context::{
        BlockEnv, CfgEnv, Context, ContextError, ContextSetters, ContextTr, LocalContext,
        Transaction,
    },
    context_interface::cfg::GasId,
    primitives::{Address, StorageKey},
    Database, Journal,
};

use crate::{
    constants,
    evm::{
        history::transaction_body_bytes,
        schedule::{satin_gas_params, satin_gas_params_history_exempt},
    },
    system::{self, keyless::KeylessCall, MEGA_SYSTEM_ADDRESS},
    AdditionalLimit, BlockHashRecord, BucketError, BucketMultipliers, EmptyExternalEnv,
    EvmTxRuntimeLimits, ExternalEnvTypes, ExternalEnvs, MegaSpecId, MegaTransaction, SaltEnv,
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
/// written together, only through [`MegaContext::with_cfg`] (and the test tooling's neutral
/// configuration), so they cannot drift apart.
///
/// Every context accessor delegates to the wrapped context, and so does every
/// [`Host`](revm::interpreter::Host) method except the three that stage what a state-writing
/// opcode did (see the `host` module). It also carries the common execution layer's state for the
/// running transaction ([`AdditionalLimit`]) and the SALT bucket multipliers that transaction has
/// priced state gas with ([`BucketMultipliers`]).
#[derive(Debug)]
pub struct MegaContext<DB: Database, ExtEnvs: ExternalEnvTypes = EmptyExternalEnv> {
    pub(crate) inner: MegaInnerContext<DB>,
    cfg: CfgEnv<MegaSpecId>,
    external_envs: ExternalEnvs<ExtEnvs>,
    pub(crate) additional_limit: AdditionalLimit,
    pub(crate) block_hash_record: BlockHashRecord,
    /// The SALT bucket multipliers the running transaction has read.
    bucket_multipliers: BucketMultipliers,
    /// Whether the running transaction is system-originated, and so prices its state gas at the
    /// minimum bucket. See [`system::is_system_originated`].
    system_originated: bool,
    /// Whether the running transaction pays history gas. See [`MegaContext::prices_history`].
    prices_history: bool,
    /// The `keylessDeploy` call the running transaction's creation runs under, from the rewrite
    /// that started the creation until the creation's result is settled into it. See the
    /// [`keyless`](crate::system::keyless) module.
    pub(crate) keyless_call: Option<KeylessCall>,
    /// Whether the context runs the neutral configuration. See [`MegaContext::with_neutral_cfg`].
    #[cfg(any(test, feature = "test-utils"))]
    neutral: bool,
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
        Self {
            inner,
            cfg,
            external_envs,
            additional_limit: AdditionalLimit::default(),
            block_hash_record: BlockHashRecord::default(),
            bucket_multipliers: BucketMultipliers::default(),
            system_originated: false,
            prices_history: true,
            keyless_call: None,
            #[cfg(any(test, feature = "test-utils"))]
            neutral: false,
        }
    }

    /// Replaces the configuration.
    ///
    /// The fields the spec fixes are set from the spec, whatever `cfg` holds: the gas schedule,
    /// the EIP-8037 and EIP-2780 switches, the execution cap, the EIP-7708 switch, the
    /// system-call state-gas margin and the two code-size limits. Every other field (chain id,
    /// disabled checks, blob schedule) is taken from `cfg`.
    ///
    /// A context that ran the neutral configuration returns to the spec's.
    pub fn with_cfg(mut self, cfg: CfgEnv<MegaSpecId>) -> Self {
        let cfg = spec_cfg(cfg);
        self.inner = self.inner.with_cfg(op_cfg(&cfg));
        self.cfg = cfg;
        #[cfg(any(test, feature = "test-utils"))]
        {
            self.neutral = false;
        }
        self
    }

    /// Replaces the configuration with `cfg` as it is given, and turns off every dimension of
    /// pricing only `MegaETH` has: the neutral configuration.
    ///
    /// It is test tooling, behind the `test-utils` feature, for the execution-spec gate, which
    /// runs Ethereum's fixtures through Satin's machinery — its handler, frame lifecycle, Host
    /// and instruction table — priced as the fixture's own fork prices them, so that what is
    /// left to differ is what the machinery does rather than what `MegaETH` charges for.
    ///
    /// - Every field is taken from `cfg`, including the ones [`with_cfg`](Self::with_cfg) sets from
    ///   the spec: the gas schedule, the EIP-8037, EIP-2780 and EIP-7708 switches, the execution
    ///   cap and the code-size limits.
    /// - No transaction pays history gas, and none has its schedule swapped for its history
    ///   exemption: every transaction runs `cfg`'s schedule.
    /// - SALT pricing needs nothing here: without a SALT environment every bucket is minimal, and
    ///   the multiplier is one.
    ///
    /// The spec stays [`MegaSpecId::SATIN`], and with it the base spec the handler and the
    /// journal execute. The precompile set is the EVM's, not the context's: a caller that wants
    /// the fixture fork's replaces it on the [`MegaEvm`](crate::MegaEvm).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_neutral_cfg(mut self, cfg: CfgEnv<MegaSpecId>) -> Self {
        self.inner = self.inner.with_cfg(op_cfg(&cfg));
        self.cfg = cfg;
        self.neutral = true;
        self.prices_history = false;
        self
    }

    /// Whether the context runs the neutral configuration ([`with_neutral_cfg`]).
    ///
    /// [`with_neutral_cfg`]: Self::with_neutral_cfg
    #[cfg(any(test, feature = "test-utils"))]
    pub const fn is_neutral(&self) -> bool {
        self.neutral
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

    /// Enforces `limits` on every transaction from now on.
    pub fn with_tx_runtime_limits(mut self, limits: EvmTxRuntimeLimits) -> Self {
        self.additional_limit.set_limits(limits);
        self
    }

    /// The block hashes execution has read on this context.
    ///
    /// The record belongs to one block: block execution empties it when the block starts, so it
    /// never carries a hash an earlier block read.
    pub const fn block_hash_record(&self) -> &BlockHashRecord {
        &self.block_hash_record
    }

    /// Forgets the block hashes read so far.
    pub fn clear_block_hash_record(&mut self) {
        self.block_hash_record.clear();
    }

    /// The common execution layer's state for the running (or last) transaction.
    pub const fn additional_limit(&self) -> &AdditionalLimit {
        &self.additional_limit
    }

    /// The common execution layer's state, mutably. For tests and tools that drive the abort
    /// protocol directly.
    #[cfg(any(test, feature = "test-utils"))]
    pub const fn additional_limit_mut(&mut self) -> &mut AdditionalLimit {
        &mut self.additional_limit
    }

    /// The SALT bucket multiplier of the account `address`'s own state lives in: the capacity of
    /// its bucket in minimum buckets, never below one.
    ///
    /// Every account-scoped EIP-8037 state gas charge on `address` is scaled by it. The bucket is
    /// read from the transaction's [`SaltEnv`] the first time the transaction asks for it and
    /// from [`BucketMultipliers`] afterwards. A bucket the environment could not report, and one
    /// it reported below the minimum capacity a bucket can hold, both fail
    /// ([`BucketError`](crate::BucketError)).
    pub fn account_bucket_multiplier(
        &mut self,
        address: Address,
    ) -> Result<u64, BucketError<<ExtEnvs::SaltEnv as SaltEnv>::Error>> {
        self.bucket_multipliers.account(&self.external_envs.salt_env, address)
    }

    /// The SALT bucket multiplier of the slot `key` of `address`, which scales every slot-scoped
    /// EIP-8037 state gas charge on it. See
    /// [`account_bucket_multiplier`](Self::account_bucket_multiplier).
    pub fn slot_bucket_multiplier(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<u64, BucketError<<ExtEnvs::SaltEnv as SaltEnv>::Error>> {
        self.bucket_multipliers.slot(&self.external_envs.salt_env, address, key)
    }

    /// The SALT bucket multipliers the running (or last) transaction read.
    pub const fn bucket_multipliers(&self) -> &BucketMultipliers {
        &self.bucket_multipliers
    }

    /// Whether the running (or last) transaction is system-originated, and so prices every
    /// EIP-8037 state gas charge at the minimum bucket and is held to no per-transaction limit.
    /// See [`system::is_system_originated`].
    pub const fn is_system_originated(&self) -> bool {
        self.system_originated
    }

    /// Whether the running (or last) transaction pays history gas for the bytes it appends.
    ///
    /// Three kinds of transaction pay none: a deposit, a transaction the protocol itself produced
    /// ([`system::is_system_originated`]) and a system call. What they append is the chain
    /// carrying its own weight — a deposit the sequencer relays, the maintenance a system
    /// transaction performs, the pre-block calls the protocol makes — and there is no sender to
    /// charge for it.
    ///
    /// The two predicates are distinct and both are needed. A deposit is not system-originated:
    /// it carries a user's source hash and a user's caller, so it prices its state gas by the
    /// SALT bucket like any other transaction — it is exempt from history alone, because the
    /// bytes it appends were paid for on L1. A system transaction is both: it prices at the
    /// minimum bucket *and* pays no history.
    pub const fn prices_history(&self) -> bool {
        self.prices_history
    }

    /// Prepares the common execution layer for a new transaction. Every transaction entry point
    /// of [`MegaEvm`](crate::MegaEvm) calls it before it runs the handler.
    pub(crate) fn on_new_tx(&mut self) {
        let system_originated = system::is_system_originated(&self.inner.tx, MEGA_SYSTEM_ADDRESS);
        self.prepare(system_originated);
    }

    /// Prepares the context for a system call. Every system-call entry point of
    /// [`MegaEvm`](crate::MegaEvm) calls it instead of [`on_new_tx`](Self::on_new_tx).
    ///
    /// A system call is system-originated whatever caller it names: it is the protocol running,
    /// not a transaction anybody sent.
    pub(crate) fn on_new_system_call(&mut self) {
        self.prepare(true);
    }

    /// Prepares the common execution layer for a transaction or a system call that is, or is not,
    /// `system_originated`.
    ///
    /// The SALT bucket multipliers go with it: they are what one transaction read, so the next
    /// one reads its own.
    ///
    /// A system-originated transaction is exempt from every per-transaction limit — the data size,
    /// the KV count, the state gas, and the frame budgets of the first two — before anything is
    /// counted, its body included: the protocol's own work must not fail on a resource limit, as
    /// it pays no history gas for the same reason. What it uses is counted all the same. A user's
    /// deposit is not system-originated and is held to every one of them.
    fn prepare(&mut self, system_originated: bool) {
        self.additional_limit.reset();
        self.bucket_multipliers.reset();
        // A transaction that failed with an error left its `keylessDeploy` call unsettled.
        self.keyless_call = None;
        self.system_originated = system_originated;
        let exempt = self.inner.tx.tx_type() == DEPOSIT_TRANSACTION_TYPE || system_originated;
        self.set_history_exempt(exempt);
        if system_originated {
            self.additional_limit.exempt();
        }
        // The body is data size whether or not the transaction pays history for it. A deposit,
        // a system transaction and a system call are exempt from the charge, not from the count.
        self.additional_limit.record_tx_body(transaction_body_bytes(self.tx()));
    }

    /// Records whether the running transaction is exempt from history gas, and installs the
    /// schedule that matches.
    ///
    /// The engine's own history charges read [`prices_history`](Self::prices_history); the one
    /// charge revm makes itself reads the schedule, so an exempt transaction runs the schedule
    /// that prices a deposited byte at zero. Both configuration views move together, and only
    /// when the transaction's exemption differs from the one in place: the two tables are built
    /// once for the process, so the swap is a shared clone.
    ///
    /// The neutral configuration prices no history and keeps its own schedule.
    fn set_history_exempt(&mut self, exempt: bool) {
        #[cfg(any(test, feature = "test-utils"))]
        if self.neutral {
            self.prices_history = false;
            return;
        }
        self.prices_history = !exempt;
        let id = GasId::code_deposit_history_gas();
        let params = if exempt { satin_gas_params_history_exempt() } else { satin_gas_params() };
        if self.inner.cfg.gas_params.get(id) != params.get(id) {
            self.cfg.gas_params = params.clone();
            self.inner.cfg.gas_params = params;
        }
    }

    /// Consumes the context and returns the database, the configuration and the block.
    pub fn into_parts(self) -> (DB, CfgEnv<MegaSpecId>, BlockEnv) {
        let Context { block, journaled_state, .. } = self.inner;
        (journaled_state.database, self.cfg, block)
    }
}

/// Sets the configuration fields the spec fixes.
///
/// Satin runs the Satin gas schedule (see [`satin_gas_params`]) on its Karst base, with EIP-8037
/// state gas and the EIP-2780 intrinsic cost switched on and gas above the execution cap going to
/// the state-gas reservoir. It raises the code-size limits to `MegaETH`'s own.
///
/// Two switches are set off rather than left alone, because the base spec would turn them on and
/// Satin does not take them: EIP-7708 mints a transfer log for every value movement, which
/// `MegaETH` meters through its own write records instead, and the system-call reservoir margin
/// belongs to the system-call reservoir split.
fn spec_cfg(mut cfg: CfgEnv<MegaSpecId>) -> CfgEnv<MegaSpecId> {
    cfg.gas_params = satin_gas_params();
    cfg.enable_amsterdam_eip8037 = true;
    cfg.enable_amsterdam_eip2780 = true;
    cfg.tx_gas_limit_cap = Some(constants::TX_GAS_LIMIT_CAP);
    cfg.enable_amsterdam_eip7708 = false;
    cfg.system_call_state_gas_margin_in_reservoir = false;
    cfg.limit_contract_code_size = Some(constants::MAX_CONTRACT_SIZE);
    cfg.limit_contract_initcode_size = Some(constants::MAX_INITCODE_SIZE);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EthSpecId, SaltEnv, TestExternalEnvs};
    use alloy_primitives::U256;
    use core::convert::Infallible;
    use revm::{context_interface::cfg::GasParams, database::EmptyDB};

    /// Asserts both configuration views carry everything the spec fixes.
    fn assert_satin_cfg(ctx: &MegaContext<EmptyDB, impl ExternalEnvTypes>) {
        let (mega, op) = (ctx.mega_cfg(), ctx.cfg());
        assert_eq!(mega.spec, MegaSpecId::SATIN);
        assert_eq!(op.spec, OpSpecId::KARST);
        for cfg in [Fixed::of(mega), Fixed::of(op)] {
            assert!(cfg.eip8037, "EIP-8037 must be on");
            assert!(cfg.eip2780, "EIP-2780 must be on");
            assert_eq!(cfg.cap, Some(constants::TX_GAS_LIMIT_CAP), "execution cap");
            assert!(!cfg.eip7708, "EIP-7708 stays off");
            assert!(!cfg.margin, "the system-call reservoir margin stays off");
            assert_eq!(cfg.code_size, Some(constants::MAX_CONTRACT_SIZE), "contract size");
            assert_eq!(cfg.initcode_size, Some(constants::MAX_INITCODE_SIZE), "initcode size");
            assert_eq!(cfg.gas_params.table(), satin_gas_params().table(), "gas schedule");
        }
    }

    /// The configuration fields the spec fixes, read out of either view.
    struct Fixed<'a> {
        eip8037: bool,
        eip2780: bool,
        cap: Option<u64>,
        eip7708: bool,
        margin: bool,
        code_size: Option<usize>,
        initcode_size: Option<usize>,
        gas_params: &'a GasParams,
    }

    impl<'a> Fixed<'a> {
        fn of<SPEC>(cfg: &'a CfgEnv<SPEC>) -> Self {
            Self {
                eip8037: cfg.enable_amsterdam_eip8037,
                eip2780: cfg.enable_amsterdam_eip2780,
                cap: cfg.tx_gas_limit_cap,
                eip7708: cfg.enable_amsterdam_eip7708,
                margin: cfg.system_call_state_gas_margin_in_reservoir,
                code_size: cfg.limit_contract_code_size,
                initcode_size: cfg.limit_contract_initcode_size,
                gas_params: &cfg.gas_params,
            }
        }
    }

    /// A context built from a configuration `mutate` has set the other way.
    fn context_with(mutate: impl FnOnce(&mut CfgEnv<MegaSpecId>)) -> MegaContext<EmptyDB> {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
        mutate(&mut cfg);
        MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN).with_cfg(cfg)
    }

    #[test]
    fn test_new_context_carries_the_satin_configuration() {
        let ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN);
        assert_eq!(ctx.spec(), MegaSpecId::SATIN);
        assert_satin_cfg(&ctx);
    }

    /// A caller's configuration cannot switch off what the spec fixes; every other field is
    /// taken from it.
    #[test]
    fn test_with_cfg_keeps_the_whole_spec_configuration() {
        let ctx = context_with(|cfg| {
            cfg.chain_id = 4326;
            cfg.enable_amsterdam_eip8037 = false;
            cfg.enable_amsterdam_eip2780 = false;
            cfg.tx_gas_limit_cap = Some(1 << 24);
            cfg.enable_amsterdam_eip7708 = true;
            cfg.system_call_state_gas_margin_in_reservoir = true;
            cfg.limit_contract_code_size = Some(24 * 1024);
            cfg.limit_contract_initcode_size = Some(48 * 1024);
            cfg.gas_params = GasParams::new_spec(EthSpecId::AMSTERDAM);
        });

        assert_satin_cfg(&ctx);
        assert_eq!(ctx.mega_cfg().chain_id, 4326);
        assert_eq!(ctx.cfg().chain_id, 4326);
    }

    /// Each switch on its own: setting only that field the other way is overwritten, and the
    /// rest of the configuration is untouched by the attempt.
    #[test]
    fn test_eip8037_stays_on() {
        assert_satin_cfg(&context_with(|cfg| cfg.enable_amsterdam_eip8037 = false));
    }

    #[test]
    fn test_eip2780_stays_on() {
        assert_satin_cfg(&context_with(|cfg| cfg.enable_amsterdam_eip2780 = false));
    }

    #[test]
    fn test_eip7708_stays_off() {
        assert_satin_cfg(&context_with(|cfg| cfg.enable_amsterdam_eip7708 = true));
    }

    #[test]
    fn test_the_system_call_reservoir_margin_stays_off() {
        assert_satin_cfg(&context_with(|cfg| cfg.system_call_state_gas_margin_in_reservoir = true));
    }

    #[test]
    fn test_the_execution_cap_is_the_spec_s() {
        assert_satin_cfg(&context_with(|cfg| cfg.tx_gas_limit_cap = None));
        assert_satin_cfg(&context_with(|cfg| cfg.tx_gas_limit_cap = Some(u64::MAX)));
    }

    #[test]
    fn test_the_code_size_limits_are_the_spec_s() {
        assert_satin_cfg(&context_with(|cfg| cfg.limit_contract_code_size = Some(24 * 1024)));
        assert_satin_cfg(&context_with(|cfg| cfg.limit_contract_initcode_size = None));
    }

    /// The schedule is the Satin one whatever the caller passed, and it is the same table
    /// object every time: it is built once, not per configuration.
    #[test]
    fn test_the_gas_schedule_is_the_satin_one() {
        for spec in [EthSpecId::OSAKA, EthSpecId::AMSTERDAM, EthSpecId::PRAGUE] {
            let ctx = context_with(|cfg| cfg.gas_params = GasParams::new_spec(spec));
            assert_satin_cfg(&ctx);
            assert_ne!(ctx.mega_cfg().gas_params.table(), GasParams::new_spec(spec).table());
        }
    }

    /// The op-revm view is the `MegaETH` view keyed by the base spec, field for field.
    #[test]
    fn test_with_cfg_spec_consistency() {
        let ctx = context_with(|cfg| {
            cfg.chain_id = 6343;
            cfg.disable_nonce_check = true;
        });
        let (mega, op) = (ctx.mega_cfg(), ctx.cfg());

        assert_eq!(op.spec, mega.spec.into_op_spec());
        assert_eq!(op.chain_id, 6343);
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

    /// The multipliers a transaction read belong to it: the next transaction reads the capacity
    /// again, so a bucket that grew between two transactions is priced at what it holds now.
    #[test]
    fn test_the_bucket_multipliers_are_forgotten_between_transactions() {
        const ACCOUNT: alloy_primitives::Address =
            alloy_primitives::address!("00000000000000000000000000000000000000a1");
        let bucket = <TestExternalEnvs as SaltEnv>::bucket_id_for_account(ACCOUNT);
        let env = TestExternalEnvs::<Infallible>::new()
            .with_bucket_capacity(bucket, crate::MIN_BUCKET_SIZE as u64 * 4);
        let mut ctx = MegaContext::new_with_external_envs(
            EmptyDB::default(),
            MegaSpecId::SATIN,
            ExternalEnvs::from(env.clone()),
        );

        assert_eq!(ctx.account_bucket_multiplier(ACCOUNT), Ok(4));
        assert_eq!(ctx.account_bucket_multiplier(ACCOUNT), Ok(4));
        assert_eq!(env.bucket_queries(bucket), 1, "the second charge came from the cache");

        ctx.on_new_tx();
        assert_eq!(ctx.bucket_multipliers().cached_buckets().len(), 0);
        assert_eq!(ctx.account_bucket_multiplier(ACCOUNT), Ok(4));
        assert_eq!(env.bucket_queries(bucket), 2, "the next transaction read it again");
    }

    /// Two contexts may read one SALT environment — a node builds an EVM per transaction over
    /// the block's environments — and each keeps the multipliers it read to itself.
    #[test]
    fn test_contexts_sharing_a_salt_environment_keep_their_own_multipliers() {
        const ONE: alloy_primitives::Address =
            alloy_primitives::address!("0000000000000000000000000000000000000b01");
        const OTHER: alloy_primitives::Address =
            alloy_primitives::address!("0000000000000000000000000000000000000b02");
        let env = TestExternalEnvs::<Infallible>::new();
        let context = || {
            MegaContext::new_with_external_envs(
                EmptyDB::default(),
                MegaSpecId::SATIN,
                ExternalEnvs::from(env.clone()),
            )
        };
        let (mut first, mut second) = (context(), context());

        first.account_bucket_multiplier(ONE).unwrap();
        second.account_bucket_multiplier(OTHER).unwrap();

        assert_eq!(
            first.bucket_multipliers().cached_buckets().collect::<Vec<_>>(),
            vec![<TestExternalEnvs as SaltEnv>::bucket_id_for_account(ONE)],
        );
        assert_eq!(
            second.bucket_multipliers().cached_buckets().collect::<Vec<_>>(),
            vec![<TestExternalEnvs as SaltEnv>::bucket_id_for_account(OTHER)],
        );
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
        assert_satin_cfg(&ctx);
    }

    /// An Osaka configuration, every field the spec fixes set the other way from Satin's.
    fn osaka_cfg() -> CfgEnv<MegaSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::SATIN);
        cfg.chain_id = 1;
        cfg.gas_params = GasParams::new_spec(EthSpecId::OSAKA);
        cfg.enable_amsterdam_eip8037 = false;
        cfg.enable_amsterdam_eip2780 = false;
        cfg.tx_gas_limit_cap = None;
        cfg.enable_amsterdam_eip7708 = true;
        cfg.limit_contract_code_size = None;
        cfg.limit_contract_initcode_size = None;
        cfg
    }

    /// Asserts both configuration views carry `cfg` field for field.
    fn assert_cfg_is(ctx: &MegaContext<EmptyDB, impl ExternalEnvTypes>, cfg: &CfgEnv<MegaSpecId>) {
        assert_eq!(ctx.mega_cfg(), cfg);
        assert_eq!(ctx.cfg(), &op_cfg(cfg));
        assert_eq!(ctx.cfg().gas_params.table(), cfg.gas_params.table());
    }

    fn call_from(caller: Address) -> MegaTransaction {
        alloy_op_evm::OpTx(crate::test_utils::op_transaction(revm::context::TxEnv {
            caller,
            ..Default::default()
        }))
    }

    /// The neutral configuration takes every field from the caller — the ones the spec fixes
    /// included — and prices no history.
    #[test]
    fn test_neutral_cfg_takes_every_field_as_given() {
        let cfg = osaka_cfg();
        let ctx =
            MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN).with_neutral_cfg(cfg.clone());

        assert!(ctx.is_neutral());
        assert!(!ctx.prices_history());
        assert_eq!(ctx.spec(), MegaSpecId::SATIN);
        assert_eq!(ctx.cfg().spec, OpSpecId::KARST);
        assert_cfg_is(&ctx, &cfg);
    }

    /// A new transaction neither prices history nor swaps the schedule for its history
    /// exemption, whichever kind of transaction it is: a user's, a system call, a deposit.
    #[test]
    fn test_neutral_cfg_survives_every_kind_of_transaction() {
        let cfg = osaka_cfg();
        let mut ctx =
            MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN).with_neutral_cfg(cfg.clone());

        ctx.set_tx(call_from(Address::repeat_byte(0x11)));
        ctx.on_new_tx();
        assert!(!ctx.prices_history(), "a user's transaction pays no history");
        assert_cfg_is(&ctx, &cfg);

        ctx.on_new_system_call();
        assert!(!ctx.prices_history());
        assert_cfg_is(&ctx, &cfg);

        let mut deposit = call_from(Address::repeat_byte(0x22));
        deposit.0.deposit.source_hash = revm::primitives::B256::repeat_byte(1);
        deposit.0.base.tx_type = DEPOSIT_TRANSACTION_TYPE;
        ctx.set_tx(deposit);
        ctx.on_new_tx();
        assert!(!ctx.prices_history());
        assert_cfg_is(&ctx, &cfg);

        // And back to a user's transaction, after an exempt one would have swapped the schedule.
        ctx.set_tx(call_from(Address::repeat_byte(0x11)));
        ctx.on_new_tx();
        assert!(!ctx.prices_history());
        assert_cfg_is(&ctx, &cfg);
    }

    /// [`MegaContext::with_cfg`] returns a neutral context to the spec's configuration, and its
    /// transactions pay history again.
    #[test]
    fn test_with_cfg_leaves_the_neutral_configuration() {
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN)
            .with_neutral_cfg(osaka_cfg())
            .with_cfg(osaka_cfg());

        assert!(!ctx.is_neutral());
        assert_satin_cfg(&ctx);
        ctx.set_tx(call_from(Address::repeat_byte(0x11)));
        ctx.on_new_tx();
        assert!(ctx.prices_history());
        assert_satin_cfg(&ctx);
    }

    /// A context is not neutral unless it is asked to be.
    #[test]
    fn test_a_new_context_is_not_neutral() {
        let mut ctx = MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN);
        assert!(!ctx.is_neutral());
        ctx.set_tx(call_from(Address::repeat_byte(0x11)));
        ctx.on_new_tx();
        assert!(ctx.prices_history());
    }
}
