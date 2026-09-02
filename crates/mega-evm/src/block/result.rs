#[cfg(not(feature = "std"))]
use alloc as std;
use std::boxed::Box;

use alloy_evm::{block::TxResult, InvalidTxError};
use alloy_primitives::TxHash;
use revm::{
    context::result::{InvalidTransaction, ResultAndState},
    state::AccountInfo,
};

use crate::{MegaHaltReason, MegaTransactionOutcome};

/// The execution outcome of a transaction in `MegaETH`.
///
/// This struct contains additional information about the transaction execution on top of the
/// standard EVM's execution result and state.
#[derive(Debug, Clone, derive_more::Deref, derive_more::DerefMut)]
pub struct BlockMegaTransactionOutcome<T> {
    /// The transaction.
    pub tx: T,
    /// The transaction size in bytes.
    pub tx_size: u64,
    /// The transaction data availability size in bytes.
    pub da_size: u64,
    /// The depositor's account as it stood before execution, recorded for the deposit receipt's
    /// nonce.
    ///
    /// Doubles as the deposit signal at commit time: `Some` iff the transaction is a deposit.
    /// The `run_transaction*` producers uphold that (a deposit with an absent depositor account
    /// still records `Some(AccountInfo::default())`); a hand-built value must uphold it too.
    pub depositor: Option<AccountInfo>,
    /// The transaction execution outcome.
    #[deref]
    #[deref_mut]
    pub inner: MegaTransactionOutcome,
}

/// Per-transaction execution result handed to `alloy_evm::block::BlockExecutor`.
///
/// This is the shape [`BlockExecutor::Result`](alloy_evm::block::BlockExecutor) is instantiated
/// with, and an associated type cannot carry the transaction: transactions enter
/// `execute_transaction_without_commit` as a method-level type parameter, which an associated
/// type — fixed once per impl — cannot name. So this type carries the scalars the commit path
/// needs instead: the type byte for the receipt builder, and the hash and gas limit for the
/// commit-time re-run of block-level admission.
///
/// Everything execution itself produced travels as the embedded
/// [`MegaTransactionOutcome`], so a resource dimension added there flows through this type
/// without being re-declared.
#[derive(Debug, Clone)]
pub struct MegaBlockTxResult<T> {
    /// Transaction type, which is what the receipt builder needs from the transaction.
    pub tx_type: T,
    /// The transaction hash, used to attribute a commit-time admission failure to this
    /// transaction.
    pub tx_hash: TxHash,
    /// The transaction's declared gas limit, re-checked against the block's remaining gas at
    /// commit time.
    pub gas_limit: u64,
    /// The transaction size in bytes.
    pub tx_size: u64,
    /// The transaction data availability size in bytes.
    pub da_size: u64,
    /// The depositor's account as it stood before execution, recorded for the deposit receipt's
    /// nonce.
    ///
    /// Doubles as the deposit signal at commit time: `Some` iff the transaction is a deposit.
    /// The `run_transaction*` producers uphold that (a deposit with an absent depositor account
    /// still records `Some(AccountInfo::default())`); a hand-built value must uphold it too.
    pub depositor: Option<AccountInfo>,
    /// The execution result, post-state and resource usage, exactly as execution produced them.
    pub inner: MegaTransactionOutcome,
}

impl<T: Send + 'static> TxResult for MegaBlockTxResult<T> {
    type HaltReason = MegaHaltReason;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.inner.result_and_state
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.inner.result_and_state
    }
}

/// Error type for additional reasons of an invalid transaction. If one transaction is invalid, it
/// will never be able to be included in a block and should be discarded.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MegaTxLimitExceededError {
    /// Transaction gas limit exceeded.
    #[error("Transaction gas limit exceeded: tx_gas_limit={tx_gas_limit} > limit={limit}")]
    TransactionGasLimit {
        /// Transaction gas limit used by current transaction
        tx_gas_limit: u64,
        /// Transaction gas limit limit
        limit: u64,
    },
    /// Transaction encode size limit exceeded.
    #[error("Transaction encode size limit exceeded: tx_size={tx_size} > limit={limit}")]
    TransactionEncodeSizeLimit {
        /// Transaction encode size used by current transaction
        tx_size: u64,
        /// Transaction encode size limit
        limit: u64,
    },

    /// Transaction data availability size limit exceeded.
    #[error(
        "Transaction data availability size limit exceeded: da_size={da_size} > limit={limit}"
    )]
    DataAvailabilitySizeLimit {
        /// Data availability size used by current transaction
        da_size: u64,
        /// Data availability size limit
        limit: u64,
    },
}

impl MegaTxLimitExceededError {
    /// The amount of the resource used by the current transaction.
    pub fn usage(&self) -> u64 {
        match self {
            Self::TransactionGasLimit { tx_gas_limit, .. } => *tx_gas_limit,
            Self::TransactionEncodeSizeLimit { tx_size, .. } => *tx_size,
            Self::DataAvailabilitySizeLimit { da_size, .. } => *da_size,
        }
    }

    /// The limit of the resource.
    pub fn limit(&self) -> u64 {
        match self {
            Self::TransactionGasLimit { limit, .. } |
            Self::TransactionEncodeSizeLimit { limit, .. } |
            Self::DataAvailabilitySizeLimit { limit, .. } => *limit,
        }
    }
}

impl InvalidTxError for MegaTxLimitExceededError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }

    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        // MegaETH's resource-limit errors have no upstream `InvalidTransaction` counterpart.
        None
    }
}

/// Error type for block-level limit exceeded. These errors are thrown when the block has already
/// exceeded its limit (from a previous transaction) and no more transactions can be added.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MegaBlockLimitExceededError {
    /// Block transactions data limit reached.
    #[error("Block transactions data limit reached: block_used={block_used} >= limit={limit}")]
    TransactionDataLimit {
        /// Transaction data used by block so far
        block_used: u64,
        /// Block transactions data limit
        limit: u64,
    },

    /// Block KV update limit reached.
    #[error("Block KV update limit reached: block_used={block_used} >= limit={limit}")]
    KVUpdateLimit {
        /// KV updates used by block so far
        block_used: u64,
        /// Block KV update limit
        limit: u64,
    },

    /// Block compute gas limit reached.
    #[error("Block compute gas limit reached: block_used={block_used} >= limit={limit}")]
    ComputeGasLimit {
        /// Compute gas used by block so far, as the limit measures it.
        ///
        /// This is the enforced reading — the counter that was actually compared — so it excludes
        /// the remainders Rex7+ exceptionally halted frames destroyed. The block's full reported
        /// compute statistic, which includes them, can be higher.
        block_used: u64,
        /// Block compute gas limit
        limit: u64,
    },

    /// Block transactions encode size limit exceeded.
    #[error("Block transactions encode size limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    TransactionEncodeSizeLimit {
        /// Transaction encode size used by block so far
        block_used: u64,
        /// Transaction encode size used by current transaction
        tx_used: u64,
        /// Block transactions encode size limit
        limit: u64,
    },

    /// Block data availability size limit exceeded.
    #[error("Block data availability size limit exceeded: block_used={block_used} + tx_used={tx_used} > limit={limit}")]
    DataAvailabilitySizeLimit {
        /// Data availability size used by block so far
        block_used: u64,
        /// Data availability size used by current transaction
        tx_used: u64,
        /// Block data availability size limit
        limit: u64,
    },

    /// Block state growth limit reached.
    #[error("Block state growth limit reached: block_used={block_used} >= limit={limit}")]
    StateGrowthLimit {
        /// State growth used by block so far
        block_used: u64,
        /// Block state growth limit
        limit: u64,
    },
}

impl MegaBlockLimitExceededError {
    /// The total amount of the resource used by the block so far.
    pub fn block_used(&self) -> u64 {
        match self {
            Self::TransactionDataLimit { block_used, .. } |
            Self::KVUpdateLimit { block_used, .. } |
            Self::ComputeGasLimit { block_used, .. } |
            Self::TransactionEncodeSizeLimit { block_used, .. } |
            Self::DataAvailabilitySizeLimit { block_used, .. } |
            Self::StateGrowthLimit { block_used, .. } => *block_used,
        }
    }

    /// The limit of the resource.
    pub fn limit(&self) -> u64 {
        match self {
            Self::TransactionDataLimit { limit, .. } |
            Self::KVUpdateLimit { limit, .. } |
            Self::ComputeGasLimit { limit, .. } |
            Self::TransactionEncodeSizeLimit { limit, .. } |
            Self::DataAvailabilitySizeLimit { limit, .. } |
            Self::StateGrowthLimit { limit, .. } => *limit,
        }
    }
}

impl InvalidTxError for MegaBlockLimitExceededError {
    fn is_nonce_too_low(&self) -> bool {
        false
    }

    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        // Block-level limit errors have no upstream `InvalidTransaction` counterpart.
        None
    }
}

/// A `MegaETH` block executor's own refusals — the ones that are neither a resource limit nor a
/// transaction the EVM rejected.
///
/// These say the executor was asked to do something it must not do, so they are reported as
/// [`BlockExecutionError::Internal`](alloy_evm::block::BlockExecutionError::Internal) rather than
/// as a verdict about the transaction: the block is not built, and there is nothing about the
/// transaction itself for a caller to fix.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MegaBlockExecutionError {
    /// A transaction reached the canonical block-execution path from an EVM running an inspector
    /// whose type carries no [`TrustedObserver`](crate::TrustedObserver) declaration.
    ///
    /// This is the admission rule, and it is a rule about the *configuration* rather than about
    /// what one run was observed to do. The measurement shim books what an inspector writes across
    /// a callback boundary, but an inspector can reach past that boundary — editing the contents
    /// of the interpreter's stack or memory, or writing the journal directly — and change what the
    /// transaction produces while leaving every lane of
    /// [`InspectorLedger`](crate::InspectorLedger) at zero. An empty ledger therefore cannot be
    /// what a block is admitted on.
    ///
    /// What can is a declaration: a line written in source, about one concrete type, by someone
    /// who had read it. So the canonical path takes an EVM running no inspector, or one whose
    /// inspector was built through
    /// [`MegaEvm::with_trusted_inspector`](crate::MegaEvm::with_trusted_inspector), and refuses
    /// everything else — including an inspector that only observes, because the criterion is what
    /// the type's author declared and not what this run happened to do.
    ///
    /// A tracer keeps working by being declared. `revm-inspectors`' tracers are foreign types and
    /// the trait is local to this crate, so a node writes a forwarding newtype of its own and
    /// declares that; `bin/mega-evme`'s replay command does exactly this. An embedder that wants a
    /// rewriting inspector still has one — [`MegaEvm::execute_transaction`](
    /// crate::MegaEvm::execute_transaction) supports it in full — it just does not get to call the
    /// result a block.
    #[error(
        "transaction {tx_hash} reached the canonical block-execution path from an EVM running an \
         inspector whose type carries no `TrustedObserver` declaration"
    )]
    UndeclaredInspector {
        /// The transaction that was refused.
        tx_hash: TxHash,
    },

    /// A transaction an inspector took part in reached the canonical block-execution path.
    ///
    /// Block production and block validation must produce the same numbers for the same block, on
    /// every node, so what the executor reports has to be what the EVM did — and only that. An
    /// inspector is present on one node's configuration and not on another's, and it can break
    /// that in two ways:
    ///
    /// - by writing gas into an interpreter's counter, into a frame's envelope, or into a
    ///   returning frame's result, which reaches the receipt, the transaction's reported compute
    ///   total, and through it the block's cumulative counters;
    /// - by rewriting what a frame *did* — its classification, its output, or the frame itself
    ///   through a synthetic outcome — which moves no gas at all and reaches the transaction's
    ///   state and its receipt directly.
    ///
    /// Both are refused. The second is why the criterion is the whole ledger rather than its gas
    /// lanes: a rewrite that costs nothing is not a rewrite that changes nothing.
    ///
    /// This is the backstop behind [`UndeclaredInspector`](Self::UndeclaredInspector), not the
    /// admission rule. An undeclared inspector is refused before it runs, so what is left for this
    /// to catch is a declaration that did not hold — which debug builds measure and assert — and a
    /// result that arrives at the commit funnel already carrying a non-zero ledger, produced by
    /// another executor instance, by an embedder driving the EVM itself, or built by hand. The
    /// commit funnel cannot see the EVM that produced such a result; the result's own ledger is
    /// the only thing there that knows anything.
    ///
    /// An embedder that genuinely wants a rewriting inspector still has one —
    /// [`MegaEvm::execute_transaction`](crate::MegaEvm::execute_transaction) supports it in full,
    /// with the ledger reported on the outcome — it just does not get to call the result a block.
    /// That is also what leaves a simulation EVM an embedder drives off the canonical path alone,
    /// however much its inspector rewrites: this guard sits on the block executor's entries, not
    /// on the EVM.
    #[error(
        "transaction {tx_hash} reached the canonical block-execution path after an inspector took \
         part in it: {ledger:?}"
    )]
    InspectorAdjustedAccounting {
        /// The transaction the adjusted accounting belongs to.
        tx_hash: TxHash,
        /// What the measurement shim booked for that transaction.
        ///
        /// Boxed: the ledger is six signed lanes and two counters, which is most of this enum's
        /// size, and every value of this type is boxed again into
        /// [`BlockExecutionError::Internal`](alloy_evm::block::BlockExecutionError::Internal) the
        /// moment it is built.
        ledger: Box<crate::InspectorLedger>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-and-run pin for the deref chain downstream code reads through.
    ///
    /// `BlockMegaTransactionOutcome` derefs to [`MegaTransactionOutcome`], which derefs to
    /// [`ResultAndState`] — so `outcome.result` / `outcome.state` resolve through two hops and
    /// the resource dimensions through one. mega-reth reads exactly these shapes (borrows,
    /// clones and `Copy` scalars; it never constructs the type and never moves a non-`Copy`
    /// field out). Restructuring the deref chain turns this test red before it turns a
    /// downstream build red.
    #[test]
    fn test_outcome_deref_chain_carries_downstream_access_shapes() {
        use crate::MegaTransactionOutcome;
        use revm::{
            context::result::{ExecutionResult, Output, ResultGas, SuccessReason},
            state::EvmState,
        };

        fn borrows_state(_: &EvmState) {}

        let inner = MegaTransactionOutcome {
            result_and_state: ResultAndState {
                result: ExecutionResult::<MegaHaltReason>::Success {
                    reason: SuccessReason::Stop,
                    gas: ResultGas::default(),
                    logs: Vec::new(),
                    output: Output::Call(alloy_primitives::Bytes::new()),
                },
                state: EvmState::default(),
            },
            data_size: 1,
            kv_updates: 2,
            compute_gas_used: 3,
            compute_gas_destroyed: 1,
            compute_gas_enforced: 2,
            state_growth_used: 4,
            inspector_ledger: crate::InspectorLedger::default(),
            undeclared_inspector: false,
        };

        // One hop: MegaTransactionOutcome -> ResultAndState.
        assert!(inner.result.is_success());
        borrows_state(&inner.state);
        let _cloned: ExecutionResult<MegaHaltReason> = inner.result.clone();

        let mut outcome =
            BlockMegaTransactionOutcome { tx: (), tx_size: 0, da_size: 0, depositor: None, inner };

        // Two hops: BlockMegaTransactionOutcome -> MegaTransactionOutcome -> ResultAndState.
        assert!(outcome.result.is_success());
        borrows_state(&outcome.state);
        let _cloned: ExecutionResult<MegaHaltReason> = outcome.result.clone();
        let _state: EvmState = outcome.state.clone();
        let _mutable: &mut EvmState = &mut outcome.state;

        // One hop for the resource dimensions (`Copy` scalars may leave through a deref).
        let kv: u64 = outcome.kv_updates;
        assert_eq!((kv, outcome.compute_gas_used, outcome.state_growth_used), (2, 3, 4));
        assert_eq!((outcome.compute_gas_destroyed, outcome.compute_gas_enforced), (1, 2));
    }

    #[test]
    fn test_transaction_limit_error_reports_usage_and_limit() {
        let cases = [
            (MegaTxLimitExceededError::TransactionGasLimit { tx_gas_limit: 31, limit: 30 }, 31, 30),
            (
                MegaTxLimitExceededError::TransactionEncodeSizeLimit { tx_size: 101, limit: 100 },
                101,
                100,
            ),
            (
                MegaTxLimitExceededError::DataAvailabilitySizeLimit { da_size: 11, limit: 10 },
                11,
                10,
            ),
        ];

        for (error, expected_usage, expected_limit) in cases {
            assert_eq!(error.usage(), expected_usage);
            assert_eq!(error.limit(), expected_limit);
            assert!(!error.is_nonce_too_low());
        }
    }

    #[test]
    fn test_block_limit_error_reports_block_usage_and_limit() {
        let cases = [
            (MegaBlockLimitExceededError::TransactionDataLimit { block_used: 1, limit: 10 }, 1, 10),
            (MegaBlockLimitExceededError::KVUpdateLimit { block_used: 2, limit: 11 }, 2, 11),
            (MegaBlockLimitExceededError::ComputeGasLimit { block_used: 3, limit: 12 }, 3, 12),
            (
                MegaBlockLimitExceededError::TransactionEncodeSizeLimit {
                    block_used: 4,
                    tx_used: 1,
                    limit: 13,
                },
                4,
                13,
            ),
            (
                MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                    block_used: 5,
                    tx_used: 1,
                    limit: 14,
                },
                5,
                14,
            ),
            (MegaBlockLimitExceededError::StateGrowthLimit { block_used: 6, limit: 15 }, 6, 15),
        ];

        for (error, expected_block_used, expected_limit) in cases {
            assert_eq!(error.block_used(), expected_block_used);
            assert_eq!(error.limit(), expected_limit);
            assert!(!error.is_nonce_too_low());
        }
    }

    /// A `MegaETH` resource-limit rejection has no upstream `InvalidTransaction` counterpart, so
    /// both error types answer `as_invalid_tx_err` with `None` for every variant. reth-side
    /// classification consumes this answer as "drop this transaction", not "abort payload
    /// building" — the mega-reth integration owns pinning that reading; this pins the answer.
    #[test]
    fn test_limit_errors_have_no_invalid_transaction_counterpart() {
        let tx_errors = [
            MegaTxLimitExceededError::TransactionGasLimit { tx_gas_limit: 31, limit: 30 },
            MegaTxLimitExceededError::TransactionEncodeSizeLimit { tx_size: 101, limit: 100 },
            MegaTxLimitExceededError::DataAvailabilitySizeLimit { da_size: 11, limit: 10 },
        ];
        for error in tx_errors {
            assert!(error.as_invalid_tx_err().is_none(), "no counterpart for {error}");
        }

        let block_errors = [
            MegaBlockLimitExceededError::TransactionDataLimit { block_used: 1, limit: 10 },
            MegaBlockLimitExceededError::KVUpdateLimit { block_used: 2, limit: 11 },
            MegaBlockLimitExceededError::ComputeGasLimit { block_used: 3, limit: 12 },
            MegaBlockLimitExceededError::TransactionEncodeSizeLimit {
                block_used: 4,
                tx_used: 1,
                limit: 13,
            },
            MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                block_used: 5,
                tx_used: 1,
                limit: 14,
            },
            MegaBlockLimitExceededError::StateGrowthLimit { block_used: 6, limit: 15 },
        ];
        for error in block_errors {
            assert!(error.as_invalid_tx_err().is_none(), "no counterpart for {error}");
        }
    }
}
