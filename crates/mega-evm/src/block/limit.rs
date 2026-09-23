//! What a block admits, and what it counts of the transactions it packed.
//!
//! [`BlockLimits`] is the configuration a node passes in the block execution context;
//! [`BlockLimiter`] is the state one block keeps while it executes. Data size defaults to the
//! production caps — [`BLOCK_DATA_LIMIT`](crate::constants::BLOCK_DATA_LIMIT) for the block and
//! [`TX_DATA_LIMIT`](crate::constants::TX_DATA_LIMIT) for each transaction — and every other
//! limit defaults to unlimited. [`BlockLimits::no_limits`] clears the data-size caps too.
//!
//! # When each limit is checked
//!
//! A limit known before execution is checked before the transaction runs, and refuses it:
//! its own gas limit, encoded size and data-availability size, and what each of those would add
//! to the block.
//!
//! A limit only known after execution — the execution ledger, the state ledger and the data-size
//! bytes a transaction kept — is accumulated when the transaction commits. The transaction that
//! crosses such a limit is therefore still packed, and the ones after it are refused; this is what
//! keeps a block full rather than dropping the work already done. A block whose counter has
//! reached its limit refuses what comes after it, so the overshoot is bounded by one transaction
//! per dimension, besides what deposits add:
//!
//! - the execution ledger and the data-size bytes are checked before the *next* transaction starts,
//!   and a block that has reached either refuses every later transaction — a deposit excepted;
//! - the state ledger is checked once the next transaction has executed, and a block that has
//!   reached its limit refuses a later transaction only if it adds state gas — a deposit excepted.
//!   One that adds none still fits, and only its own execution can tell which it is.
//!
//! # Deposits
//!
//! A deposit is an L1 message the chain cannot censor: the block derived from L1 must include it,
//! and the builder does not choose it. So a deposit is exempt from both data-availability limits
//! and does not count towards the block's. The execution-gas, state-gas and data-size limits are
//! packing budgets for the transactions the builder chooses, so none of them refuses a deposit
//! either. A deposit still counts towards all three, so the transactions after it find the room it
//! used.
//!
//! The per-transaction data-size limit is not a block limit: it holds a deposit's own execution
//! the way it holds any transaction's, and a deposit that crosses it is included with the stop as
//! its result.
//!
//! # Which dimensions are enforced
//!
//! Of the three gas ledgers the block counts ([`BlockGasCounters`]), execution and state have a
//! block limit. History has none by design; the history bytes beside it are reported, not
//! limited. The write-record count is accumulated here and enforced by nobody: the write-record
//! limit arrives with the state-growth and KV limits.

use alloy_primitives::TxHash;

#[cfg(not(feature = "std"))]
use alloc as std;
use std::boxed::Box;

use alloy_evm::block::{BlockExecutionError, BlockValidationError};

use crate::{
    BlockGasCounters, EvmTxRuntimeLimits, LimitUsage, MegaBlockLimitExceededError, MegaGasUsage,
    MegaTxLimitExceededError,
};

/// The limits one block holds its transactions to.
///
/// [`Default`] carries the production data-size caps and leaves every other dimension unlimited.
/// [`no_limits`](Self::no_limits) clears the data-size caps too. A node sets the dimensions its
/// chain configures, and the block executor always sets
/// [`block_gas_limit`](Self::block_gas_limit) from the block environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockLimits {
    /// The most gas a single transaction may declare.
    pub tx_gas_limit: u64,
    /// The block's gas limit: a transaction is admitted only if its declared gas limit fits in
    /// what the block has left. Also the budget the data-availability footprint of the block's
    /// transactions is held to.
    pub block_gas_limit: u64,
    /// The most bytes a single transaction's EIP-2718 encoding may take.
    pub tx_encode_size_limit: u64,
    /// The most bytes the block's transaction bodies may take together, uncompressed.
    pub block_txs_encode_size_limit: u64,
    /// The most data-availability bytes a single transaction may take. Deposits are exempt.
    pub tx_da_size_limit: u64,
    /// The most data-availability bytes the block's transactions may take together. Deposits are
    /// exempt and do not count towards it.
    pub block_da_size_limit: u64,
    /// The most execution gas the block's transactions may spend together. The transaction that
    /// reaches it is packed; after it, only a deposit is, which it never refuses and which counts
    /// towards it.
    pub block_execution_gas_limit: u64,
    /// The most state gas the block's transactions may spend together. The transaction that
    /// reaches it is packed; after it, only a transaction that adds no state gas is, or a deposit,
    /// which it never refuses and which counts towards it.
    pub block_state_gas_limit: u64,
    /// The most data-size bytes the block's transactions may keep together.
    ///
    /// [`Default`] is [`BLOCK_DATA_LIMIT`](crate::constants::BLOCK_DATA_LIMIT). The transaction
    /// that reaches it is packed; after it, only a deposit is, which it never refuses and which
    /// counts towards it.
    pub block_txs_data_limit: u64,
    /// The limits every transaction of the block runs under, which the executor installs on the
    /// EVM.
    ///
    /// [`Default`] sets the transaction data-size limit to
    /// [`TX_DATA_LIMIT`](crate::constants::TX_DATA_LIMIT) and leaves the frame cap unlimited, so a
    /// frame's budget is the 98% share of what its parent has left.
    pub tx_runtime_limits: EvmTxRuntimeLimits,
}

impl Default for BlockLimits {
    fn default() -> Self {
        Self::with_production_data_limits()
    }
}

impl BlockLimits {
    /// No limit at all.
    pub const fn no_limits() -> Self {
        Self {
            tx_gas_limit: u64::MAX,
            block_gas_limit: u64::MAX,
            tx_encode_size_limit: u64::MAX,
            block_txs_encode_size_limit: u64::MAX,
            tx_da_size_limit: u64::MAX,
            block_da_size_limit: u64::MAX,
            block_execution_gas_limit: u64::MAX,
            block_state_gas_limit: u64::MAX,
            block_txs_data_limit: u64::MAX,
            tx_runtime_limits: EvmTxRuntimeLimits::no_limits(),
        }
    }

    /// The limits a block runs under when its caller configures nothing else.
    ///
    /// Every dimension is unlimited except data size: the block holds
    /// [`BLOCK_DATA_LIMIT`](crate::constants::BLOCK_DATA_LIMIT) bytes, and each transaction holds
    /// [`TX_DATA_LIMIT`](crate::constants::TX_DATA_LIMIT). The block executor installs the
    /// transaction half on the EVM.
    pub const fn with_production_data_limits() -> Self {
        let mut limits = Self::no_limits();
        limits.block_txs_data_limit = crate::constants::BLOCK_DATA_LIMIT;
        limits.tx_runtime_limits = EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(crate::constants::TX_DATA_LIMIT);
        limits
    }

    /// Sets the per-transaction gas limit.
    pub const fn with_tx_gas_limit(mut self, limit: u64) -> Self {
        self.tx_gas_limit = limit;
        self
    }

    /// Sets the block's gas limit.
    ///
    /// The block executor overwrites it with the block environment's gas limit, which is the
    /// number consensus holds the block to.
    pub const fn with_block_gas_limit(mut self, limit: u64) -> Self {
        self.block_gas_limit = limit;
        self
    }

    /// Sets the per-transaction encoded-size limit.
    pub const fn with_tx_encode_size_limit(mut self, limit: u64) -> Self {
        self.tx_encode_size_limit = limit;
        self
    }

    /// Sets the block's encoded-size limit.
    pub const fn with_block_txs_encode_size_limit(mut self, limit: u64) -> Self {
        self.block_txs_encode_size_limit = limit;
        self
    }

    /// Sets the per-transaction data-availability size limit.
    pub const fn with_tx_da_size_limit(mut self, limit: u64) -> Self {
        self.tx_da_size_limit = limit;
        self
    }

    /// Sets the block's data-availability size limit.
    pub const fn with_block_da_size_limit(mut self, limit: u64) -> Self {
        self.block_da_size_limit = limit;
        self
    }

    /// Sets the block's execution-gas limit.
    pub const fn with_block_execution_gas_limit(mut self, limit: u64) -> Self {
        self.block_execution_gas_limit = limit;
        self
    }

    /// Sets the block's state-gas limit.
    pub const fn with_block_state_gas_limit(mut self, limit: u64) -> Self {
        self.block_state_gas_limit = limit;
        self
    }

    /// Sets the block's data-size limit.
    pub const fn with_block_txs_data_limit(mut self, limit: u64) -> Self {
        self.block_txs_data_limit = limit;
        self
    }

    /// Sets the limits every transaction of the block runs under.
    pub const fn with_tx_runtime_limits(mut self, limits: EvmTxRuntimeLimits) -> Self {
        self.tx_runtime_limits = limits;
        self
    }

    /// The limits every transaction of the block runs under.
    pub const fn to_evm_tx_runtime_limits(&self) -> EvmTxRuntimeLimits {
        self.tx_runtime_limits
    }

    /// A limiter that holds a block to these limits.
    pub const fn to_block_limiter(self) -> BlockLimiter {
        BlockLimiter::new(self)
    }
}

/// What one committed transaction adds to a block's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockUsage {
    /// The gas the receipt reports.
    pub gas_used: u64,
    /// The gas by ledger.
    pub gas: MegaGasUsage,
    /// The data-size bytes and write records the transaction kept.
    pub usage: LimitUsage,
    /// The transaction's EIP-2718 encoded size, in bytes.
    pub tx_size: u64,
    /// The transaction's data-availability size, in bytes.
    pub da_size: u64,
    /// The transaction's data-availability footprint, in gas.
    pub da_footprint: u64,
    /// Whether the transaction is a deposit, which the data-availability dimensions exempt and the
    /// execution-gas, state-gas and data-size limits never refuse.
    pub is_deposit: bool,
}

/// What a block has used, and the limits it is held to.
///
/// The counters are cumulative over the transactions the block committed; a transaction that was
/// executed and then discarded leaves them untouched.
#[derive(Clone, Debug)]
pub struct BlockLimiter {
    /// The limits this block is held to.
    pub limits: BlockLimits,
    /// The gas the block's receipts report, which is the block header's gas used.
    pub block_gas_used: u64,
    /// The gas the block's transactions spent, by ledger.
    pub gas: BlockGasCounters,
    /// The data-size bytes and write records the block's transactions kept.
    pub usage: LimitUsage,
    /// The encoded size of the block's transaction bodies, in bytes.
    pub block_tx_size_used: u64,
    /// The data-availability size of the block's non-deposit transactions, in bytes.
    pub block_da_size_used: u64,
    /// The data-availability footprint of the block's non-deposit transactions, in gas. This is
    /// what the block reports as its blob gas used.
    pub block_da_footprint_used: u64,
}

impl BlockLimiter {
    /// A limiter with every counter at zero.
    pub const fn new(limits: BlockLimits) -> Self {
        Self {
            limits,
            block_gas_used: 0,
            gas: BlockGasCounters { execution: 0, state: 0, history: 0, history_bytes: 0 },
            usage: LimitUsage { data_size: 0, write_records: 0 },
            block_tx_size_used: 0,
            block_da_size_used: 0,
            block_da_footprint_used: 0,
        }
    }

    /// The gas the block still has for a transaction's declared gas limit.
    pub const fn available_gas(&self) -> u64 {
        self.limits.block_gas_limit.saturating_sub(self.block_gas_used)
    }

    /// The data-availability footprint the block still has. The budget is the block's gas limit,
    /// as the fork's rule states it.
    pub const fn available_da_footprint(&self) -> u64 {
        self.limits.block_gas_limit.saturating_sub(self.block_da_footprint_used)
    }

    /// Whether `tx` may execute in this block.
    ///
    /// Checks the transaction against its own limits, and against what the block has left; a
    /// deposit is held to neither data-availability limit, nor to the execution-gas or the
    /// data-size limit. It
    /// reads the counters and changes nothing;
    /// [`post_execution_check`](Self::post_execution_check) checks what only the transaction's
    /// execution reveals, and [`post_execution_update`](Self::post_execution_update) advances the
    /// counters once the transaction commits.
    ///
    /// # Errors
    ///
    /// A [`MegaTxLimitExceededError`] when the transaction exceeds a limit of its own — it can
    /// never be included, in this block or any other — and a [`MegaBlockLimitExceededError`]
    /// when the block has no room left for it, which a builder answers by trying the next
    /// transaction.
    pub fn pre_execution_check(
        &self,
        tx_hash: TxHash,
        gas_limit: u64,
        tx_size: u64,
        da_size: u64,
        is_deposit: bool,
    ) -> Result<(), BlockExecutionError> {
        if gas_limit > self.limits.tx_gas_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaTxLimitExceededError::TransactionGasLimit {
                    tx_gas_limit: gas_limit,
                    limit: self.limits.tx_gas_limit,
                },
            ));
        }

        if self.block_gas_used.saturating_add(gas_limit) > self.limits.block_gas_limit {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: gas_limit,
                block_available_gas: self.available_gas(),
            }
            .into());
        }

        if tx_size > self.limits.tx_encode_size_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaTxLimitExceededError::TransactionEncodeSizeLimit {
                    tx_size,
                    limit: self.limits.tx_encode_size_limit,
                },
            ));
        }

        if tx_size.saturating_add(self.block_tx_size_used) > self.limits.block_txs_encode_size_limit
        {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::TransactionEncodeSizeLimit {
                    block_used: self.block_tx_size_used,
                    tx_used: tx_size,
                    limit: self.limits.block_txs_encode_size_limit,
                },
            ));
        }

        // A deposit is an L1 message the chain cannot censor, so it is exempt from both
        // data-availability limits and does not count towards the block's.
        if !is_deposit {
            if da_size > self.limits.tx_da_size_limit {
                return Err(invalid_tx(
                    tx_hash,
                    MegaTxLimitExceededError::DataAvailabilitySizeLimit {
                        da_size,
                        limit: self.limits.tx_da_size_limit,
                    },
                ));
            }

            if da_size.saturating_add(self.block_da_size_used) > self.limits.block_da_size_limit {
                return Err(invalid_tx(
                    tx_hash,
                    MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                        block_used: self.block_da_size_used,
                        tx_used: da_size,
                        limit: self.limits.block_da_size_limit,
                    },
                ));
            }
        }

        // The dimensions a transaction's own execution reveals: the transaction that crossed one
        // is already packed, so what is refused here is the next one. A deposit is not the
        // builder's to refuse on the execution ledger; it still counts, in `post_execution_update`.
        if !is_deposit && self.gas.execution >= self.limits.block_execution_gas_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::ExecutionGasLimit {
                    block_used: self.gas.execution,
                    limit: self.limits.block_execution_gas_limit,
                },
            ));
        }

        // Every committed transaction counts, a deposit included, in `post_execution_update`. The
        // transaction that crossed is already packed, so what is refused here is the next one —
        // never a deposit, which is not the builder's to refuse.
        if !is_deposit && self.usage.data_size >= self.limits.block_txs_data_limit {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::TransactionDataLimit {
                    block_used: self.usage.data_size,
                    limit: self.limits.block_txs_data_limit,
                },
            ));
        }

        // `self.gas.state` is checked after execution, in `post_execution_check`: a block that has
        // reached its state gas still admits a transaction that adds none. `self.gas.history` and
        // `self.usage.write_records` are accumulated and not checked: history has no block limit,
        // and the write-record limit arrives with the state-growth and KV limits.

        Ok(())
    }

    /// Whether the executed transaction whose block usage is `usage` may be packed in this block.
    ///
    /// A block that has reached its state-gas limit refuses a transaction that adds state gas, and
    /// only such a transaction: whether one does is known only once it has run. The transaction
    /// that reaches the limit is itself packed — the block had not reached it when that
    /// transaction came — so the block overshoots its limit by at most one transaction's state
    /// gas, besides what deposits add: a deposit is never refused. Like
    /// [`pre_execution_check`](Self::pre_execution_check) it changes nothing.
    ///
    /// # Errors
    ///
    /// A [`MegaBlockLimitExceededError::StateGasLimit`] when the block has no state gas left and
    /// the transaction, not a deposit, adds some, which a builder answers by trying the next
    /// transaction.
    pub fn post_execution_check(
        &self,
        tx_hash: TxHash,
        usage: &BlockUsage,
    ) -> Result<(), BlockExecutionError> {
        // A deposit is not the builder's to refuse; it still counts, in `post_execution_update`.
        if !usage.is_deposit &&
            usage.gas.state > 0 &&
            self.gas.state >= self.limits.block_state_gas_limit
        {
            return Err(invalid_tx(
                tx_hash,
                MegaBlockLimitExceededError::StateGasLimit {
                    block_used: self.gas.state,
                    tx_used: usage.gas.state,
                    limit: self.limits.block_state_gas_limit,
                },
            ));
        }
        Ok(())
    }

    /// Adds what one committed transaction used.
    ///
    /// Every counter saturates: a block that has already crossed a limit stays crossed, and the
    /// check before the next transaction sees it.
    pub fn post_execution_update(&mut self, usage: &BlockUsage) {
        self.block_gas_used = self.block_gas_used.saturating_add(usage.gas_used);
        self.gas.record(&usage.gas);
        self.usage.data_size = self.usage.data_size.saturating_add(usage.usage.data_size);
        self.usage.write_records =
            self.usage.write_records.saturating_add(usage.usage.write_records);
        self.block_tx_size_used = self.block_tx_size_used.saturating_add(usage.tx_size);
        if !usage.is_deposit {
            self.block_da_size_used = self.block_da_size_used.saturating_add(usage.da_size);
            self.block_da_footprint_used =
                self.block_da_footprint_used.saturating_add(usage.da_footprint);
        }
    }
}

/// Wraps a limit error as the rejection of one transaction.
fn invalid_tx(
    hash: TxHash,
    error: impl alloy_evm::InvalidTxError + 'static,
) -> BlockExecutionError {
    BlockValidationError::InvalidTx { hash, error: Box::new(error) }.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    /// A block that configures nothing holds the production data-size caps and nothing else.
    #[test]
    fn test_the_default_limits_are_the_production_data_size_caps() {
        let limits = BlockLimits::default();
        assert_eq!(limits, BlockLimits::with_production_data_limits());
        assert_eq!(limits.block_txs_data_limit, crate::constants::BLOCK_DATA_LIMIT);
        assert_eq!(limits.tx_runtime_limits.tx_data_size_limit, crate::constants::TX_DATA_LIMIT);
        assert_eq!(limits.tx_runtime_limits.frame_data_size_limit, u64::MAX);
        assert_eq!(limits.tx_gas_limit, u64::MAX);
        assert_eq!(limits.block_execution_gas_limit, u64::MAX);
        assert_eq!(limits.block_state_gas_limit, u64::MAX);

        let unlimited = BlockLimits::no_limits();
        assert_eq!(unlimited.block_txs_data_limit, u64::MAX);
        assert_eq!(unlimited.tx_runtime_limits.tx_data_size_limit, u64::MAX);
    }

    fn limits_with_block_gas(block_gas_limit: u64) -> BlockLimits {
        BlockLimits::no_limits().with_block_gas_limit(block_gas_limit).with_tx_gas_limit(u64::MAX)
    }

    fn saturated_usage() -> BlockUsage {
        BlockUsage {
            gas_used: u64::MAX,
            gas: MegaGasUsage {
                regular: u64::MAX,
                state: u64::MAX,
                history: u64::MAX,
                history_bytes: u64::MAX,
                ..Default::default()
            },
            usage: LimitUsage { data_size: u64::MAX, write_records: u64::MAX },
            tx_size: u64::MAX,
            da_size: u64::MAX,
            da_footprint: u64::MAX,
            is_deposit: false,
        }
    }

    #[test]
    fn test_pre_execution_check_block_gas_addition_saturates() {
        // Block has very high accumulated usage and a transaction with a near-`u64::MAX`
        // gas limit; unchecked addition would wrap and accidentally pass the limit check.
        let mut limiter = BlockLimiter::new(limits_with_block_gas(1_000_000));
        limiter.block_gas_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, u64::MAX, 0, 0, false);
        assert!(result.is_err(), "saturating_add must keep the rejection in place");
    }

    #[test]
    fn test_pre_execution_check_block_tx_size_addition_saturates() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_txs_encode_size_limit(1_000_000));
        limiter.block_tx_size_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, 0, u64::MAX, 0, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_pre_execution_check_block_da_size_addition_saturates() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_da_size_limit(1_000_000));
        limiter.block_da_size_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, 0, 0, u64::MAX, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_post_execution_update_saturates_all_counters() {
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        limiter.block_gas_used = u64::MAX - 1;
        limiter.gas = BlockGasCounters {
            execution: u64::MAX - 1,
            state: u64::MAX - 1,
            history: u64::MAX - 1,
            history_bytes: u64::MAX - 1,
        };
        limiter.usage = LimitUsage { data_size: u64::MAX - 1, write_records: u64::MAX - 1 };
        limiter.block_tx_size_used = u64::MAX - 1;
        limiter.block_da_size_used = u64::MAX - 1;
        limiter.block_da_footprint_used = u64::MAX - 1;

        limiter.post_execution_update(&saturated_usage());

        assert_eq!(limiter.block_gas_used, u64::MAX);
        assert_eq!(
            limiter.gas,
            BlockGasCounters {
                execution: u64::MAX,
                state: u64::MAX,
                history: u64::MAX,
                history_bytes: u64::MAX,
            }
        );
        assert_eq!(limiter.usage, LimitUsage { data_size: u64::MAX, write_records: u64::MAX });
        assert_eq!(limiter.block_tx_size_used, u64::MAX);
        assert_eq!(limiter.block_da_size_used, u64::MAX);
        assert_eq!(limiter.block_da_footprint_used, u64::MAX);
    }

    #[test]
    fn test_post_execution_update_skips_da_for_deposits() {
        // Deposit transactions should not advance the data-availability counters, even when the
        // value would otherwise saturate.
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        limiter.block_da_size_used = 100;
        limiter.block_da_footprint_used = 200;

        limiter.post_execution_update(&BlockUsage {
            is_deposit: true,
            tx_size: 7,
            ..saturated_usage()
        });

        assert_eq!(limiter.block_da_size_used, 100);
        assert_eq!(limiter.block_da_footprint_used, 200);
        assert_eq!(limiter.block_tx_size_used, 7, "a deposit still counts as a transaction body");
    }

    /// The execution ledger is the one gas dimension with a block limit: the transaction that
    /// crosses it is already packed, and the next one is refused.
    #[test]
    fn test_execution_gas_limit_refuses_the_transaction_after_the_crossing() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_execution_gas_limit(1_000));

        limiter.post_execution_update(&BlockUsage {
            gas: MegaGasUsage { regular: 999, ..Default::default() },
            ..Default::default()
        });
        assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_ok());

        limiter.post_execution_update(&BlockUsage {
            gas: MegaGasUsage { regular: 1, ..Default::default() },
            ..Default::default()
        });
        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("the block's execution gas has reached its limit");
        assert!(std::format!("{err}").contains("Block execution gas limit reached"), "{err}");
    }

    /// A deposit is never refused by the execution-gas limit, however far past it the block is,
    /// and still counts towards it: an ordinary transaction after the deposits finds the room
    /// they used.
    #[test]
    fn test_execution_gas_limit_never_refuses_a_deposit_and_counts_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_execution_gas_limit(1_000));
        let deposit = BlockUsage {
            gas: MegaGasUsage { regular: 600, ..Default::default() },
            is_deposit: true,
            ..Default::default()
        };

        // Two deposits cross the limit between them, and a third finds the block past it.
        for _ in 0..3 {
            assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, true).is_ok());
            limiter.post_execution_update(&deposit);
        }
        assert_eq!(limiter.gas.execution, 1_800, "every deposit counts");

        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("the deposits used the room an ordinary transaction would need");
        assert!(std::format!("{err}").contains("block_used=1800"), "{err}");
    }

    /// A deposit is never refused by the block's data-size limit, however far past it the block
    /// is, and still counts towards it: an ordinary transaction after the deposits finds the room
    /// they used.
    #[test]
    fn test_data_size_limit_never_refuses_a_deposit_and_counts_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_txs_data_limit(1_000));
        let deposit = BlockUsage {
            usage: LimitUsage { data_size: 600, write_records: 0 },
            is_deposit: true,
            ..Default::default()
        };

        // Two deposits cross the limit between them, and a third finds the block past it.
        for _ in 0..3 {
            assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, true).is_ok());
            limiter.post_execution_update(&deposit);
        }
        assert_eq!(limiter.usage.data_size, 1_800, "every deposit counts");

        let err = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("the deposits used the room an ordinary transaction would need");
        assert!(std::format!("{err}").contains("Block transactions data limit reached"), "{err}");
        assert!(std::format!("{err}").contains("block_used=1800"), "{err}");
    }

    /// Every limit is an inclusive bound: a transaction that exactly fills what the limit — or
    /// what the block has left of it — is admitted, and one unit more is refused.
    #[test]
    fn test_pre_execution_check_admits_what_exactly_fills_a_limit() {
        const LIMIT: u64 = 1_000;
        const USED: u64 = 400;

        // Each case names the dimension, the limiter it is checked on, what the limit leaves for
        // one transaction, and how that figure reaches `pre_execution_check`.
        type Check = fn(&BlockLimiter, u64) -> Result<(), BlockExecutionError>;
        let cases: [(&str, BlockLimiter, u64, Check); 6] = [
            (
                "a transaction's own gas limit",
                BlockLimiter::new(BlockLimits::no_limits().with_tx_gas_limit(LIMIT)),
                LIMIT,
                |limiter, gas_limit| {
                    limiter.pre_execution_check(B256::ZERO, gas_limit, 0, 0, false)
                },
            ),
            (
                "the block's gas limit",
                BlockLimiter {
                    block_gas_used: USED,
                    ..BlockLimiter::new(BlockLimits::no_limits().with_block_gas_limit(LIMIT))
                },
                LIMIT - USED,
                |limiter, gas_limit| {
                    limiter.pre_execution_check(B256::ZERO, gas_limit, 0, 0, false)
                },
            ),
            (
                "a transaction's own encoded size",
                BlockLimiter::new(BlockLimits::no_limits().with_tx_encode_size_limit(LIMIT)),
                LIMIT,
                |limiter, tx_size| limiter.pre_execution_check(B256::ZERO, 0, tx_size, 0, false),
            ),
            (
                "the block's encoded size",
                BlockLimiter {
                    block_tx_size_used: USED,
                    ..BlockLimiter::new(
                        BlockLimits::no_limits().with_block_txs_encode_size_limit(LIMIT),
                    )
                },
                LIMIT - USED,
                |limiter, tx_size| limiter.pre_execution_check(B256::ZERO, 0, tx_size, 0, false),
            ),
            (
                "a transaction's own data-availability size",
                BlockLimiter::new(BlockLimits::no_limits().with_tx_da_size_limit(LIMIT)),
                LIMIT,
                |limiter, da_size| limiter.pre_execution_check(B256::ZERO, 0, 0, da_size, false),
            ),
            (
                "the block's data-availability size",
                BlockLimiter {
                    block_da_size_used: USED,
                    ..BlockLimiter::new(BlockLimits::no_limits().with_block_da_size_limit(LIMIT))
                },
                LIMIT - USED,
                |limiter, da_size| limiter.pre_execution_check(B256::ZERO, 0, 0, da_size, false),
            ),
        ];

        for (name, limiter, bound, check) in cases {
            assert!(
                check(&limiter, bound).is_ok(),
                "{name}: {bound} exactly fills what is left and is admitted"
            );
            assert!(check(&limiter, bound + 1).is_err(), "{name}: one unit more is refused");
        }
    }

    /// What the block has left is what its refusal reports, and it is the limit minus what the
    /// block used.
    #[test]
    fn test_available_gas_is_the_limit_less_what_the_block_used() {
        let mut limiter = BlockLimiter::new(limits_with_block_gas(1_000));
        assert_eq!(limiter.available_gas(), 1_000, "an empty block has all of it");

        limiter.post_execution_update(&BlockUsage { gas_used: 400, ..Default::default() });
        assert_eq!(limiter.available_gas(), 600);

        let err = limiter
            .pre_execution_check(B256::ZERO, 601, 0, 0, false)
            .expect_err("601 does not fit in 600");
        assert!(
            std::format!("{err}").contains("blocks available gas 600"),
            "the refusal reports what the block has left: {err}"
        );

        // The block cannot owe gas: a counter past the limit reports nothing left.
        limiter.post_execution_update(&BlockUsage { gas_used: u64::MAX, ..Default::default() });
        assert_eq!(limiter.available_gas(), 0);
    }

    /// The history ledger, the history bytes and the write-record count are accumulated, and no
    /// check refuses a transaction on them; nor on the state ledger, whose limit is unlimited
    /// unless a node sets one.
    #[test]
    fn test_history_and_write_records_are_counted_but_not_enforced() {
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        let usage = BlockUsage {
            gas: MegaGasUsage {
                state: 10_000,
                history: 20_000,
                history_bytes: 250,
                ..Default::default()
            },
            usage: LimitUsage { data_size: 0, write_records: 30 },
            ..Default::default()
        };

        limiter.post_execution_update(&usage);

        assert_eq!(limiter.gas.state, 10_000);
        assert_eq!(limiter.gas.history, 20_000);
        assert_eq!(limiter.gas.history_bytes, 250);
        assert_eq!(limiter.usage.write_records, 30);
        assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_ok());
        assert!(limiter.post_execution_check(B256::ZERO, &usage).is_ok());
    }

    /// What one committed transaction spent on the state ledger, and nothing else.
    fn adds_state(state: u64) -> BlockUsage {
        BlockUsage { gas: MegaGasUsage { state, ..Default::default() }, ..Default::default() }
    }

    /// The state ledger's block limit: the transaction that reaches it is packed, and after it a
    /// transaction is refused only if it adds state gas.
    #[test]
    fn test_state_gas_limit_packs_the_crossing_transaction_and_skips_the_next_that_adds_state() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_state_gas_limit(1_000));

        // The n-th transaction finds the block below its limit and crosses it: it is packed.
        limiter.post_execution_update(&adds_state(900));
        assert!(limiter.post_execution_check(B256::ZERO, &adds_state(300)).is_ok());
        limiter.post_execution_update(&adds_state(300));
        assert_eq!(limiter.gas.state, 1_200, "the block overshoots by that one transaction");

        // The (n+1)-th adds state gas: refused, with the state dimension's own error.
        let err = limiter
            .post_execution_check(B256::ZERO, &adds_state(1))
            .expect_err("the block has no state gas left");
        assert!(std::format!("{err}").contains("Block state gas limit reached"), "{err}");
        assert!(std::format!("{err}").contains("block_used=1200"), "{err}");
        assert!(std::format!("{err}").contains("tx_used=1"), "{err}");

        // One that adds none still fits, before execution and after it.
        assert!(limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_ok());
        assert!(limiter.post_execution_check(B256::ZERO, &adds_state(0)).is_ok());
    }

    /// A deposit is never refused by the state-gas limit, however far past it the block is, and
    /// still counts towards it: an ordinary transaction after the deposits finds the room they
    /// used.
    #[test]
    fn test_state_gas_limit_never_refuses_a_deposit_and_counts_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_state_gas_limit(1_000));
        let deposit = |state| BlockUsage { is_deposit: true, ..adds_state(state) };

        // Two deposits cross the limit between them.
        for _ in 0..2 {
            assert!(limiter.post_execution_check(B256::ZERO, &deposit(600)).is_ok());
            limiter.post_execution_update(&deposit(600));
        }
        assert_eq!(limiter.gas.state, 1_200);

        // A third finds the block past its limit and is still admitted.
        assert!(limiter.post_execution_check(B256::ZERO, &deposit(600)).is_ok());
        limiter.post_execution_update(&deposit(600));
        assert_eq!(limiter.gas.state, 1_800, "every deposit counts");

        let err = limiter
            .post_execution_check(B256::ZERO, &adds_state(1))
            .expect_err("the deposits used the room an ordinary transaction would need");
        assert!(std::format!("{err}").contains("block_used=1800"), "{err}");
    }

    /// The state-gas limit is an inclusive bound like every other: a block may spend exactly its
    /// limit, and the block that has is full.
    #[test]
    fn test_state_gas_limit_admits_what_exactly_fills_it() {
        let mut limiter =
            BlockLimiter::new(BlockLimits::no_limits().with_block_state_gas_limit(1_000));
        limiter.post_execution_update(&adds_state(999));
        assert!(limiter.post_execution_check(B256::ZERO, &adds_state(1)).is_ok());

        limiter.post_execution_update(&adds_state(1));
        assert!(
            limiter.post_execution_check(B256::ZERO, &adds_state(1)).is_err(),
            "a block that spent exactly its limit is full"
        );
    }
}
