//! # `MegaETH` EVM Context
//!
//! This module provides the core context implementation for the `MegaETH` EVM.
//! The [`Context`] struct wraps the underlying `OpStack` context and provides
//! additional MegaETH-specific functionality including gas cost oracles,
//! additional limits, and block environment access tracking.
//!
//! ## Key Features
//!
//! - **Gas Cost Oracle**: Tracks and manages gas costs during transaction execution
//! - **Additional Limits**: Enforces data and KV update limits beyond standard EVM limits
//! - **Block Environment Access Tracking**: Monitors which block environment data is accessed
//! - **Spec Management**: Handles different `MegaETH` specification versions

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{rc::Rc, vec::Vec};

use alloy_evm::Database;
use alloy_primitives::Address;
use core::cell::RefCell;
use delegate::delegate;
use op_revm::{DefaultOp, L1BlockInfo, OpSpecId};

/// The revm context underlying [`MegaContext`].
///
/// Same shape as `op_revm::OpContext<DB>` except for the transaction type: `MegaETH` uses the
/// `alloy-op-evm` `OpTx` newtype so the foreign `IntoTxEnv` / `FromRecoveredTx` impls are
/// available (see [`crate::MegaTransaction`]).
pub type MegaInnerContext<DB> = revm::Context<
    revm::context::BlockEnv,
    crate::MegaTransaction,
    revm::context::CfgEnv<OpSpecId>,
    DB,
    revm::Journal<DB>,
    L1BlockInfo,
>;
use revm::{
    context::{BlockEnv, CfgEnv, ContextSetters, ContextTr, LocalContext},
    context_interface::{
        cfg::{GasId, GasParams},
        context::ContextError,
    },
    database::EmptyDB,
    primitives::hardfork::SpecId as EthSpecId,
    Journal,
};

use crate::{
    constants, evm::host::CallTargetLoadPhase, is_system_originated, AdditionalLimit, BucketId,
    DynamicGasCost, EmptyExternalEnv, EvmTxRuntimeLimits, ExternalEnvTypes, ExternalEnvs,
    MegaSpecId, TxRuntimeLimit, VolatileDataAccess, VolatileDataAccessTracker,
    VolatileDataAccessType,
};

/// `MegaETH` EVM context type. This struct wraps [`OpContext`] and implements the [`ContextTr`]
/// trait to be used as the context for the [`crate::Evm`].
#[derive(Debug, derive_more::Deref, derive_more::DerefMut)]
pub struct MegaContext<DB: Database, ExtEnvs: ExternalEnvTypes> {
    /// The inner context.
    #[deref]
    #[deref_mut]
    pub(crate) inner: MegaInnerContext<DB>,
    /// The `MegaETH` spec id. The inner context contains the `OpSpecId`.
    /// The `OpSpec` in the `inner` context should be the corresponding [`OpSpecId`] for the
    /// [`SpecId`].
    pub(crate) spec: MegaSpecId,

    /// Whether to disable the post-transaction reward to beneficiary.
    pub(crate) disable_beneficiary: bool,

    /// Additional limits for the EVM.
    pub additional_limit: Rc<RefCell<AdditionalLimit>>,

    /// Shared SALT environment handle.
    pub(crate) salt_env: Rc<ExtEnvs::SaltEnv>,

    /// Calculator for dynamic gas costs during transaction execution.
    pub dynamic_storage_gas_cost: Rc<RefCell<DynamicGasCost<Rc<ExtEnvs::SaltEnv>>>>,

    /// The oracle environment.
    pub oracle_env: Rc<RefCell<ExtEnvs::OracleEnv>>,

    /* Internal state variables */
    /// Tracker for volatile data access (block environment, beneficiary, oracle)
    /// and volatile data access disable (`MegaAccessControl` system contract).
    pub volatile_data_tracker: Rc<RefCell<VolatileDataAccessTracker>>,

    /// Phase of the CALL-family target resolution currently in flight, `Idle` outside one.
    ///
    /// Set by the CALL-family instruction handlers through
    /// [`HostExt::begin_call_target_resolution`](crate::HostExt::begin_call_target_resolution) so
    /// the host can tell a CALL's raw stack operand apart from its EIP-7702 delegate hop when
    /// marking beneficiary access. Purely per-instruction state: it is always `Idle` between
    /// opcodes, so it is not carried over when a context is rebuilt.
    ///
    /// A plain field, not a `Cell`: the phase is written from `&mut self` on both sides of revm's
    /// CALL body, and wrapping it in interior mutability instead costs ~2% on the CALL-heavy
    /// benchmarks — the writes then act as optimization barriers around that body.
    pub(crate) call_target_load_phase: CallTargetLoadPhase,

    /// Set to `true` when this context is itself a sandbox execution.
    ///
    /// Suppresses sandbox interception (preventing recursive sandboxing) and signals other
    /// Mega hooks to defer to outer-frame accounting (e.g., the Rex5+ deposit-caller
    /// materialization charge in `validate`, which is paid by the outer keyless-deploy call
    /// before the sandbox runs).
    pub(crate) inside_sandbox: Rc<RefCell<bool>>,

    /// The system address for the current block.
    /// Pre-REX5: always `MEGA_SYSTEM_ADDRESS` (the legacy hardcoded constant).
    /// REX5+: resolved from `SequencerRegistry` storage in `apply_pre_execution_changes`.
    pub(crate) system_address: Address,
}

impl Default for MegaContext<EmptyDB, EmptyExternalEnv> {
    fn default() -> Self {
        Self::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
    }
}

/* Constructors */
impl<DB: Database> MegaContext<DB, EmptyExternalEnv> {
    /// Creates a new `MegaContext` with [`EmptyExternalEnv`].
    ///
    /// This constructor initializes a new `MegaETH` EVM context with default settings.
    /// For the `MINI_REX` specification, it automatically configures appropriate
    /// contract size and initcode size limits.
    ///
    /// # Arguments
    ///
    /// * `db` - The database implementation to use for state storage
    /// * `spec` - The `MegaETH` specification version to use
    ///
    /// # Returns
    ///
    /// Returns a new `MegaContext` instance with default configuration.
    pub fn new(db: DB, spec: MegaSpecId) -> Self {
        // `OpContext::default()` starts with block number 0, so the parent block number is also 0.
        let salt_env = Rc::new(EmptyExternalEnv);
        Self::new_with_shared_ext_envs(db, spec, salt_env, Rc::new(RefCell::new(EmptyExternalEnv)))
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Test/bench-only public wrapper over [`new_with_shared_ext_envs`] so a
    /// bench can build a context over a configurable external environment
    /// (e.g. `TestExternalEnvs` with crowded buckets or oracle storage).
    pub fn new_with_ext_envs(
        db: DB,
        spec: MegaSpecId,
        salt_env: Rc<ExtEnvs::SaltEnv>,
        oracle_env: Rc<RefCell<ExtEnvs::OracleEnv>>,
    ) -> Self {
        Self::new_with_shared_ext_envs(db, spec, salt_env, oracle_env)
    }
}

impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Creates a new `MegaContext` with shared external environment references.
    ///
    /// Unlike [`MegaContext::new`] which uses [`EmptyExternalEnv`], this constructor accepts
    /// existing `Rc` references to share a parent context's salt env and oracle env.
    /// This ensures the sandbox uses the same dynamic gas pricing as the parent.
    ///
    /// # Arguments
    ///
    /// * `db` - The database implementation to use for state storage
    /// * `spec` - The `MegaETH` specification version to use
    /// * `salt_env` - Shared salt environment from the parent context
    /// * `oracle_env` - Shared oracle environment from the parent context
    ///
    /// # Returns
    ///
    /// Returns a new `MegaContext` instance sharing the parent's external environments while
    /// keeping a fresh dynamic gas cost cache local to the new context.
    pub(crate) fn new_with_shared_ext_envs(
        db: DB,
        spec: MegaSpecId,
        salt_env: Rc<ExtEnvs::SaltEnv>,
        oracle_env: Rc<RefCell<ExtEnvs::OracleEnv>>,
    ) -> Self {
        // `Context::op()` builds the transaction as `OpTransaction<TxEnv>`; `MegaETH` uses the
        // `OpTx` newtype (see `crate::MegaTransaction`), so retype it here.
        let mut inner: MegaInnerContext<DB> = revm::Context::op()
            .with_tx(crate::MegaTransaction::default())
            .with_db(db)
            .with_cfg(CfgEnv::new_with_spec(spec.into_op_spec()));

        // revm 40 flipped `CfgEnv::tx_chain_id_check` default from `false` (revm 27) to `true`.
        // With the new default, non-legacy txs with `chain_id: None` (and many unit-test
        // TxEnvBuilder::default() shapes) fail validation with MissingChainId.
        // Pin revm-27 semantics: the whole chain-id gate is off unless a caller later
        // enables it via `with_cfg` / factory `EvmEnv`. Note: with the flag off, a
        // mismatched `Some(wrong_id)` is also accepted — same as revm 27.
        inner.cfg.tx_chain_id_check = false;

        if spec.is_enabled(MegaSpecId::MINI_REX) {
            inner.cfg.limit_contract_code_size = Some(constants::mini_rex::MAX_CONTRACT_SIZE);
            inner.cfg.limit_contract_initcode_size = Some(constants::mini_rex::MAX_INITCODE_SIZE);
        }

        let tx_limits = EvmTxRuntimeLimits::from_spec(spec);
        Self {
            spec,
            disable_beneficiary: false,
            additional_limit: Rc::new(RefCell::new(AdditionalLimit::new(spec, tx_limits))),
            salt_env: Rc::clone(&salt_env),
            dynamic_storage_gas_cost: Rc::new(RefCell::new(DynamicGasCost::new(
                spec,
                salt_env,
                inner.block.number.to::<u64>().saturating_sub(1),
            ))),
            oracle_env,
            volatile_data_tracker: Rc::new(RefCell::new(VolatileDataAccessTracker::new(
                tx_limits.block_env_access_compute_gas_limit,
                tx_limits.oracle_access_compute_gas_limit,
            ))),
            call_target_load_phase: CallTargetLoadPhase::Idle,
            inside_sandbox: Rc::new(RefCell::new(false)),
            system_address: crate::MEGA_SYSTEM_ADDRESS,
            inner,
        }
    }
}

impl<DB: Database, ExtEnvTypes: ExternalEnvTypes> MegaContext<DB, ExtEnvTypes> {
    /// Creates a new `Context` from an existing `OpContext`.
    ///
    /// This constructor is useful when you already have a configured `OpContext`
    /// and want to wrap it with MegaETH-specific functionality. The specification
    /// in the provided context must match the `spec` parameter.
    ///
    /// # Arguments
    ///
    /// * `context` - The existing `OpStack` context to wrap
    /// * `spec` - The `MegaETH` specification version (must match context spec)
    /// * `external_envs` - The external environments for gas cost calculations
    ///
    /// # Returns
    ///
    /// Returns a new `Context` instance wrapping the provided context.
    ///
    /// # Panics
    ///
    /// If the provided context's `gas_params` is not the schedule its own `cfg.spec` defines —
    /// the gas schedule belongs to the spec, see [`assert_spec_owned_gas_schedule`]. A
    /// configuration still on its builder's default spec is fine: the spec relabel below
    /// re-derives the schedule for `spec`.
    #[deprecated(note = "Use `MegaContext::new` instead")]
    pub fn new_with_context(
        context: MegaInnerContext<DB>,
        spec: MegaSpecId,
        external_envs: ExternalEnvs<ExtEnvTypes>,
    ) -> Self {
        let mut inner = context;

        // Checked against the spec the caller's configuration itself carries, before the relabel
        // below re-derives the schedule: a configuration still on its builder's default op-spec
        // (`revm::Context::op()` starts at `BEDROCK`) is a supported input here, but its schedule
        // must be that spec's, not one the caller rewrote.
        assert_spec_owned_gas_schedule(&inner.cfg);

        // Spec in context must keep the same with parameter `spec`.
        // revm 40 keeps per-spec `GasParams` in `CfgEnv`, so update both together —
        // bare `cfg.spec = ...` would leave the caller's (e.g. BEDROCK) params in place.
        // A context already sitting on the target op-spec has nothing to re-derive, and its
        // gas schedule — possibly one the caller installed — is left alone.
        let op_spec = spec.into_op_spec();
        if inner.cfg.spec != op_spec {
            inner.cfg.set_spec_and_mainnet_gas_params(op_spec);
        }

        // Same revm-27 pin as `new_with_shared_ext_envs`, applied unconditionally: this is a
        // compatibility entry point, and the chain-id gate revm 40 turned on by default is not
        // part of the pre-upgrade semantics it reproduces. A caller that wants the gate builds
        // the context through [`MegaContext::with_cfg_unpinned`] instead.
        inner.cfg.tx_chain_id_check = false;

        // For the `MINI_REX` spec, we override the contract size and initcode size limits if they
        // not set in the given `OpContext`.
        if spec.is_enabled(MegaSpecId::MINI_REX) {
            if inner.cfg.limit_contract_code_size.is_none() {
                inner.cfg.limit_contract_code_size = Some(constants::mini_rex::MAX_CONTRACT_SIZE);
            }
            if inner.cfg.limit_contract_initcode_size.is_none() {
                inner.cfg.limit_contract_initcode_size =
                    Some(constants::mini_rex::MAX_INITCODE_SIZE);
            }
        }

        let tx_limits = EvmTxRuntimeLimits::from_spec(spec);
        let salt_env = Rc::new(external_envs.salt_env);
        Self {
            spec,
            disable_beneficiary: false,
            additional_limit: Rc::new(RefCell::new(AdditionalLimit::new(spec, tx_limits))),
            salt_env: Rc::clone(&salt_env),
            dynamic_storage_gas_cost: Rc::new(RefCell::new(DynamicGasCost::new(
                spec,
                salt_env,
                inner.block.number.to::<u64>().saturating_sub(1),
            ))),
            oracle_env: Rc::new(RefCell::new(external_envs.oracle_env)),
            volatile_data_tracker: Rc::new(RefCell::new(VolatileDataAccessTracker::new(
                tx_limits.block_env_access_compute_gas_limit,
                tx_limits.oracle_access_compute_gas_limit,
            ))),
            call_target_load_phase: CallTargetLoadPhase::Idle,
            inside_sandbox: Rc::new(RefCell::new(false)),
            system_address: crate::MEGA_SYSTEM_ADDRESS,
            inner,
        }
    }

    /// Sets the [`Database`] used by the EVM.
    ///
    /// This method allows changing the underlying database implementation
    /// while preserving all other context configuration.
    ///
    /// # Arguments
    ///
    /// * `db` - The new database implementation
    ///
    /// # Returns
    ///
    /// Returns a new `Context` with the updated database type.
    pub fn with_db<ODB: Database>(self, db: ODB) -> MegaContext<ODB, ExtEnvTypes> {
        MegaContext {
            inner: self.inner.with_db(db),
            spec: self.spec,
            disable_beneficiary: self.disable_beneficiary,
            additional_limit: self.additional_limit,
            salt_env: self.salt_env,
            dynamic_storage_gas_cost: self.dynamic_storage_gas_cost,
            oracle_env: self.oracle_env,
            volatile_data_tracker: self.volatile_data_tracker,
            call_target_load_phase: CallTargetLoadPhase::Idle,
            inside_sandbox: self.inside_sandbox,
            system_address: self.system_address,
        }
    }

    /// Sets the [`Transaction`] to be executed by the EVM.
    ///
    /// This method configures the transaction to be executed and automatically
    /// resets internal state for the new transaction.
    ///
    /// # Arguments
    ///
    /// * `tx` - The transaction to execute
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_tx(mut self, tx: crate::MegaTransaction) -> Self {
        self.inner = self.inner.with_tx(tx);
        self
    }

    /// Sets the [`BlockEnv`] for the EVM.
    ///
    /// This method configures the block environment and automatically
    /// resets internal state for the new block.
    ///
    /// # Arguments
    ///
    /// * `block` - The block environment configuration
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_block(mut self, block: BlockEnv) -> Self {
        self.inner = self.inner.with_block(block);
        // Reset internal state for new block
        self.on_new_block();
        self
    }

    /// Sets the [`CfgEnv`] for the EVM.
    ///
    /// This method configures the EVM environment settings. For the `MINI_REX`
    /// specification, it automatically applies appropriate contract size limits
    /// if they are not already set in the configuration.
    ///
    /// A spec change rebuilds the additional-limit trackers from the new spec and the
    /// already-configured runtime limits so spec-latched state stays aligned. An unchanged
    /// spec leaves the existing tracker in place.
    ///
    /// # `tx_chain_id_check` is pinned off
    ///
    /// revm 40 flipped the `CfgEnv::tx_chain_id_check` default from `false` to `true` — a gate
    /// that rejects transactions with no chain id at all, and transactions whose chain id
    /// differs from the configuration's. Every `MegaETH` spec froze its semantics with the gate
    /// off, and an embedder that never touched the field would inherit the flipped default
    /// silently, so this entry point pins the flag to `false` unconditionally — including over a
    /// configuration that explicitly set it. An embedder that wants the gate opts in through
    /// [`with_cfg_unpinned`](Self::with_cfg_unpinned), where the field is taken as provided.
    ///
    /// # The gas schedule is defined by the spec
    ///
    /// `cfg.gas_params` must be exactly the schedule `cfg.spec` defines — the table
    /// `CfgEnv::new_with_spec(spec)` and `cfg.set_spec_and_mainnet_gas_params(spec)` install.
    /// `MegaETH`'s gas schedule is a property of the spec rather than of the configuration, so
    /// there is no supported way to override it, and a configuration that deviates is rejected
    /// here with a panic rather than run. See [`assert_spec_owned_gas_schedule`] for why the
    /// deviation cannot be tolerated and where else the same check runs.
    ///
    /// # Panics
    ///
    /// If `cfg.gas_params` is not the schedule `cfg.spec` defines.
    ///
    /// # Arguments
    ///
    /// * `cfg` - The configuration environment
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_cfg(self, cfg: CfgEnv<MegaSpecId>) -> Self {
        self.apply_cfg(cfg, CfgIntent::Pinned)
    }

    /// Sets the [`CfgEnv`] for the EVM, taking `tx_chain_id_check` as provided.
    ///
    /// Same as [`with_cfg`](Self::with_cfg) except that the revm-27 compatibility pin described
    /// there does not run: `tx_chain_id_check` reaches the EVM exactly as the caller set it.
    /// This is the entry point for an embedder that enables the chain-id gate on purpose —
    /// accepting that historical `MegaETH` semantics ran without it.
    ///
    /// Skipping that pin does not skip `MegaETH`'s own consensus pins:
    ///
    /// - The gas schedule is defined by the spec: a `gas_params` that deviates from what `cfg.spec`
    ///   defines panics here exactly as it does in [`with_cfg`](Self::with_cfg) — see
    ///   [`assert_spec_owned_gas_schedule`].
    /// - EIP-8037 (Amsterdam state gas) is forced off before every transaction runs, wherever the
    ///   configuration came from — see [`force_amsterdam_eip8037_off`].
    /// - Under `MINI_REX` and later, the contract size and initcode size limits fill in when the
    ///   configuration leaves them unset.
    ///
    /// # Panics
    ///
    /// If `cfg.gas_params` is not the schedule `cfg.spec` defines.
    ///
    /// # Arguments
    ///
    /// * `cfg` - The configuration environment
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_cfg_unpinned(self, cfg: CfgEnv<MegaSpecId>) -> Self {
        self.apply_cfg(cfg, CfgIntent::Declared)
    }

    /// Shared body of [`with_cfg`](Self::with_cfg) and
    /// [`with_cfg_unpinned`](Self::with_cfg_unpinned): both adopt the caller's configuration the
    /// same way, and differ only in whether `tx_chain_id_check` is pinned to the revm-27 `false`
    /// or taken as the caller provided it.
    ///
    /// A spec change rebuilds [`AdditionalLimit`] from the new spec and the already-configured
    /// runtime limits, so spec-latched tracker state stays aligned with [`Self::spec`]. Limits
    /// already set by [`with_tx_runtime_limits`](Self::with_tx_runtime_limits) are kept; they are
    /// not replaced by the new spec's defaults. An unchanged spec leaves the existing tracker in
    /// place.
    ///
    /// # Panics
    ///
    /// If `cfg.gas_params` is not the schedule `cfg.spec` defines — the gas schedule belongs to
    /// the spec, see [`assert_spec_owned_gas_schedule`].
    fn apply_cfg(mut self, cfg: CfgEnv<MegaSpecId>, intent: CfgIntent) -> Self {
        assert_spec_owned_gas_schedule(&cfg);
        let new_spec = cfg.spec;
        let spec_changed = new_spec != self.spec;
        self.spec = new_spec;
        self.inner = self.inner.with_cfg(cfg.into_op_cfg());
        if intent == CfgIntent::Pinned {
            self.inner.cfg.tx_chain_id_check = false;
        }
        if self.spec.is_enabled(MegaSpecId::MINI_REX) {
            if self.inner.cfg.limit_contract_code_size.is_none() {
                self.inner.cfg.limit_contract_code_size =
                    Some(constants::mini_rex::MAX_CONTRACT_SIZE);
            }
            if self.inner.cfg.limit_contract_initcode_size.is_none() {
                self.inner.cfg.limit_contract_initcode_size =
                    Some(constants::mini_rex::MAX_INITCODE_SIZE);
            }
        }
        if spec_changed {
            let limits = self.additional_limit.borrow().limits;
            self.additional_limit = Rc::new(RefCell::new(AdditionalLimit::new(self.spec, limits)));
        }
        self
    }

    /// Sets the external environments for the EVM.
    ///
    /// This method updates the external environments used for gas cost calculations,
    /// including the salt environment and oracle environment. When setting new
    /// external environments, the dynamic gas cost calculator and oracle environment
    /// are reinitialized with the new configurations.
    ///
    /// # Arguments
    ///
    /// * `external_envs` - The new external environments configuration
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_external_envs<NewExtEnvTypes: ExternalEnvTypes>(
        self,
        external_envs: ExternalEnvs<NewExtEnvTypes>,
    ) -> MegaContext<DB, NewExtEnvTypes> {
        let parent_block_number = self.inner.block.number.to::<u64>().saturating_sub(1);
        let spec = self.spec;
        let salt_env = Rc::new(external_envs.salt_env);
        MegaContext {
            inner: self.inner,
            spec,
            disable_beneficiary: self.disable_beneficiary,
            additional_limit: self.additional_limit,
            salt_env: Rc::clone(&salt_env),
            dynamic_storage_gas_cost: Rc::new(RefCell::new(DynamicGasCost::new(
                spec,
                salt_env,
                parent_block_number,
            ))),
            oracle_env: Rc::new(RefCell::new(external_envs.oracle_env)),
            volatile_data_tracker: self.volatile_data_tracker,
            call_target_load_phase: CallTargetLoadPhase::Idle,
            inside_sandbox: self.inside_sandbox,
            system_address: self.system_address,
        }
    }

    /// Sets the Op Stack's [`L1BlockInfo`] for the EVM.
    ///
    /// This method configures the L1 block information used by the `OpStack`
    /// for cross-layer communication and state management.
    ///
    /// # Arguments
    ///
    /// * `chain` - The L1 block information
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_chain(mut self, chain: L1BlockInfo) -> Self {
        self.inner = self.inner.with_chain(chain);
        self
    }

    /// Sets the transaction limits for the EVM.
    pub fn with_tx_runtime_limits(mut self, tx_limits: EvmTxRuntimeLimits) -> Self {
        self.additional_limit = Rc::new(RefCell::new(AdditionalLimit::new(self.spec, tx_limits)));
        self.volatile_data_tracker = Rc::new(RefCell::new(VolatileDataAccessTracker::new(
            tx_limits.block_env_access_compute_gas_limit,
            tx_limits.oracle_access_compute_gas_limit,
        )));
        self
    }
}

/* Getters */
impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Gets the `MegaETH` specification ID.
    ///
    /// Returns the specification version currently configured for this context.
    ///
    /// # Returns
    ///
    /// Returns the [`SpecId`] representing the current `MegaETH` specification.
    pub fn mega_spec(&self) -> MegaSpecId {
        self.spec
    }

    /// Gets the system address for the current block.
    ///
    /// Pre-REX5: always `MEGA_SYSTEM_ADDRESS`.
    /// REX5+: resolved from `SequencerRegistry` storage in `apply_pre_execution_changes`.
    pub fn system_address(&self) -> Address {
        self.system_address
    }

    /// Sets the system address for the current block.
    pub(crate) fn set_system_address(&mut self, address: Address) {
        self.system_address = address;
    }

    /// Returns whether this context is itself a sandbox execution.
    ///
    /// When `true`, sandbox interception (e.g., keyless deploy) is suppressed to prevent
    /// recursive sandboxing, and Mega hooks defer to outer-frame accounting (e.g., the
    /// Rex5+ deposit-caller materialization charge in `validate`).
    #[inline]
    pub fn is_inside_sandbox(&self) -> bool {
        *self.inside_sandbox.borrow()
    }

    /// Sets whether this context is itself a sandbox execution.
    #[inline]
    pub(crate) fn set_inside_sandbox(&self, value: bool) {
        *self.inside_sandbox.borrow_mut() = value;
    }

    /// Builder method to mark this context as itself a sandbox execution.
    ///
    /// Used when constructing a sandbox's own context to prevent recursive interception.
    #[inline]
    pub fn with_inside_sandbox(self, value: bool) -> Self {
        self.set_inside_sandbox(value);
        self
    }

    /// Gets the current total data size generated from transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the current total data size in bytes generated so far. The data size is reset at the
    /// beginning of each transaction.
    pub fn generated_data_size(&self) -> u64 {
        self.additional_limit.borrow().data_size.tx_usage()
    }

    /// Gets the current total number of key-value updates performed during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the current total number of KV operations performed so far. The count is reset at
    /// the beginning of each transaction.
    pub fn kv_update_count(&self) -> u64 {
        self.additional_limit.borrow().kv_update.tx_usage()
    }

    /// Gets the bucket IDs used during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the bucket IDs used during transaction execution.
    pub fn accessed_bucket_ids(&self) -> Vec<BucketId> {
        self.dynamic_storage_gas_cost.borrow().get_bucket_ids()
    }

    /// Consumes the context and converts it into the inner `OpContext`.
    ///
    /// This method extracts the underlying `OpStack` context, discarding
    /// all MegaETH-specific state and configuration.
    ///
    /// # Returns
    ///
    /// Returns the inner context.
    pub fn into_inner(self) -> MegaInnerContext<DB> {
        self.inner
    }
}

/* Block Environment Access Tracking */
impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Returns the bitmap of block environment data accessed during transaction execution.
    ///
    /// This method provides information about which block environment fields
    /// have been accessed during the current transaction, which is useful for
    /// optimization and analysis purposes.
    ///
    /// # Returns
    ///
    /// Returns a [`VolatileDataAccess`] bitmap indicating accessed fields.
    pub fn get_block_env_accesses(&self) -> VolatileDataAccess {
        self.volatile_data_tracker.borrow().get_block_env_accesses()
    }

    /// Resets the volatile data access tracker for new transactions.
    ///
    /// This method clears the volatile data access tracker, preparing the context for a new
    /// transaction.
    pub fn reset_volatile_data_access(&mut self) {
        self.volatile_data_tracker.borrow_mut().reset();
    }

    /// Marks that a specific type of block environment has been accessed.
    ///
    /// This internal method is used to track which block environment fields
    /// are being accessed during transaction execution.
    ///
    /// # Arguments
    ///
    /// * `access_type` - The type of block environment access to record
    pub(crate) fn mark_block_env_accessed(&self, access_type: VolatileDataAccessType) {
        self.volatile_data_tracker.borrow_mut().mark_block_env_accessed(access_type);
    }
}

/* Beneficiary Access Tracking */
impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Disables the beneficiary reward.
    pub fn disable_beneficiary(&mut self) {
        self.disable_beneficiary = true;
    }

    /// Check if address is beneficiary and mark access if so.
    /// Returns true if beneficiary was accessed.
    pub(crate) fn check_and_mark_beneficiary_balance_access(&self, address: &Address) -> bool {
        if self.inner.block.beneficiary == *address {
            self.volatile_data_tracker.borrow_mut().mark_beneficiary_balance_accessed();
            true
        } else {
            false
        }
    }

    /// Check if the transaction caller or recipient is the beneficiary
    pub(crate) fn check_tx_beneficiary_access(&self) {
        let tx = &self.inner.tx;
        let beneficiary = self.inner.block.beneficiary;

        // Check if caller is beneficiary
        if tx.base.caller == beneficiary {
            self.volatile_data_tracker.borrow_mut().mark_beneficiary_balance_accessed();
        }

        // Check if recipient is beneficiary (for calls)
        if let revm::primitives::TxKind::Call(recipient) = tx.base.kind {
            if recipient == beneficiary {
                self.volatile_data_tracker.borrow_mut().mark_beneficiary_balance_accessed();
            }
        }
    }
}

/* Hooks */
impl<DB: Database, ExtEnvs: ExternalEnvTypes> MegaContext<DB, ExtEnvs> {
    /// Resets the internal state for a new block.
    ///
    /// This method is called when transitioning to a new block and updates
    /// the dynamic gas cost calculator and additional limits accordingly.
    pub(crate) fn on_new_block(&self) {
        // The dynamic gas cost calculator is only enabled when the `MINI_REX` spec is enabled.
        if self.spec.is_enabled(MegaSpecId::MINI_REX) {
            self.dynamic_storage_gas_cost.borrow_mut().on_new_block(&self.inner.block);
        }
    }

    /// Resets the internal state for a new transaction.
    ///
    /// This method is called when starting a new transaction and resets
    /// block environment access tracking and additional limits.
    ///
    /// If transaction-only intrinsic resource usage exceeds a configured limit,
    /// `before_tx_start()` sets `has_exceeded_limit` so that the subsequent
    /// `frame_result_if_exceeding_limit()` or `before_frame_init()` call produces a normal
    /// execution failure on the standard additional-limit path.
    ///
    /// DB-dependent pre-frame usage may still be recorded later during pre-execution.
    pub(crate) fn on_new_tx(&mut self) {
        assert_cfg_spec_matches_context_spec(self.spec, self.inner.cfg.spec);
        assert_spec_owned_gas_schedule(&self.inner.cfg);
        force_amsterdam_eip8037_off(&mut self.inner.cfg);

        self.reset_volatile_data_access();

        // The additional-limit lifecycle (reset → intrinsic accounting) exists only for MINI_REX+.
        if self.spec.is_enabled(MegaSpecId::MINI_REX) {
            self.additional_limit.borrow_mut().reset();
            self.additional_limit.borrow_mut().before_tx_start(&self.inner.tx);
        }

        // REX6+: exempt system-originated transactions (see `crate::is_system_originated`) from
        // MegaETH per-tx resource metering.
        if self.spec.is_enabled(MegaSpecId::REX6) &&
            is_system_originated(&self.inner.tx, self.system_address)
        {
            self.additional_limit.borrow_mut().mark_exempt();
        }

        // Mark beneficiary access AFTER additional_limit.reset() so that the volatile
        // tracker marking from check_tx_beneficiary_access can be synchronized into
        // additional_limit below, rather than being cleared by the reset.
        //
        // Gated to REX4: pre-REX4 specs never had eager beneficiary detention at TX start.
        // Changing pre-REX4 behavior would alter historical replay results.
        self.check_tx_beneficiary_access();
        if self.spec.is_enabled(MegaSpecId::REX4) {
            let compute_gas_limit = self.volatile_data_tracker.borrow().get_compute_gas_limit();
            if let Some(limit) = compute_gas_limit {
                self.additional_limit.borrow_mut().set_compute_gas_limit(limit);
            }
        }
    }
}

/// Implementation of the `ContextTr` trait for `Context`.
///
/// This implementation delegates most methods to the inner `OpContext` while
/// maintaining the MegaETH-specific functionality. The trait provides access
/// to the core EVM context components like transaction, block, configuration,
/// database, journal, and chain information.
impl<DB: Database, ExtEnvs: ExternalEnvTypes> ContextTr for MegaContext<DB, ExtEnvs> {
    type Block = BlockEnv;
    type Tx = crate::MegaTransaction;
    type Cfg = CfgEnv<OpSpecId>;
    type Db = DB;
    type Journal = Journal<DB>;
    type Chain = L1BlockInfo;
    type Local = LocalContext;

    delegate! {
        to self.inner {
            fn tx(&self) -> &Self::Tx;
            fn block(&self) -> &Self::Block;
            fn cfg(&self) -> &Self::Cfg;
            fn journal(&self) -> &Self::Journal;
            fn journal_mut(&mut self) -> &mut Self::Journal;
            fn journal_ref(&self) -> &Self::Journal;
            fn db(&self) -> &Self::Db;
            fn db_mut(&mut self) -> &mut Self::Db;
            fn chain(&self) -> &Self::Chain;
            fn chain_mut(&mut self) -> &mut Self::Chain;
            fn local(&self) -> &Self::Local;
            fn local_mut(&mut self) -> &mut Self::Local;
            fn error(&mut self) -> &mut Result<(), ContextError<<Self::Db as revm::Database>::Error>>;
            fn tx_journal_mut(&mut self) -> (&Self::Tx, &mut Self::Journal);
            fn tx_local_mut(&mut self) -> (&Self::Tx, &mut Self::Local);
            fn all(
                &self,
            ) -> (
                &Self::Block,
                &Self::Tx,
                &Self::Cfg,
                &Self::Db,
                &Self::Journal,
                &Self::Chain,
                &Self::Local,
            );
            fn all_mut(
                &mut self,
            ) -> (
                &Self::Block,
                &Self::Tx,
                &Self::Cfg,
                &mut Self::Journal,
                &mut Self::Chain,
                &mut Self::Local,
            );
        }
    }
}

/// Implementation of the `ContextSetters` trait for `Context`.
///
/// This implementation provides methods to update the context state, with
/// special handling for transaction updates to reset internal state.
impl<DB: Database, ExtEnvs: ExternalEnvTypes> ContextSetters for MegaContext<DB, ExtEnvs> {
    delegate! {
        to self.inner {
            fn set_block(&mut self, block: Self::Block);
            fn set_tx(&mut self, tx: Self::Tx);
        }
    }
}

/// How a `MegaContext` entry point treats the caller's `tx_chain_id_check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CfgIntent {
    /// Pin `tx_chain_id_check` to the revm-27 `false`, whatever the configuration carries:
    /// every `MegaETH` spec froze its semantics with the gate off, and revm 40 flipped the
    /// upstream default to `true` underneath existing embedders.
    Pinned,
    /// Take `tx_chain_id_check` exactly as the caller provided it.
    Declared,
}

/// Forces EIP-8037 (Amsterdam state gas) off on the configuration a transaction is about to run
/// with, whatever a caller put there.
///
/// `CfgEnv::enable_amsterdam_eip8037` is a free-standing switch in revm 40: the sites that split a
/// charge into regular gas plus state gas — `SSTORE` on a fresh slot, the `CREATE` account and
/// code-deposit charge, the new-account cost of a value-bearing `CALL`, the EIP-7702 authority
/// refund, and the transaction's initial gas / reservoir split — read the flag alone and do not
/// also require an Amsterdam spec. Left alone, it would turn the state-gas split on underneath a
/// frozen `MegaSpecId` and change what that spec charges.
///
/// `MegaETH`'s gas accounting is built on there being no such split: per-opcode compute gas is
/// recorded from the gas the opcode actually consumed, storage gas and the storage call stipend
/// are charged and drawn on top of that same single dimension, and precompile gas is normalized
/// back onto the caller's one forwarded budget. Nothing in `MegaETH` charges against revm's
/// state-gas reservoir, so a charge split off into it would escape `MegaETH`'s meters entirely.
///
/// Enabling EIP-8037 is therefore a consensus change, and belongs to a `MegaSpecId` gate rather
/// than to a `CfgEnv` field a caller can set. It is also one `MegaETH` has no reason to want:
/// storage gas already charges the same operations, scaled by SALT bucket occupancy rather than at
/// a flat rate, and the state-growth tracker already bounds net new accounts and slots per
/// transaction. Turning the split on would charge those operations a second time, and the second
/// charge would land in a reservoir none of the four resource dimensions reads.
///
/// Called from [`MegaContext::on_new_tx`], which every execution path passes through — the
/// transaction path via `MegaHandler::before_run`, system calls via `Handler::run_system_call` —
/// and which runs before anything reads the flag. Forcing the value at the point of use rather
/// than pinning it at each entry point is what makes the guarantee independent of how a
/// configuration arrived: revm's own carriers propagate a set flag (`with_spec_and_gas_params`
/// moves it across the spec relabel, `set_spec_and_mainnet_gas_params` ORs it back on for an
/// Amsterdam-or-later spec), so a per-entry-point pin has to be re-derived every time one of them
/// is rewired or a new path appears, and covers nothing a caller does to a live context.
pub(crate) fn force_amsterdam_eip8037_off<SPEC>(cfg: &mut CfgEnv<SPEC>) {
    cfg.enable_amsterdam_eip8037 = false;
}

/// Panics unless `cfg` carries exactly the gas schedule its own `spec` defines.
///
/// `MegaETH`'s gas schedule belongs to the spec, not to the configuration. revm 40 turned the
/// price of every operation into a `CfgEnv::gas_params` table an embedder can rewrite, and
/// `MegaETH`'s own accounting does not read that table everywhere revm does: several recording
/// sites carry the schedule's value as a constant (the `CALL_STIPEND` a value-transferring call
/// mints, the pre-`REX7` per-byte code-deposit rate, the mainnet table the keyless-deploy
/// preflight estimates intrinsic gas from). Under a rewritten table those sites would book
/// something other than what revm charged, which silently breaks the conservation law the
/// reported compute total is derived from — and, for a table that prices the call stipend below
/// revm's, takes the 98/100 forwarding cap's subtraction below zero.
///
/// Rather than teach every such site to read the table, the schedule is pinned: a configuration
/// whose `gas_params` deviates from its spec's is rejected outright, at the loudest available
/// signal, so no transaction ever runs on one. This mirrors [`crate::HardforkParams::validate`],
/// which panics at chain-config load time instead of letting a bad value surface at the first
/// block that uses it. The check is unconditional across specs: it governs the configuration
/// domain, which no historical block covers, so gating it on a spec would only narrow the
/// guarantee without preserving anything.
///
/// Deviation is the only thing rejected — the rest of `CfgEnv` stays the embedder's, including
/// switches that change what a transaction costs by other means (`disable_eip7623`,
/// `limit_contract_code_size`, the blob caps).
///
/// Called from the three points a configuration can reach the EVM through: both `with_cfg`
/// entry points (via [`MegaContext::apply_cfg`]), the deprecated
/// [`MegaContext::new_with_context`], and [`MegaContext::on_new_tx`] — the last one being the
/// point of use, which also covers a configuration mutated in place after the context was built
/// (reachable through the mutable deref, e.g. `ctx.modify_cfg`). The comparison is a pointer
/// compare in the common case: both tables come from the same per-spec `OnceLock` inside revm.
pub(crate) fn assert_spec_owned_gas_schedule<SPEC: Into<EthSpecId> + Clone + core::fmt::Debug>(
    cfg: &CfgEnv<SPEC>,
) {
    let expected = GasParams::new_spec(cfg.spec.clone().into());
    if cfg.gas_params != expected {
        panic_gas_schedule_mismatch(&cfg.spec, &cfg.gas_params, &expected);
    }
}

/// Reports the first entry on which a configuration's gas schedule deviates from its spec's, and
/// panics.
///
/// Split out of [`assert_spec_owned_gas_schedule`] and taking the spec as `&dyn Debug` so the
/// formatting and the table walk stay out of the caller's inlined fast path, and are emitted once
/// rather than per instantiation.
#[cold]
#[inline(never)]
fn panic_gas_schedule_mismatch(
    spec: &dyn core::fmt::Debug,
    actual: &GasParams,
    expected: &GasParams,
) -> ! {
    let mismatch = actual
        .table()
        .iter()
        .zip(expected.table().iter())
        .enumerate()
        .find(|(_, (got, want))| got != want);
    let (id, got, want) = match mismatch {
        Some((index, (got, want))) => (GasId::new(index as u8), *got, *want),
        // `!=` on `GasParams` compares exactly these tables, so a mismatch always has an entry.
        None => unreachable!("gas params differ but no table entry does"),
    };
    panic!(
        "gas params differ from the spec-defined schedule for {spec:?}: `{}` is {got}, the \
         schedule defines {want}. MegaETH's gas schedule is defined by the spec and cannot be \
         overridden through `CfgEnv::gas_params`; build the configuration with \
         `CfgEnv::new_with_spec(spec)` or `cfg.set_spec_and_mainnet_gas_params(spec)` and leave \
         the schedule alone.",
        id.name(),
    )
}

/// Panics unless `cfg_spec` is the op-spec `spec` maps to.
///
/// A [`MegaContext`] carries its spec twice: as the [`MegaSpecId`] on the context, and as the
/// [`OpSpecId`] that spec maps to inside `CfgEnv`. Execution reads both, from different halves.
/// The `MegaSpecId` selects the instruction table, the precompile set and the
/// [`AdditionalLimit`](crate::AdditionalLimit) trackers, all baked when the EVM is built from the
/// context; `CfgEnv::spec` is what revm's own spec gating reads while a transaction runs. The two
/// must name the same fork, and every supported way of setting a spec writes both from a single
/// value — the constructors derive the configuration from the `MegaSpecId` they are given, and
/// [`MegaContext::apply_cfg`] takes the context's `MegaSpecId` from the configuration it adopts.
///
/// They come apart only through the mutable deref to the inner context (`ctx.modify_cfg`, or a
/// `&mut CfgEnv` taken directly), which reaches the configuration without passing through either.
/// Rewriting `cfg.spec` there leaves the baked halves on the context's `MegaSpecId` while revm
/// prices the transaction under the written one — one transaction executing under two forks at
/// once, with `MegaETH`'s wrappers, precompiles and resource limits taken from a fork revm is not
/// pricing. Writing the schedule along with the spec (`set_spec_and_mainnet_gas_params`) leaves
/// [`assert_spec_owned_gas_schedule`] satisfied, because the schedule then does match the spec
/// that was written, so that check alone does not catch this.
///
/// Called from [`MegaContext::on_new_tx`], and checked there rather than at each entry point for
/// the same reason [`force_amsterdam_eip8037_off`] is applied there: the point of use covers a
/// configuration mutated in place after the context was built, which a per-entry-point check
/// cannot. It runs ahead of [`assert_spec_owned_gas_schedule`] so that a bare `cfg.spec` write —
/// which both checks reject — is reported as the desync it is, rather than as a schedule that
/// could be repaired by installing the written spec's table.
pub(crate) fn assert_cfg_spec_matches_context_spec(spec: MegaSpecId, cfg_spec: OpSpecId) {
    if cfg_spec != spec.into_op_spec() {
        panic_context_spec_mismatch(spec, cfg_spec);
    }
}

/// Reports a context whose two spec fields name different forks, and panics.
///
/// Split out of [`assert_cfg_spec_matches_context_spec`] and marked cold for the same reason
/// [`panic_gas_schedule_mismatch`] is: the formatting stays out of the caller's inlined fast path.
#[cold]
#[inline(never)]
fn panic_context_spec_mismatch(spec: MegaSpecId, cfg_spec: OpSpecId) -> ! {
    panic!(
        "the configuration's spec is {cfg_spec:?}, but this context executes {spec:?}, whose \
         op-spec is {:?}. MegaETH's spec is not a `CfgEnv` field a caller can rewrite on a live \
         context: it also selects the instruction table, the precompiles and the resource-limit \
         trackers, which are baked when the EVM is built, so a rewritten `CfgEnv::spec` would \
         run one transaction under two specs at once. Change the spec by adopting a whole \
         configuration through `MegaContext::with_cfg` (or `with_cfg_unpinned`), which sets both.",
        spec.into_op_spec(),
    )
}

/// A convenient trait to convert a `CfgEnv<OpSpecId>` into a `CfgEnv<SpecId>`.
///
/// This trait provides a conversion method for `OpStack` configuration environments
/// to `MegaETH` configuration environments, preserving all configuration fields
/// while changing the specification type.
pub trait IntoMegaethCfgEnv {
    /// Converts to `CfgEnv<MegaethSpecId>`.
    fn into_megaeth_cfg(self, spec: MegaSpecId) -> CfgEnv<MegaSpecId>;
}

/// A convenient trait to convert a `CfgEnv<SpecId>` into a `CfgEnv<OpSpecId>`.
///
/// This trait provides a conversion method for `MegaETH` configuration environments
/// to `OpStack` configuration environments, preserving all configuration fields
/// while changing the specification type.
pub trait IntoOpCfgEnv {
    /// Converts to `CfgEnv<OpSpecId>`.
    fn into_op_cfg(self) -> CfgEnv<OpSpecId>;
}

/// Implementation of `IntoOpCfgEnv` for `CfgEnv<SpecId>`.
///
/// This implementation converts a `MegaETH` configuration environment to an
/// `OpStack` configuration environment by copying all relevant fields.
impl IntoOpCfgEnv for CfgEnv<MegaSpecId> {
    /// Converts to `CfgEnv<OpSpecId>`.
    ///
    /// This method relabels the specification type and carries every other field of the
    /// caller's configuration — the gas schedule included — into the `OpStack` shape. It is a
    /// relabel and nothing more: the fields `MegaETH` does not let a caller choose are settled
    /// where they are read, not here (EIP-8037 by [`force_amsterdam_eip8037_off`], the gas
    /// schedule by [`assert_spec_owned_gas_schedule`]).
    ///
    /// # Returns
    ///
    /// Returns a new `CfgEnv<OpSpecId>` with all fields moved from `self`.
    fn into_op_cfg(self) -> CfgEnv<OpSpecId> {
        let op_spec = OpSpecId::from(self.spec);
        // Carry the schedule rather than re-deriving it: the relabel must not be the thing that
        // silently repairs a deviating table, and re-deriving would be a no-op on a conforming
        // one anyway — every `MegaSpecId` and its op-spec map to the same eth hardfork.
        let gas_params = self.gas_params.clone();
        // `with_spec_and_gas_params` is revm's own whole-struct carrier: it moves every field
        // (including the ones behind revm cargo features) into the new spec type, so fields
        // added upstream come along instead of being silently reset to their defaults.
        self.with_spec_and_gas_params(op_spec, gas_params)
    }
}

/// Implementation of `IntoMegaethCfgEnv` for `CfgEnv<OpSpecId>`.
///
/// This implementation converts an `OpStack` configuration environment to a
/// `MegaETH` configuration environment by copying all relevant fields.
impl IntoMegaethCfgEnv for CfgEnv<OpSpecId> {
    /// Converts to `CfgEnv<SpecId>`.
    ///
    /// The inverse of [`IntoOpCfgEnv::into_op_cfg`]: it relabels the specification type with the
    /// given `spec` and carries every other field — the gas schedule included — unchanged, so a
    /// configuration handed to the EVM reads back as the caller wrote it. Reading back a
    /// configuration the EVM has run at least one transaction with also reads back EIP-8037 off,
    /// because [`force_amsterdam_eip8037_off`] settles it on the context itself.
    ///
    /// # Arguments
    ///
    /// * `spec` - The `MegaETH` specification ID to use in the new configuration
    ///
    /// # Returns
    ///
    /// Returns a new `CfgEnv<SpecId>` with all fields moved from `self`.
    fn into_megaeth_cfg(self, spec: MegaSpecId) -> CfgEnv<MegaSpecId> {
        let gas_params = self.gas_params.clone();
        self.with_spec_and_gas_params(spec, gas_params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::{address, Address, Bytes, U256};
    use revm::{
        context::{tx::TxEnvBuilder, CfgEnv},
        context_interface::cfg::{GasId, GasParams},
        database::EmptyDB,
        primitives::hardfork::SpecId,
    };

    use crate::{test_utils::MemoryDatabase, MegaTransactionNew as _, TestExternalEnvs};

    /// A gas schedule an embedder could try to install: the spec table with one entry moved off
    /// its mainnet value. Distinct from every `GasParams::new_spec(..)` table, so a conversion
    /// that re-derives the schedule from the spec instead of carrying it shows up as a diff — and
    /// so an entry point that admits it instead of rejecting it does too.
    fn custom_gas_params() -> GasParams {
        let mut gas_params = GasParams::new_spec(SpecId::PRAGUE);
        gas_params.override_gas([(GasId::tx_token_cost(), 40)]);
        assert_ne!(gas_params, GasParams::new_spec(SpecId::PRAGUE));
        gas_params
    }

    /// The schedule `spec` defines — the one and only schedule an entry point admits.
    fn spec_gas_params(spec: MegaSpecId) -> GasParams {
        GasParams::new_spec(SpecId::from(spec))
    }

    /// An untouched configuration for `spec`, schedule included.
    fn spec_cfg(spec: MegaSpecId) -> CfgEnv<MegaSpecId> {
        CfgEnv::new_with_spec(spec)
    }

    /// Every `MegaSpecId`, so the schedule pin is asserted across the whole progression rather
    /// than on whichever spec a test happened to pick.
    const ALL_SPECS: [MegaSpecId; 10] = [
        MegaSpecId::EQUIVALENCE,
        MegaSpecId::MINI_REX,
        MegaSpecId::REX,
        MegaSpecId::REX1,
        MegaSpecId::REX2,
        MegaSpecId::REX3,
        MegaSpecId::REX4,
        MegaSpecId::REX5,
        MegaSpecId::REX6,
        MegaSpecId::REX7,
    ];

    /// A [`CfgEnv`] with every field moved off its revm default, so any field the conversion
    /// drops instead of carrying collapses back to a default and fails an equality assert.
    fn fully_customized_cfg(spec: MegaSpecId) -> CfgEnv<MegaSpecId> {
        let mut cfg = CfgEnv::new_with_spec(spec);
        cfg.gas_params = custom_gas_params();
        cfg.chain_id = 6342;
        cfg.tx_chain_id_check = false;
        cfg.limit_contract_code_size = Some(0x1234);
        cfg.limit_contract_initcode_size = Some(0x2468);
        cfg.disable_nonce_check = true;
        cfg.max_blobs_per_tx = Some(3);
        cfg.blob_base_fee_update_fraction = Some(7);
        cfg.tx_gas_limit_cap = Some(12_345);
        cfg.memory_limit = 1 << 20;
        cfg.disable_balance_check = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_eip3541 = true;
        cfg.disable_eip3607 = true;
        cfg.disable_eip7623 = true;
        cfg.disable_base_fee = true;
        cfg.enable_amsterdam_eip8037 = true;
        cfg.amsterdam_eip7708_disabled = true;
        cfg.amsterdam_eip7708_delayed_burn_disabled = true;
        cfg
    }

    /// [`fully_customized_cfg`] with the one field a caller does not own put back to the schedule
    /// its spec defines, so the configuration reaches an entry point instead of being rejected by
    /// it. Everything else is still off its revm default, which is what the carry-every-field
    /// asserts need.
    fn admissible_customized_cfg(spec: MegaSpecId) -> CfgEnv<MegaSpecId> {
        let mut cfg = fully_customized_cfg(spec);
        cfg.gas_params = spec_gas_params(spec);
        cfg
    }

    /// The `MegaSpecId` <-> `OpSpecId` config conversions relabel the spec type and nothing else:
    /// every other field — the gas schedule and the revm 40 switches included — belongs to the
    /// caller and must survive both legs, EIP-8037 included. What `MegaETH` does not let a caller
    /// choose is settled where it is read (see `test_eip8037_is_forced_off_before_execution`), not
    /// by a conversion quietly rewriting a field.
    #[test]
    fn test_cfg_conversion_carries_every_field_both_ways() {
        let cfg = fully_customized_cfg(MegaSpecId::REX6);

        let op_cfg = cfg.clone().into_op_cfg();

        assert_eq!(op_cfg.spec, MegaSpecId::REX6.into_op_spec());
        assert_eq!(op_cfg.gas_params, cfg.gas_params, "custom gas schedule must survive");
        assert!(op_cfg.disable_eip7623);
        assert!(
            op_cfg.enable_amsterdam_eip8037,
            "a relabel carries every field, this one included"
        );
        assert!(op_cfg.amsterdam_eip7708_disabled);
        assert!(op_cfg.amsterdam_eip7708_delayed_burn_disabled);

        // Whole-struct equality on the round trip: with every input field off its default, a
        // field dropped on either leg reverts to a default and trips this assert.
        assert_eq!(op_cfg.into_megaeth_cfg(MegaSpecId::REX6), cfg);
    }

    /// `with_cfg` is where an embedder's `CfgEnv` lands. It must reach the inner revm config
    /// intact — only the `MegaETH` pins (spec, `MINI_REX` size limits) may differ.
    #[test]
    fn test_with_cfg_carries_embedder_switches_and_the_spec_schedule() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX6);
        cfg.chain_id = 6342;
        cfg.disable_eip7623 = true;

        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg(cfg);

        assert_eq!(context.inner.cfg.gas_params, spec_gas_params(MegaSpecId::REX6));
        assert!(context.inner.cfg.disable_eip7623);
        assert_eq!(context.inner.cfg.chain_id, 6342);
    }

    /// The gas schedule is not an embedder's to set. A configuration carrying anything other than
    /// the schedule its spec defines is rejected at the entry point rather than run, because
    /// `MegaETH` records several of the schedule's values from constants rather than from the
    /// table and would otherwise book charges revm never made.
    #[test]
    #[should_panic(expected = "gas params differ from the spec-defined schedule")]
    fn test_with_cfg_rejects_a_schedule_off_the_spec_table() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX6);
        cfg.gas_params = custom_gas_params();

        let _ = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg(cfg);
    }

    /// The `call_stipend` entry specifically: `MegaETH` books the stipend revm mints into a
    /// value-transferring call's child frame from `gas::CALL_STIPEND`, and the 98/100 forwarding
    /// cap subtracts the same constant back out of the child's budget. A schedule that priced the
    /// stipend differently would desync both.
    #[test]
    #[should_panic(expected = "gas params differ from the spec-defined schedule")]
    fn test_with_cfg_rejects_an_overridden_call_stipend() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX7);
        cfg.gas_params.override_gas([(GasId::call_stipend(), 0)]);

        let _ = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg(cfg);
    }

    /// The `code_deposit_cost` entry specifically: revm debits a successful `CREATE` the
    /// schedule's per-byte rate, and pre-`REX7` specs record that charge from the constant.
    #[test]
    #[should_panic(expected = "gas params differ from the spec-defined schedule")]
    fn test_with_cfg_rejects_an_overridden_code_deposit_cost() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX7);
        cfg.gas_params.override_gas([(GasId::code_deposit_cost(), 201)]);

        let _ = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg(cfg);
    }

    /// The rejection reports the spec whose schedule was expected, which entry deviated and both
    /// values, so an embedder that hits it can act on the message rather than bisect its
    /// configuration.
    #[test]
    #[should_panic(
        expected = "schedule for REX7: `code_deposit_cost` is 201, the schedule defines 200"
    )]
    fn test_schedule_rejection_names_the_spec_the_entry_and_both_values() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX7);
        cfg.gas_params.override_gas([(GasId::code_deposit_cost(), 201)]);

        let _ = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg(cfg);
    }

    /// Every spec's own default schedule is admissible on every entry point, and reaches the EVM
    /// as written. The pin is a rejection of deviation, not a narrowing of which specs run.
    #[test]
    fn test_every_spec_default_schedule_is_admissible() {
        for spec in ALL_SPECS {
            let expected = spec_gas_params(spec);

            let pinned = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
                .with_cfg(spec_cfg(spec));
            assert_eq!(pinned.inner.cfg.gas_params, expected, "with_cfg on {spec:?}");

            let unpinned = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
                .with_cfg_unpinned(spec_cfg(spec));
            assert_eq!(unpinned.inner.cfg.gas_params, expected, "with_cfg_unpinned on {spec:?}");
        }
    }

    /// The constructors that build their own configuration install the spec's schedule, so the
    /// contexts that never see a caller's `CfgEnv` satisfy the pin by construction. This is the
    /// path the keyless-deploy sandbox takes: it builds its inner context from
    /// [`MegaContext::new_with_shared_ext_envs`] rather than inheriting the outer configuration,
    /// so no override could reach it even if one had been admitted outside.
    #[test]
    fn test_constructors_install_the_spec_schedule_on_every_spec() {
        for spec in ALL_SPECS {
            let expected = spec_gas_params(spec);

            let plain = MegaContext::new(EmptyDB::default(), spec);
            assert_eq!(plain.inner.cfg.gas_params, expected, "MegaContext::new on {spec:?}");

            let sandbox_shaped = MegaContext::<_, EmptyExternalEnv>::new_with_shared_ext_envs(
                EmptyDB::default(),
                spec,
                Rc::new(EmptyExternalEnv),
                Rc::new(RefCell::new(EmptyExternalEnv)),
            );
            assert_eq!(
                sandbox_shaped.inner.cfg.gas_params, expected,
                "the sandbox's constructor on {spec:?}",
            );
        }
    }

    /// Migrating a live context between specs leaves it on the schedule the new spec defines, in
    /// both directions — the entry point adopts the whole configuration, so the schedule cannot
    /// be left behind from the spec the context previously ran.
    #[test]
    fn test_spec_migration_keeps_the_schedule_on_the_active_spec() {
        let mut context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE);

        for spec in [MegaSpecId::REX5, MegaSpecId::REX7, MegaSpecId::REX5, MegaSpecId::MINI_REX] {
            context = context.with_cfg(spec_cfg(spec));

            assert_eq!(context.mega_spec(), spec);
            assert_eq!(context.inner.cfg.gas_params, spec_gas_params(spec), "after {spec:?}");
            // And the migrated context is one a transaction can run on: `on_new_tx` re-checks the
            // schedule at the point of use.
            context.on_new_tx();
        }
    }

    /// The entry points are not the only place the schedule is checked: it is re-checked at the
    /// point of use, so a configuration mutated in place after the context was built — reachable
    /// through the mutable deref, e.g. `ctx.modify_cfg` — cannot execute either.
    #[test]
    #[should_panic(expected = "gas params differ from the spec-defined schedule")]
    fn test_on_new_tx_rejects_a_schedule_mutated_after_construction() {
        let mut context = MegaContext::new(EmptyDB::default(), MegaSpecId::REX7);
        context.modify_cfg(|cfg| cfg.gas_params = custom_gas_params());

        context.on_new_tx();
    }

    /// The spec is pinned the same way the schedule is, and for a shape the schedule pin cannot
    /// see: `cfg.spec` and `cfg.gas_params` rewritten together on a live context are
    /// self-consistent, so the schedule matches the spec that was written. What no longer matches
    /// is the context — the instruction table, the precompiles and the resource-limit trackers
    /// stay on the `MegaSpecId` the EVM was built from, so the transaction would run under two
    /// specs at once.
    #[test]
    #[should_panic(expected = "the configuration's spec is BEDROCK, but this context executes")]
    fn test_on_new_tx_rejects_a_spec_and_schedule_mutated_together() {
        let mut context = MegaContext::new(EmptyDB::default(), MegaSpecId::REX7);
        context.modify_cfg(|cfg| cfg.set_spec_and_mainnet_gas_params(OpSpecId::BEDROCK));
        // The pair is self-consistent: the schedule pin, on its own, admits this configuration.
        assert_eq!(
            context.inner.cfg.gas_params,
            GasParams::new_spec(SpecId::from(OpSpecId::BEDROCK)),
        );

        context.on_new_tx();
    }

    /// A bare `cfg.spec` write desyncs the context the same way, and is rejected by the spec pin
    /// rather than by the schedule pin it also trips: the schedule is a consequence here, and its
    /// message would send an embedder to `set_spec_and_mainnet_gas_params`, which repairs the
    /// schedule and leaves the desync — the shape the test above covers.
    #[test]
    #[should_panic(expected = "the configuration's spec is BEDROCK, but this context executes")]
    fn test_on_new_tx_rejects_a_bare_spec_write() {
        let mut context = MegaContext::new(EmptyDB::default(), MegaSpecId::REX7);
        context.modify_cfg(|cfg| cfg.spec = OpSpecId::BEDROCK);

        context.on_new_tx();
    }

    /// The rejection names both specs and the entry point that sets them together, so an embedder
    /// that hits it can act on the message.
    #[test]
    #[should_panic(
        expected = "this context executes REX7, whose op-spec is ISTHMUS. MegaETH's spec is not"
    )]
    fn test_spec_rejection_names_both_specs_and_the_supported_entry_point() {
        let mut context = MegaContext::new(EmptyDB::default(), MegaSpecId::REX7);
        context.modify_cfg(|cfg| cfg.set_spec_and_mainnet_gas_params(OpSpecId::BEDROCK));

        context.on_new_tx();
    }

    /// The spec pin rejects desync, not any particular spec: every spec, reached by every
    /// supported path — the constructors, both `with_cfg` entry points, and the deprecated
    /// `new_with_context` — passes it at the point of use.
    #[allow(deprecated)]
    #[test]
    fn test_every_spec_passes_the_spec_pin_on_every_construction_path() {
        for spec in ALL_SPECS {
            MegaContext::new(EmptyDB::default(), spec).on_new_tx();

            MegaContext::<_, EmptyExternalEnv>::new_with_shared_ext_envs(
                EmptyDB::default(),
                spec,
                Rc::new(EmptyExternalEnv),
                Rc::new(RefCell::new(EmptyExternalEnv)),
            )
            .on_new_tx();

            MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
                .with_cfg(spec_cfg(spec))
                .on_new_tx();

            MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
                .with_cfg_unpinned(spec_cfg(spec))
                .on_new_tx();

            // The deprecated constructor's input is a configuration on revm's default op-spec,
            // which it relabels — so this also covers the relabel leaving the two in sync.
            let inner: MegaInnerContext<EmptyDB> = revm::Context::op()
                .with_tx(crate::MegaTransaction::default())
                .with_db(EmptyDB::default());
            MegaContext::new_with_context(inner, spec, ExternalEnvs::<EmptyExternalEnv>::default())
                .on_new_tx();
        }
    }

    /// The pin is unconditional: a configured cfg — here a blob schedule plus an explicitly
    /// enabled check — still comes out with the gate off. `with_cfg` is the compatibility entry
    /// point; enabling the gate requires `with_cfg_unpinned`.
    #[test]
    fn test_with_cfg_pins_chain_id_check_off_for_configured_cfg() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX5);
        cfg.max_blobs_per_tx = Some(6);
        cfg.tx_chain_id_check = true;

        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg(cfg);

        assert!(
            !context.inner.cfg.tx_chain_id_check,
            "with_cfg must pin the chain-id gate off even when the cfg enabled it"
        );
    }

    /// An untouched `CfgEnv::new_with_spec` config carries revm 40's flipped default, which the
    /// caller never asked for: `with_cfg` re-pins revm 27's `false`.
    #[test]
    fn test_with_cfg_repins_untouched_default_cfg() {
        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
            .with_cfg(CfgEnv::new_with_spec(MegaSpecId::REX5));

        assert!(
            !context.inner.cfg.tx_chain_id_check,
            "an untouched revm-40 default cfg must be re-pinned to revm-27 semantics"
        );
    }

    /// `with_cfg_unpinned` is the opt-in for the gate: the `tx_chain_id_check` a configuration
    /// carries — here revm 40's default `true` on an untouched `CfgEnv::new_with_spec` — reaches
    /// the EVM as provided.
    #[test]
    fn test_with_cfg_unpinned_keeps_chain_id_check_on_default_shape() {
        // Exactly the shape `with_cfg` re-pins: chain 1, check on, nothing else touched.
        let cfg = CfgEnv::new_with_spec(MegaSpecId::REX5);
        assert_eq!(cfg.chain_id, 1);
        assert!(cfg.tx_chain_id_check);

        let context =
            MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg_unpinned(cfg);

        assert!(
            context.inner.cfg.tx_chain_id_check,
            "with_cfg_unpinned must take tx_chain_id_check at face value"
        );
    }

    /// The escape hatch carries the caller's configuration verbatim: every field reaches the
    /// inner revm config as written.
    #[test]
    fn test_with_cfg_unpinned_carries_every_field() {
        let mut cfg = admissible_customized_cfg(MegaSpecId::REX6);
        cfg.tx_chain_id_check = true;

        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
            .with_cfg_unpinned(cfg.clone());

        assert!(context.inner.cfg.tx_chain_id_check);
        assert_eq!(context.inner.cfg, cfg.into_op_cfg());
    }

    /// Skipping the chain-id pin does not skip the schedule check: the escape hatch is an opt-in
    /// to revm 40's chain-id gate, not to owning the gas schedule.
    #[test]
    #[should_panic(expected = "gas params differ from the spec-defined schedule")]
    fn test_with_cfg_unpinned_rejects_a_schedule_off_the_spec_table() {
        let cfg = fully_customized_cfg(MegaSpecId::REX6);

        let _ =
            MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg_unpinned(cfg);
    }

    /// The opt-in skips the chain-id pin, not `MegaETH`'s other normalization: the spec is
    /// adopted and the `MINI_REX` size limits still fill in when unset.
    #[test]
    fn test_with_cfg_unpinned_still_applies_mega_pins() {
        for spec in [MegaSpecId::MINI_REX, MegaSpecId::REX5] {
            let cfg = CfgEnv::new_with_spec(spec);
            let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
                .with_cfg_unpinned(cfg);

            assert_eq!(context.mega_spec(), spec);
            assert_eq!(
                context.inner.cfg.limit_contract_code_size,
                Some(constants::mini_rex::MAX_CONTRACT_SIZE)
            );
            assert_eq!(
                context.inner.cfg.limit_contract_initcode_size,
                Some(constants::mini_rex::MAX_INITCODE_SIZE)
            );
            // The escape hatch still means what it says alongside those pins.
            assert!(context.inner.cfg.tx_chain_id_check);
        }
    }

    /// EIP-8037 is settled where it is read, not where a configuration enters: the entry points
    /// carry a caller's flag like any other field, and it is forced off on the way into execution.
    ///
    /// Both halves are asserted here. A caller's `true` survives `with_cfg` — that is what makes
    /// this a real probe rather than one blind to a flag the entry point already cleared — and
    /// `on_new_tx`, which `MegaHandler::before_run` and `Handler::run_system_call` both call
    /// before anything reads the flag, clears it.
    #[test]
    fn test_eip8037_is_forced_off_before_every_transaction() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX5);
        cfg.enable_amsterdam_eip8037 = true;

        let mut context =
            MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg(cfg);
        assert!(
            context.inner.cfg.enable_amsterdam_eip8037,
            "the entry point carries the caller's field; clearing it here would make the pin \
             below untestable",
        );

        context.on_new_tx();
        assert!(
            !context.inner.cfg.enable_amsterdam_eip8037,
            "every transaction runs with the state-gas split off",
        );

        // And it is forced every time, not just on the first transaction: a caller that sets the
        // flag between transactions is cleared again.
        context.inner.cfg.enable_amsterdam_eip8037 = true;
        context.on_new_tx();
        assert!(!context.inner.cfg.enable_amsterdam_eip8037);
    }

    /// Building the EVM settles the flag too, so the snapshot `alloy_evm::Evm::cfg_env` hands out
    /// and everything read back through it describe what transactions will run with.
    #[test]
    fn test_building_the_evm_forces_eip8037_off() {
        let mut cfg = CfgEnv::new_with_spec(MegaSpecId::REX5);
        cfg.enable_amsterdam_eip8037 = true;

        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE).with_cfg(cfg);
        let evm = crate::MegaEvm::new(context);

        assert!(!evm.ctx.inner.cfg.enable_amsterdam_eip8037);
        assert!(!alloy_evm::Evm::cfg_env(&evm).enable_amsterdam_eip8037);
    }

    /// The deprecated constructor rejects a rewritten schedule like every other entry point. Its
    /// input is checked against the spec the caller's own configuration carries — the relabel
    /// below re-derives the schedule for a configuration on a different spec, and must not be
    /// what quietly repairs a rewritten one.
    #[allow(deprecated)]
    #[test]
    #[should_panic(expected = "gas params differ from the spec-defined schedule")]
    fn test_new_with_context_rejects_a_schedule_off_the_spec_table() {
        let mut inner: MegaInnerContext<EmptyDB> = revm::Context::op()
            .with_tx(crate::MegaTransaction::default())
            .with_db(EmptyDB::default());
        inner.cfg.set_spec_and_mainnet_gas_params(MegaSpecId::EQUIVALENCE.into_op_spec());
        inner.cfg.gas_params = custom_gas_params();

        let _ = MegaContext::new_with_context(
            inner,
            MegaSpecId::EQUIVALENCE,
            ExternalEnvs::<EmptyExternalEnv>::default(),
        );
    }

    /// Same unconditional pin at the deprecated constructor: a configured context that enabled
    /// the check still comes out with the gate off.
    #[allow(deprecated)]
    #[test]
    fn test_new_with_context_pins_chain_id_check_off() {
        let mut inner: MegaInnerContext<EmptyDB> = revm::Context::op()
            .with_tx(crate::MegaTransaction::default())
            .with_db(EmptyDB::default());
        inner.cfg.chain_id = 4326;
        inner.cfg.tx_chain_id_check = true;

        let context = MegaContext::new_with_context(
            inner,
            MegaSpecId::EQUIVALENCE,
            ExternalEnvs::<EmptyExternalEnv>::default(),
        );

        assert!(
            !context.inner.cfg.tx_chain_id_check,
            "new_with_context must pin the chain-id gate off even when the caller enabled it"
        );
    }

    /// revm 40 keeps per-spec [`GasParams`] in [`CfgEnv`]. The deprecated
    /// `new_with_context` must update both when applying the `MegaETH` op-spec —
    /// bare `cfg.spec = ...` would leave the caller's BEDROCK/MERGE params.
    #[allow(deprecated)]
    #[test]
    fn test_new_with_context_syncs_gas_params_with_spec() {
        // `DefaultOp` seeds BEDROCK (MERGE eth) gas params.
        let inner: MegaInnerContext<EmptyDB> = revm::Context::op()
            .with_tx(crate::MegaTransaction::default())
            .with_db(EmptyDB::default());
        assert_eq!(inner.cfg.spec, OpSpecId::BEDROCK);
        assert_eq!(inner.cfg.gas_params, GasParams::new_spec(SpecId::from(OpSpecId::BEDROCK)),);

        let context = MegaContext::new_with_context(
            inner,
            MegaSpecId::EQUIVALENCE,
            ExternalEnvs::<EmptyExternalEnv>::default(),
        );

        let op_spec = MegaSpecId::EQUIVALENCE.into_op_spec();
        assert_eq!(context.inner.cfg.spec, op_spec);
        assert_eq!(context.inner.cfg.gas_params, GasParams::new_spec(SpecId::from(op_spec)));
        // Distinct from the caller's BEDROCK params so a bare-assignment bug
        // cannot pass this test by accident.
        assert_ne!(
            context.inner.cfg.gas_params,
            GasParams::new_spec(SpecId::from(OpSpecId::BEDROCK)),
        );
    }

    #[test]
    fn test_with_cfg_updates_spec() {
        // Create context with initial spec
        let mut context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE);

        // Verify initial state
        assert_eq!(context.mega_spec(), MegaSpecId::EQUIVALENCE);
        assert_eq!(context.inner.cfg.spec, OpSpecId::from(MegaSpecId::EQUIVALENCE));

        // Create new config with different spec
        let new_cfg = CfgEnv::new_with_spec(MegaSpecId::MINI_REX);

        // Apply new config using with_cfg
        context = context.with_cfg(new_cfg);

        // Verify that both the context's spec and inner config's spec are updated
        assert_eq!(context.mega_spec(), MegaSpecId::MINI_REX);
        assert_eq!(context.inner.cfg.spec, OpSpecId::from(MegaSpecId::MINI_REX));
    }

    #[test]
    fn test_with_cfg_spec_consistency() {
        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE);

        // Test multiple spec transitions
        let specs_to_test = [MegaSpecId::MINI_REX, MegaSpecId::EQUIVALENCE];

        let mut current_context = context;
        for spec in specs_to_test {
            let cfg = CfgEnv::new_with_spec(spec);
            current_context = current_context.with_cfg(cfg);

            // Verify consistency between context spec and inner config spec
            assert_eq!(current_context.mega_spec(), spec);
            assert_eq!(current_context.inner.cfg.spec, OpSpecId::from(spec));
        }
    }

    /// Compute limit tight enough that a leftover REX7 gas clamp is visible in receipt gas.
    const CFG_MIGRATION_COMPUTE_LIMIT: u64 = 50_000;
    /// Transaction gas limit used by the `PUSH0 STOP` migration probe.
    const CFG_MIGRATION_TX_GAS_LIMIT: u64 = 1_000_000;
    const CFG_MIGRATION_CALLER: Address = address!("0000000000000000000000000000000000300000");
    const CFG_MIGRATION_CONTRACT: Address = address!("0000000000000000000000000000000000300001");
    /// `PUSH0 STOP` — a compute-only body, so a leftover gas clamp shows up as receipt gas.
    const CFG_MIGRATION_CODE: [u8; 2] = [0x5f, 0x00];

    #[derive(Debug, PartialEq, Eq)]
    struct CfgMigrationOutcome {
        success: bool,
        gas_used: u64,
        compute_gas: u64,
    }

    fn cfg_migration_limits() -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7)
            .with_tx_compute_gas_limit(CFG_MIGRATION_COMPUTE_LIMIT)
    }

    fn cfg_migration_db() -> MemoryDatabase {
        MemoryDatabase::default()
            .account_balance(CFG_MIGRATION_CALLER, U256::from(10).pow(U256::from(18)))
            .account_code(CFG_MIGRATION_CONTRACT, Bytes::from_static(&CFG_MIGRATION_CODE))
    }

    fn run_cfg_migration_tx<DB: Database>(
        mut context: MegaContext<DB, EmptyExternalEnv>,
    ) -> CfgMigrationOutcome {
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });

        let tx = TxEnvBuilder::default()
            .caller(CFG_MIGRATION_CALLER)
            .call(CFG_MIGRATION_CONTRACT)
            .gas_limit(CFG_MIGRATION_TX_GAS_LIMIT)
            .build_fill();
        let mut tx = crate::MegaTransaction::new(tx);
        tx.enveloped_tx = Some(Bytes::new());

        let mut evm = crate::MegaEvm::new(context);
        let result =
            alloy_evm::Evm::transact_raw(&mut evm, tx).expect("cfg-migration probe must execute");
        let compute_gas = evm.ctx.additional_limit.borrow().get_usage().compute_gas;
        CfgMigrationOutcome {
            success: result.result.is_success(),
            gas_used: result.result.tx_gas_used(),
            compute_gas,
        }
    }

    /// Spec-latched tracker bits must follow `with_cfg`, using the already-configured limits.
    #[test]
    fn test_with_cfg_rebuilds_latched_limit_state_when_spec_changes() {
        let limits = cfg_migration_limits();

        let rex7_to_rex6 = MegaContext::new(EmptyDB::default(), MegaSpecId::REX7)
            .with_tx_runtime_limits(limits)
            .with_cfg(CfgEnv::new_with_spec(MegaSpecId::REX6));
        let rex6_direct =
            MegaContext::new(EmptyDB::default(), MegaSpecId::REX6).with_tx_runtime_limits(limits);

        assert_eq!(rex7_to_rex6.mega_spec(), MegaSpecId::REX6);
        assert_eq!(
            rex7_to_rex6.additional_limit.borrow().rex7_enabled(),
            rex6_direct.additional_limit.borrow().rex7_enabled(),
        );
        assert!(
            !rex7_to_rex6.additional_limit.borrow().rex7_enabled(),
            "REX6 must not latch checkpoint accounting"
        );
        assert_eq!(rex7_to_rex6.additional_limit.borrow().limits, limits);

        let rex6_to_rex7 = MegaContext::new(EmptyDB::default(), MegaSpecId::REX6)
            .with_tx_runtime_limits(limits)
            .with_cfg(CfgEnv::new_with_spec(MegaSpecId::REX7));
        let rex7_direct =
            MegaContext::new(EmptyDB::default(), MegaSpecId::REX7).with_tx_runtime_limits(limits);

        assert_eq!(rex6_to_rex7.mega_spec(), MegaSpecId::REX7);
        assert_eq!(
            rex6_to_rex7.additional_limit.borrow().rex7_enabled(),
            rex7_direct.additional_limit.borrow().rex7_enabled(),
        );
        assert!(
            rex6_to_rex7.additional_limit.borrow().rex7_enabled(),
            "REX7 must latch checkpoint accounting"
        );
        assert_eq!(rex6_to_rex7.additional_limit.borrow().limits, limits);

        let via_unpinned = MegaContext::new(EmptyDB::default(), MegaSpecId::REX7)
            .with_tx_runtime_limits(limits)
            .with_cfg_unpinned(CfgEnv::new_with_spec(MegaSpecId::REX6));
        assert!(
            !via_unpinned.additional_limit.borrow().rex7_enabled(),
            "with_cfg_unpinned must rebuild latched limit state on a spec change"
        );
        assert_eq!(via_unpinned.additional_limit.borrow().limits, limits);
    }

    /// Same-spec `with_cfg` must not replace the additional-limit `Rc`.
    #[test]
    fn test_with_cfg_same_spec_keeps_additional_limit_identity() {
        let limits = cfg_migration_limits();
        let context =
            MegaContext::new(EmptyDB::default(), MegaSpecId::REX6).with_tx_runtime_limits(limits);
        let before = Rc::clone(&context.additional_limit);

        let context = context.with_cfg(CfgEnv::new_with_spec(MegaSpecId::REX6));

        assert!(Rc::ptr_eq(&before, &context.additional_limit));
        assert_eq!(context.additional_limit.borrow().limits, limits);
        assert!(!context.additional_limit.borrow().rex7_enabled());
    }

    #[test]
    fn test_with_cfg_rex7_to_rex6_matches_direct_rex6_when_limits_applied_first() {
        let limits = cfg_migration_limits();

        let mut migrated_db = cfg_migration_db();
        let migrated = run_cfg_migration_tx(
            MegaContext::new(&mut migrated_db, MegaSpecId::REX7)
                .with_tx_runtime_limits(limits)
                .with_cfg(CfgEnv::new_with_spec(MegaSpecId::REX6)),
        );

        let mut direct_db = cfg_migration_db();
        let direct = run_cfg_migration_tx(
            MegaContext::new(&mut direct_db, MegaSpecId::REX6).with_tx_runtime_limits(limits),
        );

        assert_eq!(migrated, direct);
    }

    #[test]
    fn test_with_cfg_rex6_to_rex7_matches_direct_rex7_when_limits_applied_first() {
        let limits = cfg_migration_limits();

        let mut migrated_db = cfg_migration_db();
        let migrated = run_cfg_migration_tx(
            MegaContext::new(&mut migrated_db, MegaSpecId::REX6)
                .with_tx_runtime_limits(limits)
                .with_cfg(CfgEnv::new_with_spec(MegaSpecId::REX7)),
        );

        let mut direct_db = cfg_migration_db();
        let direct = run_cfg_migration_tx(
            MegaContext::new(&mut direct_db, MegaSpecId::REX7).with_tx_runtime_limits(limits),
        );

        assert_eq!(migrated, direct);
    }

    #[test]
    fn test_with_cfg_rex7_to_rex6_matches_direct_rex6_when_limits_applied_after() {
        let limits = cfg_migration_limits();

        let mut migrated_db = cfg_migration_db();
        let migrated = run_cfg_migration_tx(
            MegaContext::new(&mut migrated_db, MegaSpecId::REX7)
                .with_cfg(CfgEnv::new_with_spec(MegaSpecId::REX6))
                .with_tx_runtime_limits(limits),
        );

        let mut direct_db = cfg_migration_db();
        let direct = run_cfg_migration_tx(
            MegaContext::new(&mut direct_db, MegaSpecId::REX6).with_tx_runtime_limits(limits),
        );

        assert_eq!(migrated, direct);
    }

    #[test]
    fn test_with_cfg_rex6_to_rex7_matches_direct_rex7_when_limits_applied_after() {
        let limits = cfg_migration_limits();

        let mut migrated_db = cfg_migration_db();
        let migrated = run_cfg_migration_tx(
            MegaContext::new(&mut migrated_db, MegaSpecId::REX6)
                .with_cfg(CfgEnv::new_with_spec(MegaSpecId::REX7))
                .with_tx_runtime_limits(limits),
        );

        let mut direct_db = cfg_migration_db();
        let direct = run_cfg_migration_tx(
            MegaContext::new(&mut direct_db, MegaSpecId::REX7).with_tx_runtime_limits(limits),
        );

        assert_eq!(migrated, direct);
    }

    /// Sharing SALT env handles between parent and sandbox must not merge their bucket caches.
    #[test]
    fn test_shared_salt_env_keeps_dynamic_gas_cache_isolated() {
        let external_envs = TestExternalEnvs::new();
        let parent = MegaContext::new(EmptyDB::default(), MegaSpecId::REX4)
            .with_external_envs(external_envs.into());
        let parent_address = address!("0000000000000000000000000000000000100001");
        let sandbox_address = address!("0000000000000000000000000000000000100002");

        parent
            .dynamic_storage_gas_cost
            .borrow_mut()
            .new_account_gas(parent_address)
            .expect("parent bucket lookup should succeed");
        let parent_bucket_ids = parent.accessed_bucket_ids();

        let sandbox =
            MegaContext::<_, TestExternalEnvs<std::convert::Infallible>>::new_with_shared_ext_envs(
                EmptyDB::default(),
                MegaSpecId::REX4,
                Rc::clone(&parent.salt_env),
                Rc::clone(&parent.oracle_env),
            )
            .with_block(parent.block().clone())
            .with_chain(parent.chain().clone())
            .with_inside_sandbox(true);
        sandbox
            .dynamic_storage_gas_cost
            .borrow_mut()
            .new_account_gas(sandbox_address)
            .expect("sandbox bucket lookup should succeed");

        assert_eq!(parent.accessed_bucket_ids(), parent_bucket_ids);
        assert_ne!(sandbox.accessed_bucket_ids(), parent_bucket_ids);
    }

    /// revm 40 defaults `tx_chain_id_check` to `true`; `MegaContext` pins revm-27's
    /// `false` so unit-test txs with `chain_id: None` still validate.
    #[test]
    fn test_new_disables_tx_chain_id_check_for_revm27_parity() {
        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::REX5);
        assert!(
            !context.inner.cfg.tx_chain_id_check,
            "MegaContext::new must pin revm-27 default (tx_chain_id_check=false)"
        );
    }

    /// Deprecated constructor must apply the same revm-27 chain-id pin, even when
    /// the caller's `OpContext` was built with revm 40's default (`true`).
    #[allow(deprecated)]
    #[test]
    fn test_new_with_context_disables_tx_chain_id_check() {
        let mut inner: MegaInnerContext<EmptyDB> = revm::Context::op()
            .with_tx(crate::MegaTransaction::default())
            .with_db(EmptyDB::default());
        // Simulate a caller that left revm 40's default (true) in place.
        inner.cfg.tx_chain_id_check = true;

        let context = MegaContext::new_with_context(
            inner,
            MegaSpecId::EQUIVALENCE,
            ExternalEnvs::<EmptyExternalEnv>::default(),
        );
        assert!(
            !context.inner.cfg.tx_chain_id_check,
            "new_with_context must pin revm-27 default regardless of caller's cfg"
        );
    }

    /// The deprecated constructor fills the `MINI_REX` contract-size limits when the caller left
    /// them unset. Pins the enabled side of the gate: dropping it would silently fall back to
    /// revm's 24 KiB EIP-170 cap for every `MINI_REX`-and-later context built this way.
    #[allow(deprecated)]
    #[test]
    fn test_new_with_context_fills_mini_rex_contract_size_limits() {
        let inner: MegaInnerContext<EmptyDB> = revm::Context::op()
            .with_tx(crate::MegaTransaction::default())
            .with_db(EmptyDB::default());
        assert_eq!(inner.cfg.limit_contract_code_size, None, "caller leaves the limits unset");
        assert_eq!(inner.cfg.limit_contract_initcode_size, None);

        let context = MegaContext::new_with_context(
            inner,
            MegaSpecId::MINI_REX,
            ExternalEnvs::<EmptyExternalEnv>::default(),
        );

        assert_eq!(
            context.inner.cfg.limit_contract_code_size,
            Some(constants::mini_rex::MAX_CONTRACT_SIZE)
        );
        assert_eq!(
            context.inner.cfg.limit_contract_initcode_size,
            Some(constants::mini_rex::MAX_INITCODE_SIZE)
        );
    }

    /// The `EQUIVALENCE` side of the same gate: the enlarged `MINI_REX` sizes must not leak into
    /// the baseline spec, which keeps revm's EIP-170 / EIP-3860 defaults.
    #[allow(deprecated)]
    #[test]
    fn test_new_with_context_keeps_equivalence_contract_size_defaults() {
        let inner: MegaInnerContext<EmptyDB> = revm::Context::op()
            .with_tx(crate::MegaTransaction::default())
            .with_db(EmptyDB::default());

        let context = MegaContext::new_with_context(
            inner,
            MegaSpecId::EQUIVALENCE,
            ExternalEnvs::<EmptyExternalEnv>::default(),
        );

        assert_eq!(
            context.inner.cfg.limit_contract_code_size, None,
            "EQUIVALENCE must keep revm's EIP-170 default"
        );
        assert_eq!(
            context.inner.cfg.limit_contract_initcode_size, None,
            "EQUIVALENCE must keep revm's EIP-3860 default"
        );
    }

    /// `with_cfg` fills the `MINI_REX` sizes only from `MINI_REX` on; an `EQUIVALENCE` config
    /// comes out with revm's defaults untouched. Complements
    /// `test_with_cfg_unpinned_still_applies_mega_pins`, which pins the enabled side.
    #[test]
    fn test_with_cfg_keeps_equivalence_contract_size_defaults() {
        let context = MegaContext::new(EmptyDB::default(), MegaSpecId::EQUIVALENCE)
            .with_cfg(CfgEnv::new_with_spec(MegaSpecId::EQUIVALENCE));

        assert_eq!(context.mega_spec(), MegaSpecId::EQUIVALENCE);
        assert_eq!(
            context.inner.cfg.limit_contract_code_size, None,
            "EQUIVALENCE must keep revm's EIP-170 default"
        );
        assert_eq!(
            context.inner.cfg.limit_contract_initcode_size, None,
            "EQUIVALENCE must keep revm's EIP-3860 default"
        );
    }

    /// The test/bench-only `new_with_ext_envs` wrapper builds a `MegaContext`
    /// over a caller-supplied external environment (`TestExternalEnvs`), at the
    /// requested spec and wired to the given SALT/oracle handles.
    #[test]
    fn test_new_with_ext_envs_builds_over_configurable_env() {
        let env = TestExternalEnvs::<std::convert::Infallible>::new();
        let context =
            MegaContext::<_, TestExternalEnvs<std::convert::Infallible>>::new_with_ext_envs(
                EmptyDB::default(),
                MegaSpecId::REX5,
                Rc::new(env.clone()),
                Rc::new(RefCell::new(env)),
            );

        assert_eq!(context.mega_spec(), MegaSpecId::REX5);
        // The supplied SALT env is wired through: a bucket lookup against the
        // dynamic-gas cache succeeds.
        context
            .dynamic_storage_gas_cost
            .borrow_mut()
            .new_account_gas(address!("0000000000000000000000000000000000100003"))
            .expect("bucket lookup against the supplied env should succeed");
    }
}
