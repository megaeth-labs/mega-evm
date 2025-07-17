use alloy_evm::Database;
use delegate::delegate;
use op_revm::{DefaultOp, L1BlockInfo, OpContext, OpSpecId};
use revm::{
    context::{BlockEnv, CfgEnv, ContextSetters, ContextTr},
    context_interface::context::ContextError,
    Context, Journal,
};

use crate::{MegaethSpecId, MegaethTransaction};

/// `MegaETH` EVM context type.
#[derive(Debug, derive_more::Deref, derive_more::DerefMut)]
pub struct MegaethContext<DB: Database> {
    #[deref]
    #[deref_mut]
    inner: OpContext<DB>,
    /// The `MegaETH` spec id.
    /// The `inner` context uses `OpSpecId`, which should be converted from `MegaethSpecId`
    /// when creating the context. The consistency between the spec here and `inner` context
    /// should be maintained and guaranteed by the caller.
    spec: MegaethSpecId,
}

impl<DB: Database> MegaethContext<DB> {
    /// Create a new `MegaethContext` with the given database.
    pub fn new(db: DB, spec: MegaethSpecId) -> Self {
        Self {
            inner: Context::op().with_db(db),
            spec,
        }
    }

    /// Set the database.
    pub fn with_db<ODB: Database>(self, db: ODB) -> MegaethContext<ODB> {
        MegaethContext {
            inner: self.inner.with_db(db),
            spec: self.spec,
        }
    }

    /// Set the transaction.
    pub fn with_tx(self, tx: MegaethTransaction) -> Self {
        Self {
            inner: self.inner.with_tx(tx),
            spec: self.spec,
        }
    }

    /// Set the block.
    pub fn with_block(self, block: BlockEnv) -> Self {
        Self {
            inner: self.inner.with_block(block),
            spec: self.spec,
        }
    }

    /// Set the configuration.
    pub fn with_cfg(self, cfg: CfgEnv<MegaethSpecId>) -> Self {
        Self {
            inner: self.inner.with_cfg(cfg.into_op_cfg()),
            spec: self.spec,
        }
    }

    /// Set the chain.
    pub fn with_chain(self, chain: L1BlockInfo) -> Self {
        Self {
            inner: self.inner.with_chain(chain),
            spec: self.spec,
        }
    }

    /// Get the `MegaETH` spec id. This value should be consistent with the `spec` field by
    /// coverting this value to `OpSpecId`.
    pub fn spec(&self) -> MegaethSpecId {
        self.spec
    }

    /// Convert the `MegaethContext` into the inner `OpContext`.
    pub fn into_inner(self) -> OpContext<DB> {
        self.inner
    }
}

impl<DB: Database> ContextTr for MegaethContext<DB> {
    type Block = BlockEnv;
    type Tx = MegaethTransaction;
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

impl<DB: Database> ContextSetters for MegaethContext<DB> {
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
    fn into_megaeth_cfg(self, spec: MegaethSpecId) -> CfgEnv<MegaethSpecId>;
}

/// A convenient trait to convert a `CfgEnv<MegaethSpecId>` into a `CfgEnv<OpSpecId>`.
pub trait IntoOpCfgEnv {
    /// Convert to `CfgEnv<OpSpecId>`.
    fn into_op_cfg(self) -> CfgEnv<OpSpecId>;
}

impl IntoOpCfgEnv for CfgEnv<MegaethSpecId> {
    fn into_op_cfg(self) -> CfgEnv<OpSpecId> {
        let mut op_cfg = CfgEnv::new_with_spec(OpSpecId::from(self.spec));
        op_cfg.chain_id = self.chain_id;
        op_cfg.limit_contract_code_size = self.limit_contract_code_size;
        op_cfg.disable_nonce_check = self.disable_nonce_check;
        op_cfg.blob_target_and_max_count = self.blob_target_and_max_count;
        op_cfg
    }
}

impl IntoMegaethCfgEnv for CfgEnv<OpSpecId> {
    fn into_megaeth_cfg(self, spec: MegaethSpecId) -> CfgEnv<MegaethSpecId> {
        let mut cfg = CfgEnv::new_with_spec(spec);
        cfg.chain_id = self.chain_id;
        cfg.limit_contract_code_size = self.limit_contract_code_size;
        cfg.disable_nonce_check = self.disable_nonce_check;
        cfg.blob_target_and_max_count = self.blob_target_and_max_count;
        cfg
    }
}
