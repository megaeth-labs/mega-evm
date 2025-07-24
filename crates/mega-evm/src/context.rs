use alloy_evm::Database;
use delegate::delegate;
use op_revm::{DefaultOp, L1BlockInfo, OpContext, OpSpecId};
use revm::{
    context::{BlockEnv, CfgEnv, ContextSetters, ContextTr},
    context_interface::context::ContextError,
    Journal,
};

use crate::{constants, SpecId, Transaction};

/// `MegaETH` EVM context type.
#[derive(Debug, derive_more::Deref, derive_more::DerefMut)]
pub struct Context<DB: Database> {
    #[deref]
    #[deref_mut]
    pub(crate) inner: OpContext<DB>,
    /// The `MegaETH` spec id.
    /// The `inner` context uses `OpSpecId`, which should be converted from `MegaethSpecId`
    /// when creating the context. The consistency between the spec here and `inner` context
    /// should be maintained and guaranteed by the caller.
    spec: SpecId,

    /* Internal state variables */
    /// The total size of all log data.
    pub(crate) log_data_size: u64,
}

impl<DB: Database> Context<DB> {
    /// Create a new `MegaethContext` with the given database.
    pub fn new(db: DB, spec: SpecId) -> Self {
        let mut inner =
            revm::Context::op().with_db(db).with_cfg(CfgEnv::new_with_spec(spec.into_op_spec()));

        if spec.is_enabled_in(SpecId::MINI_REX) && inner.cfg.limit_contract_code_size.is_none() {
            inner.cfg.limit_contract_code_size = Some(constants::mini_rex::MAX_CONTRACT_SIZE);
        }

        Self { inner, spec, log_data_size: 0 }
    }

    /// Set the database.
    pub fn with_db<ODB: Database>(self, db: ODB) -> Context<ODB> {
        Context {
            inner: self.inner.with_db(db),
            spec: self.spec,
            log_data_size: self.log_data_size,
        }
    }

    /// Set the transaction.
    pub fn with_tx(mut self, tx: Transaction) -> Self {
        self.inner = self.inner.with_tx(tx);
        self
    }

    /// Set the block.
    pub fn with_block(mut self, block: BlockEnv) -> Self {
        self.inner = self.inner.with_block(block);
        self
    }

    /// Set the configuration.
    pub fn with_cfg(mut self, cfg: CfgEnv<SpecId>) -> Self {
        self.inner = self.inner.with_cfg(cfg.into_op_cfg());
        if self.spec.is_enabled_in(SpecId::MINI_REX) &&
            self.inner.cfg.limit_contract_code_size.is_none()
        {
            self.inner.cfg.limit_contract_code_size = Some(constants::mini_rex::MAX_CONTRACT_SIZE);
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
}

impl<DB: Database> ContextTr for Context<DB> {
    type Block = BlockEnv;
    type Tx = Transaction;
    type Cfg = CfgEnv<OpSpecId>;
    type Db = DB;
    type Journal = Journal<DB>;
    type Chain = L1BlockInfo;

    delegate! {
        to self.inner {
            fn tx(&self) -> &Self::Tx;
            fn block(&self) -> &Self::Block;
            fn cfg(&self) -> &Self::Cfg;
            fn journal(&mut self) -> &mut Self::Journal;
            fn journal_ref(&self) -> &Self::Journal;
            fn db(&mut self) -> &mut Self::Db;
            fn db_ref(&self) -> &Self::Db;
            fn chain(&mut self) -> &mut Self::Chain;
            fn error(&mut self) -> &mut Result<(), ContextError<<Self::Db as revm::Database>::Error>>;
            fn tx_journal(&mut self) -> (&mut Self::Tx, &mut Self::Journal);
        }
    }
}

impl<DB: Database> ContextSetters for Context<DB> {
    delegate! {
        to self.inner {
            fn set_tx(&mut self, tx: Self::Tx);
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
    fn into_op_cfg(self) -> CfgEnv<OpSpecId> {
        let mut op_cfg = CfgEnv::new_with_spec(OpSpecId::from(self.spec));
        op_cfg.chain_id = self.chain_id;
        op_cfg.limit_contract_code_size = self.limit_contract_code_size;
        op_cfg.disable_nonce_check = self.disable_nonce_check;
        op_cfg.blob_target_and_max_count = self.blob_target_and_max_count;
        op_cfg.memory_limit = self.memory_limit;
        op_cfg.disable_balance_check = self.disable_balance_check;
        op_cfg.disable_block_gas_limit = self.disable_block_gas_limit;
        op_cfg.disable_eip3607 = self.disable_eip3607;
        op_cfg.disable_base_fee = self.disable_base_fee;
        op_cfg
    }
}

impl IntoMegaethCfgEnv for CfgEnv<OpSpecId> {
    fn into_megaeth_cfg(self, spec: SpecId) -> CfgEnv<SpecId> {
        let mut cfg = CfgEnv::new_with_spec(spec);
        cfg.chain_id = self.chain_id;
        cfg.limit_contract_code_size = self.limit_contract_code_size;
        cfg.disable_nonce_check = self.disable_nonce_check;
        cfg.blob_target_and_max_count = self.blob_target_and_max_count;
        cfg.memory_limit = self.memory_limit;
        cfg.disable_balance_check = self.disable_balance_check;
        cfg.disable_block_gas_limit = self.disable_block_gas_limit;
        cfg.disable_eip3607 = self.disable_eip3607;
        cfg.disable_base_fee = self.disable_base_fee;
        cfg
    }
}
