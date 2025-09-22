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

use alloy_evm::Database;
use alloy_primitives::Address;
use delegate::delegate;
use op_revm::{DefaultOp, L1BlockInfo, OpContext, OpSpecId};
use revm::{
    context::{BlockEnv, CfgEnv, ContextSetters, ContextTr, LocalContext},
    context_interface::context::ContextError,
    Journal,
};
use std::{cell::RefCell, rc::Rc};

use crate::{
    constants, AdditionalLimit, BlockEnvAccess, ExternalEnvOracle, GasCostOracle, MegaSpecId,
};

/// `MegaETH` EVM context type. This struct wraps [`OpContext`] and implements the [`ContextTr`]
/// trait to be used as the context for the [`crate::Evm`].
#[derive(Debug, derive_more::Deref, derive_more::DerefMut)]
pub struct MegaContext<DB: Database, Oracle: ExternalEnvOracle> {
    /// The inner context.
    #[deref]
    #[deref_mut]
    pub(crate) inner: OpContext<DB>,
    /// The `MegaETH` spec id. The inner context contains the `OpSpecId`.
    /// The `OpSpec` in the `inner` context should be the corresponding [`OpSpecId`] for the
    /// [`SpecId`].
    pub(crate) spec: MegaSpecId,

    /// Whether to disable the post-transaction reward to beneficiary.
    pub(crate) disable_beneficiary: bool,

    /// Additional limits for the EVM.
    pub(crate) additional_limit: Rc<RefCell<AdditionalLimit>>,

    /// An oracle for the gas cost during the transaction execution.
    pub(crate) gas_cost_oracle: Rc<RefCell<GasCostOracle<Oracle>>>,

    /* Internal state variables */
    /// Bitmap of block environment data accessed during transaction execution.
    pub(crate) block_env_accessed: RefCell<BlockEnvAccess>,
    /// Whether beneficiary data has been accessed in current transaction
    pub(crate) beneficiary_balance_accessed: RefCell<bool>,
}

/* Constructors */
impl<DB: Database, Oracle: ExternalEnvOracle> MegaContext<DB, Oracle> {
    /// Creates a new `Context` with the given database, specification, and oracle.
    ///
    /// This constructor initializes a new `MegaETH` EVM context with default settings.
    /// For the `MINI_REX` specification, it automatically configures appropriate
    /// contract size and initcode size limits.
    ///
    /// # Arguments
    ///
    /// * `db` - The database implementation to use for state storage
    /// * `spec` - The `MegaETH` specification version to use
    /// * `oracle` - The external environment oracle for gas cost calculations
    ///
    /// # Returns
    ///
    /// Returns a new `Context` instance with default configuration.
    pub fn new(db: DB, spec: MegaSpecId, oracle: Oracle) -> Self {
        let mut inner =
            revm::Context::op().with_db(db).with_cfg(CfgEnv::new_with_spec(spec.into_op_spec()));

        // For the `MINI_REX` spec, we override the contract size and initcode size limits.
        if spec.is_enabled(MegaSpecId::MINI_REX) {
            inner.cfg.limit_contract_code_size = Some(constants::mini_rex::MAX_CONTRACT_SIZE);
            inner.cfg.limit_contract_initcode_size = Some(constants::mini_rex::MAX_INITCODE_SIZE);
        }

        Self {
            spec,
            disable_beneficiary: false,
            additional_limit: Rc::new(RefCell::new(AdditionalLimit::default())),
            gas_cost_oracle: Rc::new(RefCell::new(GasCostOracle::new(
                oracle,
                inner.block.number.to::<u64>().saturating_sub(1),
            ))),
            block_env_accessed: RefCell::new(BlockEnvAccess::empty()),
            beneficiary_balance_accessed: RefCell::new(false),
            inner,
        }
    }

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
    /// * `oracle` - The external environment oracle for gas cost calculations
    ///
    /// # Returns
    ///
    /// Returns a new `Context` instance wrapping the provided context.
    pub fn new_with_context(context: OpContext<DB>, spec: MegaSpecId, oracle: Oracle) -> Self {
        let mut inner = context;

        // spec in context must keep the same with parameter `spec`
        inner.cfg.spec = spec.into_op_spec();

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

        Self {
            spec,
            disable_beneficiary: false,
            additional_limit: Rc::new(RefCell::new(AdditionalLimit::default())),
            gas_cost_oracle: Rc::new(RefCell::new(GasCostOracle::new(
                oracle,
                inner.block.number.to::<u64>() - 1,
            ))),
            block_env_accessed: RefCell::new(BlockEnvAccess::empty()),
            beneficiary_balance_accessed: RefCell::new(false),
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
    pub fn with_db<ODB: Database>(self, db: ODB) -> MegaContext<ODB, Oracle> {
        MegaContext {
            inner: self.inner.with_db(db),
            spec: self.spec,
            disable_beneficiary: self.disable_beneficiary,
            additional_limit: self.additional_limit,
            block_env_accessed: self.block_env_accessed,
            beneficiary_balance_accessed: self.beneficiary_balance_accessed,
            gas_cost_oracle: self.gas_cost_oracle,
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
    /// # Arguments
    ///
    /// * `cfg` - The configuration environment
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_cfg(mut self, cfg: CfgEnv<MegaSpecId>) -> Self {
        self.inner = self.inner.with_cfg(cfg.into_op_cfg());
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
        self
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

    /// Sets the data limit for the EVM.
    ///
    /// When the data limit is reached, the transaction will error and halt
    /// (consuming all remaining gas). This limit controls the maximum amount
    /// of data that can be processed during transaction execution.
    ///
    /// # Arguments
    ///
    /// * `data_limit` - The maximum amount of data allowed (in bytes)
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_data_limit(self, data_limit: u64) -> Self {
        self.additional_limit.borrow_mut().data_limit = data_limit;
        self
    }

    /// Sets the KV update limit for the EVM. When the KV update limit is reached,
    /// the transaction will error and halt (consuming all remaining gas).
    ///
    /// This limit controls the maximum number of key-value storage operations
    /// (reads, writes, deletes) that can be performed during transaction execution.
    ///
    /// # Arguments
    ///
    /// * `kv_update_limit` - The maximum number of KV operations allowed
    ///
    /// # Returns
    ///
    /// Returns `self` for method chaining.
    pub fn with_kv_update_limit(self, kv_update_limit: u64) -> Self {
        self.additional_limit.borrow_mut().kv_update_limit = kv_update_limit;
        self
    }
}

/* Getters */
impl<DB: Database, Oracle: ExternalEnvOracle> MegaContext<DB, Oracle> {
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

    /// Gets the current total data size generated from transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the current total data size in bytes generated so far. The data size is reset at the
    /// beginning of each transaction.
    pub fn generated_data_size(&self) -> u64 {
        self.additional_limit.borrow().data_size_tracker.current_size()
    }

    /// Gets the current total number of key-value updates performed during transaction execution.
    ///
    /// # Returns
    ///
    /// Returns the current total number of KV operations performed so far. The count is reset at
    /// the beginning of each transaction.
    pub fn kv_update_count(&self) -> u64 {
        self.additional_limit.borrow().kv_update_counter.current_count()
    }

    /// Consumes the context and converts it into the inner `OpContext`.
    ///
    /// This method extracts the underlying `OpStack` context, discarding
    /// all MegaETH-specific state and configuration.
    ///
    /// # Returns
    ///
    /// Returns the inner `OpContext<DB>`.
    pub fn into_inner(self) -> OpContext<DB> {
        self.inner
    }
}

/* Block Environment Access Tracking */
impl<DB: Database, Oracle: ExternalEnvOracle> MegaContext<DB, Oracle> {
    /// Returns the bitmap of block environment data accessed during transaction execution.
    ///
    /// This method provides information about which block environment fields
    /// have been accessed during the current transaction, which is useful for
    /// optimization and analysis purposes.
    ///
    /// # Returns
    ///
    /// Returns a [`BlockEnvAccess`] bitmap indicating accessed fields.
    pub fn get_block_env_accesses(&self) -> BlockEnvAccess {
        *self.block_env_accessed.borrow()
    }

    /// Resets the block environment access bitmap for new transactions.
    ///
    /// This method clears the tracking of which block environment fields
    /// have been accessed, preparing the context for a new transaction.
    pub fn reset_block_env_access(&mut self) {
        *self.block_env_accessed.borrow_mut() = BlockEnvAccess::empty();
        *self.beneficiary_balance_accessed.borrow_mut() = false;
    }

    /// Marks that a specific type of block environment has been accessed.
    ///
    /// This internal method is used to track which block environment fields
    /// are being accessed during transaction execution.
    ///
    /// # Arguments
    ///
    /// * `access_type` - The type of block environment access to record
    pub(crate) fn mark_block_env_accessed(&self, access_type: BlockEnvAccess) {
        self.block_env_accessed.borrow_mut().insert(access_type);
    }
}

/* Beneficiary Access Tracking */
impl<DB: Database, Oracle: ExternalEnvOracle> MegaContext<DB, Oracle> {
    /// Disables the beneficiary reward.
    pub fn disable_beneficiary(&mut self) {
        self.disable_beneficiary = true;
    }

    /// Check if beneficiary data has been accessed in current transaction
    pub fn has_accessed_beneficiary_balance(&self) -> bool {
        *self.beneficiary_balance_accessed.borrow()
    }

    /// Check if address is beneficiary and mark access if so. Returns true if beneficiary was
    /// accessed.
    pub(crate) fn check_and_mark_beneficiary_balance_access(&self, address: &Address) -> bool {
        if self.inner.block.beneficiary == *address {
            *self.beneficiary_balance_accessed.borrow_mut() = true;
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
            *self.beneficiary_balance_accessed.borrow_mut() = true;
        }

        // Check if recipient is beneficiary (for calls)
        if let revm::primitives::TxKind::Call(recipient) = tx.base.kind {
            if recipient == beneficiary {
                *self.beneficiary_balance_accessed.borrow_mut() = true;
            }
        }
    }
}

/* Hooks */
impl<DB: Database, Oracle: ExternalEnvOracle> MegaContext<DB, Oracle> {
    /// Resets the internal state for a new block.
    ///
    /// This method is called when transitioning to a new block and updates
    /// the gas cost oracle and additional limits accordingly.
    pub(crate) fn on_new_block(&self) {
        // The gas cost oracle is only enabled when the `MINI_REX` spec is enabled.
        if self.spec.is_enabled(MegaSpecId::MINI_REX) {
            self.gas_cost_oracle.borrow_mut().on_new_block(&self.inner.block);
        }
    }

    /// Resets the internal state for a new transaction.
    ///
    /// This method is called when starting a new transaction and resets
    /// block environment access tracking and additional limits.
    pub(crate) fn on_new_tx(&mut self) {
        self.reset_block_env_access();
        self.check_tx_beneficiary_access();

        // Apply the additional limits only when the `MINI_REX` spec is enabled.
        if self.spec.is_enabled(MegaSpecId::MINI_REX) {
            self.additional_limit.borrow_mut().before_tx_start(&self.inner.tx);
        }
    }
}

/// Implementation of the `ContextTr` trait for `Context`.
///
/// This implementation delegates most methods to the inner `OpContext` while
/// maintaining the MegaETH-specific functionality. The trait provides access
/// to the core EVM context components like transaction, block, configuration,
/// database, journal, and chain information.
impl<DB: Database, Oracle: ExternalEnvOracle> ContextTr for MegaContext<DB, Oracle> {
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
        }
    }
}

/// Implementation of the `ContextSetters` trait for `Context`.
///
/// This implementation provides methods to update the context state, with
/// special handling for transaction updates to reset internal state.
impl<DB: Database, Oracle: ExternalEnvOracle> ContextSetters for MegaContext<DB, Oracle> {
    delegate! {
        to self.inner {
            fn set_block(&mut self, block: Self::Block);
            fn set_tx(&mut self, tx: Self::Tx);
        }
    }
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
    /// This method creates a new `OpStack` configuration environment with the
    /// same settings as the `MegaETH` configuration, converting the specification ID.
    ///
    /// # Returns
    ///
    /// Returns a new `CfgEnv<OpSpecId>` with all fields copied from `self`.
    ///
    /// # Note
    ///
    /// When the fields of [`CfgEnv`] change, this function needs to be updated
    /// to include the new fields.
    fn into_op_cfg(self) -> CfgEnv<OpSpecId> {
        let mut op_cfg = CfgEnv::new_with_spec(OpSpecId::from(self.spec));
        op_cfg.chain_id = self.chain_id;
        op_cfg.tx_chain_id_check = self.tx_chain_id_check;
        op_cfg.limit_contract_code_size = self.limit_contract_code_size;
        op_cfg.limit_contract_initcode_size = self.limit_contract_initcode_size;
        op_cfg.disable_nonce_check = self.disable_nonce_check;
        op_cfg.max_blobs_per_tx = self.max_blobs_per_tx;
        op_cfg.blob_base_fee_update_fraction = self.blob_base_fee_update_fraction;
        op_cfg.tx_gas_limit_cap = self.tx_gas_limit_cap;
        op_cfg.memory_limit = self.memory_limit;
        op_cfg.disable_balance_check = self.disable_balance_check;
        op_cfg.disable_block_gas_limit = self.disable_block_gas_limit;
        op_cfg.disable_eip3541 = self.disable_eip3541;
        op_cfg.disable_eip3607 = self.disable_eip3607;
        op_cfg.disable_base_fee = self.disable_base_fee;
        op_cfg
    }
}

/// Implementation of `IntoMegaethCfgEnv` for `CfgEnv<OpSpecId>`.
///
/// This implementation converts an `OpStack` configuration environment to a
/// `MegaETH` configuration environment by copying all relevant fields.
impl IntoMegaethCfgEnv for CfgEnv<OpSpecId> {
    /// Converts to `CfgEnv<SpecId>`.
    ///
    /// This method creates a new `MegaETH` configuration environment with the
    /// same settings as the `OpStack` configuration, using the provided specification ID.
    ///
    /// # Arguments
    ///
    /// * `spec` - The `MegaETH` specification ID to use in the new configuration
    ///
    /// # Returns
    ///
    /// Returns a new `CfgEnv<SpecId>` with all fields copied from `self`.
    ///
    /// # Note
    ///
    /// When the fields of [`CfgEnv`] change, this function needs to be updated
    /// to include the new fields.
    fn into_megaeth_cfg(self, spec: MegaSpecId) -> CfgEnv<MegaSpecId> {
        let mut cfg = CfgEnv::new_with_spec(spec);
        cfg.chain_id = self.chain_id;
        cfg.tx_chain_id_check = self.tx_chain_id_check;
        cfg.limit_contract_code_size = self.limit_contract_code_size;
        cfg.limit_contract_initcode_size = self.limit_contract_initcode_size;
        cfg.disable_nonce_check = self.disable_nonce_check;
        cfg.max_blobs_per_tx = self.max_blobs_per_tx;
        cfg.blob_base_fee_update_fraction = self.blob_base_fee_update_fraction;
        cfg.tx_gas_limit_cap = self.tx_gas_limit_cap;
        cfg.memory_limit = self.memory_limit;
        cfg.disable_balance_check = self.disable_balance_check;
        cfg.disable_block_gas_limit = self.disable_block_gas_limit;
        cfg.disable_eip3541 = self.disable_eip3541;
        cfg.disable_eip3607 = self.disable_eip3607;
        cfg.disable_base_fee = self.disable_base_fee;
        cfg
    }
}
