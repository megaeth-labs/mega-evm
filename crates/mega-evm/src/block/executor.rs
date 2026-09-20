//! Block executor of the Satin engine.
//!
//! [`MegaBlockExecutor`] is alloy-evm's [`BlockExecutor`] over a [`MegaEvm`]: it runs the block's
//! transactions, holds them to the block's limits, builds their receipts and counts what the
//! block spent on each of the three gas ledgers.
//!
//! # The block rules of the Karst base
//!
//! - **An activation block admits only deposits.** The caller sets
//!   [`no_user_tx_activation_block`](MegaBlockExecutionCtx::no_user_tx_activation_block) from the
//!   chain's schedule ([`MegaHardforks::admits_only_deposits`]), and a user transaction in such a
//!   block is refused before it executes. This mirrors alloy-op-evm's flag of the same name.
//! - **The data-availability footprint is a block limit.** Each non-deposit transaction's footprint
//!   is its compressed size times the footprint gas scalar the L1 block contract holds, accumulated
//!   against the block's gas limit and reported as the block's blob gas used. With no scalar in
//!   state the scalar is zero and the rule costs nothing. This mirrors alloy-op-evm's Jovian DA
//!   footprint block limit.
//! - **The L1 block info is read once, at the start of the block.** An empty L1 block contract
//!   reads as zeroes, not as an error, and a caller that placed its own info for this block keeps
//!   it. This mirrors alloy-op-evm's `l1_block_info`.
//!
//! # What later mechanisms fill in
//!
//! [`apply_pre_execution_changes`](BlockExecutor::apply_pre_execution_changes) names two hook
//! points that are empty today: system contract deployment, and the pre-block system calls.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::fmt;
use std::{boxed::Box, vec::Vec};

use alloy_consensus::{Eip658Value, Header, Transaction, TransactionEnvelope, TxReceipt};
use alloy_eips::{eip7685::Requests, Encodable2718, Typed2718};
use alloy_evm::{
    block::{
        state_changes::post_block_balance_increments, BlockExecutionError, BlockExecutionResult,
        BlockExecutor, CommitChanges, ExecutableTx, GasOutput, StateDB,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
    Database, Evm, FromRecoveredTx, FromTxWithEncoded, RecoveredTx,
};
use alloy_op_evm::block::{receipt_builder::OpReceiptBuilder, OpTxEnv};
use alloy_primitives::{Bytes, B256};
use op_alloy_consensus::OpDepositReceipt;
use op_revm::{
    constants::L1_BLOCK_CONTRACT, transaction::deposit::DEPOSIT_TRANSACTION_TYPE, L1BlockInfo,
};
use revm::{
    context::{result::ResultAndState, Block, ContextTr},
    database::DatabaseCommitExt,
    state::EvmState,
    DatabaseCommit, Inspector,
};

/// What [`MegaBlockExecutor::finish_with_counters`] hands back: the EVM the block ran on, and
/// what the block produced and counted.
pub type MegaFinishedBlock<DB, INSP, ExtEnvs, R> =
    (MegaEvm<DB, INSP, ExtEnvs>, MegaBlockExecutionResult<<R as OpReceiptBuilder>::Receipt>);

use crate::{
    block::eips, estimated_da_size, BlockGasCounters, BlockLimiter, BlockLimits, ExternalEnvTypes,
    MegaBlockExecutionResult, MegaBlockTxResult, MegaContext, MegaEvm, MegaHardforks,
    MegaTransaction,
};

/// What the node hands block execution beside the EVM.
///
/// It carries what the pre-block calls need (the parent hash and the parent beacon block root, as
/// alloy-op-evm does), the block's own extra data, the activation-block flag and the limits the
/// block holds its transactions to.
#[derive(Clone, Debug, Default)]
pub struct MegaBlockExecutionCtx {
    /// The parent block's hash, which the EIP-2935 pre-block call records.
    pub parent_hash: B256,
    /// The parent block's beacon block root, which the EIP-4788 pre-block call records.
    pub parent_beacon_block_root: Option<B256>,
    /// The block's extra data.
    pub extra_data: Bytes,
    /// Whether this block admits only deposit transactions because a fork activates in it.
    ///
    /// The caller sets it from the chain's schedule with
    /// [`MegaHardforks::admits_only_deposits`], which needs the parent block's timestamp; `false`
    /// skips the rule.
    pub no_user_tx_activation_block: bool,
    /// The limits this block holds its transactions to.
    pub block_limits: BlockLimits,
}

impl MegaBlockExecutionCtx {
    /// A context for a block that activates no fork.
    pub const fn new(
        parent_hash: B256,
        parent_beacon_block_root: Option<B256>,
        extra_data: Bytes,
        block_limits: BlockLimits,
    ) -> Self {
        Self {
            parent_hash,
            parent_beacon_block_root,
            extra_data,
            no_user_tx_activation_block: false,
            block_limits,
        }
    }

    /// Marks this block one that admits only deposit transactions.
    pub const fn with_no_user_tx_activation_block(mut self, activation_block: bool) -> Self {
        self.no_user_tx_activation_block = activation_block;
        self
    }
}

/// A block this engine refuses to execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MegaBlockExecutionError {
    /// A block that activates a fork carries a transaction that is not a deposit.
    UnexpectedNonDepositTxInActivationBlock,
    /// A transaction's data-availability footprint does not fit in what the block has left.
    TransactionDaFootprintAboveGasLimit {
        /// The footprint of the transaction that does not fit.
        transaction_da_footprint: u64,
        /// The footprint the block has left.
        available_block_da_footprint: u64,
    },
    /// The EVM runs an inspector that may rewrite what execution produces, which block execution
    /// does not admit.
    RewritingInspector,
}

impl fmt::Display for MegaBlockExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedNonDepositTxInActivationBlock => {
                f.write_str("unexpected non-deposit transaction in fork activation block")
            }
            Self::TransactionDaFootprintAboveGasLimit {
                transaction_da_footprint,
                available_block_da_footprint,
            } => write!(
                f,
                "transaction DA footprint exceeds available block DA footprint. \
                 transaction_da_footprint: {transaction_da_footprint}, \
                 available_block_da_footprint: {available_block_da_footprint}"
            ),
            Self::RewritingInspector => f.write_str(
                "block execution does not admit an inspector that may rewrite execution",
            ),
        }
    }
}

impl core::error::Error for MegaBlockExecutionError {}

impl From<MegaBlockExecutionError> for BlockExecutionError {
    fn from(error: MegaBlockExecutionError) -> Self {
        Self::Validation(alloy_evm::block::BlockValidationError::Other(Box::new(error)))
    }
}

/// Executes the transactions of one block on a Satin EVM.
#[derive(Debug)]
pub struct MegaBlockExecutor<E, R: OpReceiptBuilder, Spec> {
    /// The chain's hardfork schedule.
    pub spec: Spec,
    /// The receipt builder.
    pub receipt_builder: R,
    /// What the node handed this block.
    pub ctx: MegaBlockExecutionCtx,
    /// The EVM the block runs on.
    pub evm: E,
    /// The receipts of the transactions committed so far.
    pub receipts: Vec<R::Receipt>,
    /// What the block has used, and the limits it is held to.
    pub limiter: BlockLimiter,
    /// Whether Canyon is active, which decides whether a deposit receipt carries a version.
    is_canyon: bool,
    /// Whether Regolith is active, which decides whether a deposit receipt carries a nonce.
    is_regolith: bool,
}

impl<E, R: OpReceiptBuilder, Spec> MegaBlockExecutor<E, R, Spec> {
    /// The EVM the block runs on.
    pub const fn evm(&self) -> &E {
        &self.evm
    }

    /// The EVM the block runs on, mutably.
    pub const fn evm_mut(&mut self) -> &mut E {
        &mut self.evm
    }

    /// Consumes the executor and returns its EVM.
    pub fn into_evm(self) -> E {
        self.evm
    }

    /// What the block has used, and the limits it is held to.
    pub const fn limiter(&self) -> &BlockLimiter {
        &self.limiter
    }

    /// The gas the block's transactions have spent so far, by ledger.
    pub const fn gas(&self) -> &BlockGasCounters {
        &self.limiter.gas
    }
}

impl<E: Evm, R: OpReceiptBuilder, Spec: MegaHardforks> MegaBlockExecutor<E, R, Spec> {
    /// Creates an executor that runs `ctx`'s block on `evm`.
    ///
    /// The block's gas limit comes from the block environment, whatever `ctx` carries: it is the
    /// number consensus holds the block to, and it is also the budget the data-availability
    /// footprint of the block's transactions is held to.
    pub fn new(evm: E, ctx: MegaBlockExecutionCtx, spec: Spec, receipt_builder: R) -> Self {
        let timestamp = evm.block().timestamp().saturating_to();
        let limits = ctx.block_limits.with_block_gas_limit(evm.block().gas_limit());
        Self {
            is_canyon: spec.is_canyon_active_at_timestamp(timestamp),
            is_regolith: spec.is_regolith_active_at_timestamp(timestamp),
            spec,
            receipt_builder,
            ctx,
            evm,
            receipts: Vec::new(),
            limiter: limits.to_block_limiter(),
        }
    }
}

impl<DB, INSP, ExtEnvs, R, Spec> MegaBlockExecutor<MegaEvm<DB, INSP, ExtEnvs>, R, Spec>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
    ExtEnvs: ExternalEnvTypes,
    R: OpReceiptBuilder,
    Spec: MegaHardforks,
{
    /// The inspector the EVM runs.
    pub const fn inspector(&self) -> &INSP {
        self.evm.inspector()
    }

    /// Reads the L1 block info of this block from state, unless the EVM already carries the info
    /// of this very block.
    ///
    /// An empty L1 block contract reads as zeroes — no fee scalars, no data-availability
    /// footprint scalar — which is a chain that has not set them, not an error. A caller that
    /// placed its own info for this block number keeps it: this is the same condition op-revm's
    /// handler reloads on, so the two never disagree about which info the block runs with.
    fn load_l1_block_info(&mut self) -> Result<(), BlockExecutionError> {
        let block_number = self.evm.block().number();
        if self.evm.ctx().chain().l2_block == Some(block_number) {
            return Ok(());
        }
        let spec = self.evm.ctx().cfg().spec;
        let info = L1BlockInfo::try_fetch(self.evm.ctx_mut().db_mut(), block_number, spec)
            .map_err(BlockExecutionError::other)?;
        self.evm.ctx_mut().modify_chain(|chain| *chain = info);
        Ok(())
    }

    /// The data-availability footprint gas scalar the L1 block contract holds.
    ///
    /// Read per transaction, as the fork's rule is stated: the block's own L1 info transaction
    /// may set it, and the transactions after it are held to the value it set.
    fn da_footprint_gas_scalar(&mut self) -> Result<u64, BlockExecutionError> {
        // Load the L1 block account into the cache first; a database that has never seen it
        // cannot serve its storage.
        let db = self.evm.ctx_mut().db_mut();
        db.basic(L1_BLOCK_CONTRACT).map_err(BlockExecutionError::other)?;
        L1BlockInfo::fetch_da_footprint_gas_scalar(db)
            .map(u64::from)
            .map_err(BlockExecutionError::other)
    }

    /// Commits `state` to the database.
    fn commit(&mut self, state: EvmState) {
        self.evm.ctx_mut().db_mut().commit(state);
    }
}

impl<DB, INSP, ExtEnvs, R, Spec> BlockExecutor
    for MegaBlockExecutor<MegaEvm<DB, INSP, ExtEnvs>, R, Spec>
where
    DB: Database + StateDB,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
    ExtEnvs: ExternalEnvTypes,
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    Spec: MegaHardforks,
    MegaTransaction: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
{
    type Transaction = R::Transaction;
    type Receipt = R::Receipt;
    type Evm = MegaEvm<DB, INSP, ExtEnvs>;
    type Result = MegaBlockTxResult<<R::Transaction as TransactionEnvelope>::TxType>;

    /// Runs what a block does before its transactions.
    ///
    /// In order: the admission gate, the EIP-2935 and EIP-4788 pre-block calls, and the read of
    /// the L1 block info. Each call's state is committed here rather than inside its helper, so
    /// a witness generator sees every step's read and write set.
    ///
    /// Two hook points are empty: system contract deployment, which deploys the chain's system
    /// contracts at the Satin activation, and the pre-block system calls, which apply the
    /// pending changes the sequencer registry holds. Both arrive with the mechanisms of those
    /// names, between the pre-block calls and the L1 block info read.
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        if self.evm.has_rewriting_inspector() {
            return Err(MegaBlockExecutionError::RewritingInspector.into());
        }

        let state = eips::transact_blockhashes_contract_call(
            &self.spec,
            self.ctx.parent_hash,
            &mut self.evm,
        )?;
        if let Some(ResultAndState { state, .. }) = state {
            self.commit(state);
        }

        let state = eips::transact_beacon_root_contract_call(
            &self.spec,
            self.ctx.parent_beacon_block_root,
            &mut self.evm,
        )?;
        if let Some(ResultAndState { state, .. }) = state {
            self.commit(state);
        }

        // Hook point: system contract deployment.
        // Hook point: the pre-block system calls.

        self.load_l1_block_info()?;

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, tx) = tx.into_parts();
        let inner = tx.tx();
        let is_deposit = inner.ty() == DEPOSIT_TRANSACTION_TYPE;

        // A block that activates a fork carries the chain's own transactions only, so a user
        // transaction in it is refused before anything runs.
        if self.ctx.no_user_tx_activation_block && !is_deposit {
            return Err(MegaBlockExecutionError::UnexpectedNonDepositTxInActivationBlock.into());
        }

        // The encoding, once: the transaction environment carries it when the node passes an
        // encoded transaction, and it is encoded here when it does not.
        let da_size = tx_env.encoded_bytes().map_or_else(
            || estimated_da_size(inner.encoded_2718().as_ref()),
            |encoded| estimated_da_size(encoded),
        );
        let tx_size = inner.encode_2718_len() as u64;
        let gas_limit = inner.gas_limit();
        let tx_hash = inner.trie_hash();

        self.limiter.pre_execution_check(tx_hash, gas_limit, tx_size, da_size, is_deposit)?;

        // A deposit is exempt from the data-availability footprint of the block, as it is from
        // its data-availability size.
        let da_footprint = if is_deposit {
            0
        } else {
            let footprint = da_size.saturating_mul(self.da_footprint_gas_scalar()?);
            let available = self.limiter.available_da_footprint();
            if footprint > available {
                return Err(MegaBlockExecutionError::TransactionDaFootprintAboveGasLimit {
                    transaction_da_footprint: footprint,
                    available_block_da_footprint: available,
                }
                .into());
            }
            footprint
        };

        // Read before execution, so committing the transaction cannot fail.
        let depositor_nonce = if is_deposit && self.is_regolith {
            let sender = *tx.signer();
            Some(
                self.evm
                    .ctx_mut()
                    .db_mut()
                    .basic(sender)
                    .map_err(BlockExecutionError::other)?
                    .unwrap_or_default()
                    .nonce,
            )
        } else {
            None
        };

        let outcome = self
            .evm
            .execute_transaction(tx_env)
            .map_err(|err| BlockExecutionError::evm(err, tx_hash))?;

        Ok(MegaBlockTxResult {
            tx_type: inner.tx_type(),
            tx_hash,
            gas_limit,
            tx_size,
            da_size,
            da_footprint,
            is_deposit,
            depositor_nonce,
            inner: outcome,
        })
    }

    /// Executes `tx` and commits it if `f` says so.
    ///
    /// The block's counters may have moved between the transaction executing and its commit — a
    /// builder that executes candidates and then picks among them — so what the transaction adds
    /// is checked against the block once more before it is committed.
    fn execute_transaction_with_commit_condition(
        &mut self,
        tx: impl ExecutableTx<Self>,
        f: impl FnOnce(&Self::Result) -> CommitChanges,
    ) -> Result<Option<GasOutput>, BlockExecutionError> {
        let output = self.execute_transaction_without_commit(tx)?;

        if !f(&output).should_commit() {
            return Ok(None);
        }

        self.limiter.pre_execution_check(
            output.tx_hash,
            output.gas_limit,
            output.tx_size,
            output.da_size,
            output.is_deposit,
        )?;

        Ok(Some(self.commit_transaction(output)))
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.limiter.post_execution_update(&output.block_usage());

        let MegaBlockTxResult { tx_type, is_deposit, depositor_nonce, inner, .. } = output;
        let gas = inner.gas;
        let cumulative_gas_used = self.limiter.block_gas_used;
        let ResultAndState { result, state } = inner.result_and_state;

        self.receipts.push(
            match self.receipt_builder.build_receipt(ReceiptBuilderCtx {
                tx_type,
                result,
                cumulative_gas_used,
                evm: &self.evm,
                state: &state,
            }) {
                Ok(receipt) => receipt,
                Err(ctx) => {
                    let receipt = alloy_consensus::Receipt {
                        // EIP-658 put the status code in the receipt.
                        status: Eip658Value::Eip658(ctx.result.is_success()),
                        cumulative_gas_used,
                        logs: ctx.result.into_logs(),
                    };

                    self.receipt_builder.build_deposit_receipt(OpDepositReceipt {
                        inner: receipt,
                        deposit_nonce: depositor_nonce,
                        // Canyon introduced the deposit receipt version, which says how the
                        // receipt hash is computed.
                        deposit_receipt_version: (is_deposit && self.is_canyon).then_some(1),
                    })
                }
            },
        );

        self.commit(state);

        GasOutput::with_state_gas(gas.gas_used, gas.state)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        let (evm, result) = self.finish_with_counters()?;
        Ok((evm, result.inner))
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }
}

impl<DB, INSP, ExtEnvs, R, Spec> MegaBlockExecutor<MegaEvm<DB, INSP, ExtEnvs>, R, Spec>
where
    DB: Database + StateDB,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
    ExtEnvs: ExternalEnvTypes,
    R: OpReceiptBuilder<Transaction: Transaction + Encodable2718, Receipt: TxReceipt>,
    Spec: MegaHardforks,
    MegaTransaction: FromRecoveredTx<R::Transaction> + FromTxWithEncoded<R::Transaction>,
{
    /// Finishes the block and reports what it counted, on top of what
    /// [`finish`](BlockExecutor::finish) returns.
    ///
    /// The block's gas used is the sum of its receipts, as it has always been; the three ledgers
    /// and the data-size and write-record counts ride beside it, where no upstream type has a
    /// place for them. The blob gas used is the block's data-availability footprint.
    pub fn finish_with_counters(
        mut self,
    ) -> Result<MegaFinishedBlock<DB, INSP, ExtEnvs, R>, BlockExecutionError> {
        let balance_increments =
            post_block_balance_increments::<Header>(&self.spec, self.evm.block(), &[], None);
        self.evm
            .ctx_mut()
            .db_mut()
            .increment_balances(balance_increments)
            .map_err(|_| alloy_evm::block::BlockValidationError::IncrementBalanceFailed)?;

        let BlockLimiter { gas, usage, block_gas_used, block_da_footprint_used, .. } = self.limiter;
        Ok((
            self.evm,
            MegaBlockExecutionResult {
                inner: BlockExecutionResult {
                    receipts: self.receipts,
                    requests: Requests::default(),
                    gas_used: block_gas_used,
                    blob_gas_used: block_da_footprint_used,
                },
                gas,
                usage,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_utils::MemoryDatabase, MegaContext, MegaEvm, MegaHardforkConfig, MegaSpecId};
    use alloy_evm::Evm;
    use revm::context::BlockEnv;

    #[test]
    fn test_executor_hands_back_its_evm() {
        let ctx = MegaContext::new(MemoryDatabase::default(), MegaSpecId::SATIN)
            .with_block(BlockEnv { gas_limit: 30_000_000, ..Default::default() });
        let mut executor = MegaBlockExecutor::new(
            MegaEvm::new(ctx),
            MegaBlockExecutionCtx::default(),
            MegaHardforkConfig::default().with_all_activated(),
            alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder::default(),
        );

        assert_eq!(executor.evm().block().gas_limit, 30_000_000);
        assert_eq!(executor.limiter().limits.block_gas_limit, 30_000_000);
        executor.evm_mut().set_inspector_enabled(true);
        assert!(executor.into_evm().is_inspecting());
    }
}
