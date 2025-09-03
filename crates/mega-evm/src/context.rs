use alloy_evm::Database;
use delegate::delegate;
use op_revm::{DefaultOp, L1BlockInfo, OpContext, OpSpecId};
use revm::{
    context::{BlockEnv, CfgEnv, ContextSetters, ContextTr, LocalContext},
    context_interface::context::ContextError,
    inspector::JournalExt,
    Journal,
};
use std::cell::RefCell;

use crate::{constants, BlockEnvAccess, SpecId, Transaction};
use alloy_primitives::{Address, Bytes, Log, B256, U256};

/// `MegaETH` EVM context type.
#[derive(Debug, derive_more::Deref, derive_more::DerefMut)]
pub struct Context<DB: Database> {
    /// The inner context. The inner context contains the `OpSpecId`, which should be kept
    /// consistent with the `spec` field using [`SpecId::into_op_spec()`].
    #[deref]
    #[deref_mut]
    pub(crate) inner: OpContext<DB>,
    /// The `MegaETH` spec id.
    /// The consistency between the spec here and `inner` context should be maintained and
    /// guaranteed by the caller.
    spec: SpecId,

    /* Internal state variables */
    /// The total size of all log data.
    pub(crate) log_data_size: u64,
    /// Bitmap of block environment data accessed during transaction execution.
    pub(crate) block_env_accessed: RefCell<BlockEnvAccess>,
    /// Whether beneficiary data has been accessed in current transaction
    pub(crate) beneficiary_balance_accessed: RefCell<bool>,
}

impl<DB: Database> Context<DB> {
    /// Create a new `MegaethContext` with the given database.
    pub fn new(db: DB, spec: SpecId) -> Self {
        let mut inner =
            revm::Context::op().with_db(db).with_cfg(CfgEnv::new_with_spec(spec.into_op_spec()));

        if spec.is_enabled_in(SpecId::MINI_REX) {
            inner.cfg.limit_contract_code_size = Some(constants::mini_rex::MAX_CONTRACT_SIZE);
            inner.cfg.limit_contract_initcode_size = Some(constants::mini_rex::MAX_INITCODE_SIZE);
        }

        Self {
            inner,
            spec,
            log_data_size: 0,
            block_env_accessed: RefCell::new(BlockEnvAccess::empty()),
            beneficiary_balance_accessed: RefCell::new(false),
        }
    }

    /// Create a new `MegaethContext` with the given `revm::Context`.
    pub fn new_with_context(context: OpContext<DB>, spec: SpecId) -> Self {
        let mut inner = context;

        // spec in context must keep the same with parameter `spec`
        inner.cfg.spec = spec.into_op_spec();
        if spec.is_enabled_in(SpecId::MINI_REX) {
            if inner.cfg.limit_contract_code_size.is_none() {
                inner.cfg.limit_contract_code_size = Some(constants::mini_rex::MAX_CONTRACT_SIZE);
            }
            if inner.cfg.limit_contract_initcode_size.is_none() {
                inner.cfg.limit_contract_initcode_size =
                    Some(constants::mini_rex::MAX_INITCODE_SIZE);
            }
        }

        Self {
            inner,
            spec,
            log_data_size: 0,
            block_env_accessed: RefCell::new(BlockEnvAccess::empty()),
            beneficiary_balance_accessed: RefCell::new(false),
        }
    }

    /// Set the database.
    pub fn with_db<ODB: Database>(self, db: ODB) -> Context<ODB> {
        Context {
            inner: self.inner.with_db(db),
            spec: self.spec,
            log_data_size: self.log_data_size,
            block_env_accessed: self.block_env_accessed,
            beneficiary_balance_accessed: self.beneficiary_balance_accessed,
        }
    }

    /// Set the transaction.
    pub fn with_tx(mut self, tx: Transaction) -> Self {
        self.inner = self.inner.with_tx(tx);
        self
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

    /// Set the block.
    pub fn with_block(mut self, block: BlockEnv) -> Self {
        self.inner = self.inner.with_block(block);
        self
    }

    /// Set the configuration.
    pub fn with_cfg(mut self, cfg: CfgEnv<SpecId>) -> Self {
        self.inner = self.inner.with_cfg(cfg.into_op_cfg());
        if self.spec.is_enabled_in(SpecId::MINI_REX) {
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

    /// Set the chain.
    pub fn with_chain(mut self, chain: L1BlockInfo) -> Self {
        self.inner = self.inner.with_chain(chain);
        self
    }

    /// Get the `MegaETH` spec id. This value should be consistent with the `spec` field by
    /// coverting this value to `OpSpecId`.
    pub fn megaeth_spec(&self) -> SpecId {
        self.spec
    }

    /// Convert the `MegaethContext` into the inner `OpContext`.
    pub fn into_inner(self) -> OpContext<DB> {
        self.inner
    }

    /// Returns the bitmap of block environment data accessed during transaction execution.
    pub fn get_block_env_accesses(&self) -> BlockEnvAccess {
        *self.block_env_accessed.borrow()
    }

    /// Resets the block environment access bitmap (for new transactions).
    pub fn reset_block_env_access(&mut self) {
        *self.block_env_accessed.borrow_mut() = BlockEnvAccess::empty();
        *self.beneficiary_balance_accessed.borrow_mut() = false;
    }

    /// Marks that a specific type of block environment has been accessed.
    pub(crate) fn mark_block_env_accessed(&self, access_type: BlockEnvAccess) {
        self.block_env_accessed.borrow_mut().insert(access_type);
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
}

impl<DB: Database> ContextTr for Context<DB> {
    type Block = BlockEnv;
    type Tx = Transaction;
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

impl<DB: Database> ContextSetters for Context<DB> {
    fn set_tx(&mut self, tx: Self::Tx) {
        self.inner.set_tx(tx);
    }

    delegate! {
        to self.inner {
            fn set_block(&mut self, block: Self::Block);
        }
    }
}

/// A convenient trait to convert a `CfgEnv<OpSpecId>` into a `CfgEnv<MegaethSpecId>`.
pub trait IntoMegaethCfgEnv {
    /// Convert to `CfgEnv<MegaethSpecId>`.
    fn into_megaeth_cfg(self, spec: SpecId) -> CfgEnv<SpecId>;
}

/// A convenient trait to convert a `CfgEnv<MegaethSpecId>` into a `CfgEnv<OpSpecId>`.
pub trait IntoOpCfgEnv {
    /// Convert to `CfgEnv<OpSpecId>`.
    fn into_op_cfg(self) -> CfgEnv<OpSpecId>;
}

impl IntoOpCfgEnv for CfgEnv<SpecId> {
    /// Convert to `CfgEnv<OpSpecId>`.
    ///
    /// DEV: when the fields of [`CfgEnv`] changes, you need to update this function.
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

impl IntoMegaethCfgEnv for CfgEnv<OpSpecId> {
    /// Convert to `CfgEnv<MegaethSpecId>`.
    ///
    /// DEV: when the fields of [`CfgEnv`] changes, you need to update this function.
    fn into_megaeth_cfg(self, spec: SpecId) -> CfgEnv<SpecId> {
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
