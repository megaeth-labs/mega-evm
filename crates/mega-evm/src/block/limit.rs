//! Block-level resource limit enforcement for `MegaETH`.
//!
//! This module provides comprehensive limit tracking and enforcement for block-level resources
//! in the `MegaETH` EVM. It implements a two-phase checking system to optimize block construction
//! while preventing spam attacks and ensuring fair resource allocation.
//!
//! # Overview
//!
//! MegaEVM enforces **6 types of limits**, each with both **transaction-level** and **block-level**
//! variants:
//!
//! 1. **Gas Limit** - Traditional EVM gas consumption
//! 2. **Transaction Size Limit** - EIP-2718 encoded transaction size
//! 3. **Data Availability (DA) Size Limit** - Compressed DA size
//! 4. **Compute Gas Limit** - Actual computational cost (separate from standard gas)
//! 5. **Data Size Limit** - Total execution data generated
//! 6. **KV Update Limit** - Storage operations count
//!
//! # Two-Phase Checking Strategy
//!
//! ## Phase 1: Pre-execution Checks (Limits 1-3)
//!
//! **When**: Before transaction execution
//! **What**: Validates gas limit, transaction size, and DA size
//! **Purpose**: Fast rejection of oversized transactions without expensive execution
//!
//! ### Transaction-level Violations
//! - **Error**: [`MegaTxLimitExceededError`]
//! - **Action**: Transaction is **rejected permanently** (invalid transaction)
//! - **Reason**: Transaction exceeds individual limit (e.g., gas > `single_tx_gas_limit`)
//! - **Example**: A transaction declares 50M gas when limit is 30M gas
//!
//! ### Block-level Violations
//! - **Error**: [`MegaBlockLimitExceededError`] or
//!   [`BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`]
//! - **Action**: Transaction is **skipped**, try next transaction
//! - **Reason**: Including this transaction would exceed block capacity
//! - **Example**: Block has 5M gas remaining, transaction needs 10M gas
//!
//! ## Phase 2: Post-execution Checks (Limits 4-6)
//!
//! **When**: After transaction execution, before state commitment
//! **What**: Validates compute gas, data size, and KV updates
//! **Purpose**: Enforce limits based on actual execution results
//!
//! ### Transaction-level Enforcement (During Execution)
//! - **Location**: Enforced in [`crate::evm::limit::AdditionalLimit`]
//! - **Behavior**: Transaction halts with `OutOfGas`, remaining gas preserved
//! - **Result**: Transaction fails (status=0) but is **still included in block** (if passes
//!   block-level checks)
//! - **Reason**: Failed transactions consume resources and must be recorded on-chain
//!
//! ### Block-level Enforcement (After Execution)
//! - **Error**: [`MegaBlockLimitExceededError`]
//! - **Action**: Discard execution outcome, skip transaction, try next one
//! - **Reason**: Including this transaction (even with failed status) would exceed block limits
//! - **Example**: Transaction uses 10MB data, but block only has 5MB remaining
//!
//! # Architecture
//!
//! The module consists of two main types:
//!
//! - [`BlockLimits`]: Configuration struct defining all limit thresholds
//! - [`BlockLimiter`]: Stateful tracker that enforces limits and accumulates resource usage
//!
//! # Limit Types
//!
//! ## Pre-execution Limits (Checked before execution)
//!
//! 1. **Gas Limit**
//!    - Tx-level: `single_tx_gas_limit` - Maximum gas per transaction
//!    - Block-level: `block_gas_limit` - Total gas available in block
//!
//! 2. **Transaction Size Limit**
//!    - Tx-level: `single_tx_size_limit` - Maximum encoded transaction size
//!    - Block-level: `block_tx_size_limit` - Total uncompressed transaction size in block
//!
//! 3. **Data Availability Size Limit**
//!    - Tx-level: `single_tx_da_size_limit` - Maximum DA size per transaction
//!    - Block-level: `block_da_size_limit` - Total compressed DA size in block
//!
//! ## Post-execution Limits (Checked during/after execution)
//!
//! 4. **Compute Gas Limit**
//!    - Tx-level: `single_tx_compute_gas_limit` - Maximum compute gas per transaction
//!    - Block-level: `block_compute_gas_limit` - Total compute gas in block
//!
//! 5. **Data Size Limit**
//!    - Tx-level: `single_tx_data_limit` - Maximum data per transaction
//!    - Block-level: `block_data_limit` - Total execution data in block
//!    - Includes: tx data, logs, storage writes, account updates, contract code
//!
//! 6. **KV Update Limit**
//!    - Tx-level: `single_tx_kv_update_limit` - Maximum storage updates per transaction
//!    - Block-level: `block_kv_update_limit` - Total storage updates in block
//!    - Tracks: SSTORE operations and account updates
//!
//! # Block Building Workflow
//!
//! For each transaction in the mempool:
//!
//! 1. **Pre-execution check** - [`BlockLimiter::pre_execution_check`]
//!    - If tx-level violation → Reject permanently
//!    - If block-level violation → Skip, try next transaction
//!
//! 2. **Execute transaction** - [`crate::MegaBlockExecutor::execute_mega_transaction`]
//!    - During execution, tx-level limits (4-6) are enforced in EVM
//!    - If exceeded → Transaction fails but continues to step 3
//!
//! 3. **Post-execution check** - [`BlockLimiter::post_execution_check`]
//!    - If block-level violation → Discard outcome, skip, try next transaction
//!
//! 4. **Commit transaction** - [`crate::MegaBlockExecutor::commit_execution_outcome`]
//!    - Include in block (with success or failed receipt)
//!    - Update block usage counters
//!
//! # Configuration
//!
//! ## For EQUIVALENCE Specification (Optimism Isthmus Compatible)
//!
//! ```rust,ignore
//! let limits = BlockLimits::no_limits()
//!     .with_block_gas_limit(block_env.gas_limit)
//!     .fit_equivalence();
//! ```
//!
//! ## For MINI_REX Specification (Enhanced with Additional Limits)
//!
//! ```rust,ignore
//! use mega_evm::constants::mini_rex;
//!
//! let limits = BlockLimits::no_limits()
//!     .with_block_gas_limit(block_env.gas_limit)
//!     .fit_mini_rex();  // Sets all MINI_REX limits automatically
//! ```
//!
//! Or configure manually:
//!
//! ```rust,ignore
//! let limits = BlockLimits::no_limits()
//!     .with_block_gas_limit(block_env.gas_limit)
//!     .with_single_tx_compute_gas_limit(mini_rex::TX_COMPUTE_GAS_LIMIT)
//!     .with_block_data_limit(mini_rex::BLOCK_DATA_LIMIT)
//!     .with_block_kv_update_limit(mini_rex::BLOCK_KV_UPDATE_LIMIT);
//! ```
//!
//! # Error Handling
//!
//! ## Transaction-level Errors (Permanent Rejection)
//! - [`MegaTxLimitExceededError::TransactionGasLimit`] - Gas limit too high
//! - [`MegaTxLimitExceededError::TransactionSizeLimit`] - Transaction too large
//! - [`MegaTxLimitExceededError::DataAvailabilitySizeLimit`] - DA size too large
//!
//! ## Block-level Errors (Skip and Try Next)
//! - [`MegaBlockLimitExceededError::ComputeGasLimit`] - Would exceed block compute gas
//! - [`MegaBlockLimitExceededError::DataLimit`] - Would exceed block data limit
//! - [`MegaBlockLimitExceededError::KVUpdateLimit`] - Would exceed block KV updates
//! - [`MegaBlockLimitExceededError::TransactionSizeLimit`] - Would exceed block tx size
//! - [`MegaBlockLimitExceededError::DataAvailabilitySizeLimit`] - Would exceed block DA size
//! - [`BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`] - Insufficient gas
//!
//! # See Also
//!
//! - [`crate::evm::limit`] - Transaction-level limit enforcement during execution
//! - [`crate::MegaBlockExecutor`] - Block executor that orchestrates limit checks
//! - [Block and Transaction Limits Documentation](../../../docs/BLOCK_AND_TX_LIMITS.md)

use alloy_evm::{
    block::{BlockExecutionError, BlockValidationError},
    RecoveredTx,
};
use alloy_primitives::TxHash;

use crate::{
    BlockMegaTransactionOutcome, EvmTxRuntimeLimits, MegaBlockLimitExceededError, MegaTxEnvelope,
    MegaTxLimitExceededError,
};

/// Configuration for block-level resource limits.
///
/// This struct defines the thresholds for various resource limits that govern transaction
/// inclusion and block construction. Limits are categorized into two groups:
///
/// 1. **Pre-execution limits**: Checked before transaction execution (gas, size, DA size)
/// 2. **Post-execution limits**: Checked after execution (data size, KV updates)
///
/// # Default Behavior
///
/// By default, most limits are set to `u64::MAX` (effectively unlimited) except for:
/// - `block_data_limit`: Set to [`crate::constants::mini_rex::BLOCK_DATA_LIMIT`]
/// - `block_kv_update_limit`: Set to [`crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT`]
/// - `block_gas_limit`: Auto-set from the EVM block environment (cannot be manually configured)
///
/// # Builder Pattern
///
/// Use builder methods to configure specific limits:
///
/// ```rust,ignore
/// let limits = BlockLimits::default()
///     .with_single_tx_gas_limit(15_000_000)
///     .with_block_tx_size_limit(1_000_000)
///     .with_block_data_limit(5_000);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockLimits {
    /// Maximum gas limit for a single transaction.
    ///
    /// Transactions with gas limits exceeding this value will be rejected with
    /// [`MegaTxLimitExceededError::TransactionGasLimit`].
    pub single_tx_gas_limit: u64,

    /// Total gas limit for the block.
    ///
    /// Transactions that would cause the cumulative block gas to exceed this limit are rejected
    /// with [`BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`].
    pub block_gas_limit: u64,

    /// Maximum encoded size (in bytes) for a single transaction.
    ///
    /// This limit applies to the transaction's EIP-2718 encoded size. Transactions exceeding
    /// this size will be rejected with [`MegaTxLimitExceededError::TransactionSizeLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub single_tx_size_limit: u64,

    /// Maximum cumulative size (in bytes) of all transaction bodies in a block.
    ///
    /// This limit applies to the **uncompressed** transaction size. For compressed data
    /// availability limits, see [`block_da_size_limit`](Self::block_da_size_limit).
    ///
    /// Transactions that would cause the cumulative block transaction size to exceed this
    /// limit are rejected with [`MegaBlockLimitExceededError::TransactionSizeLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub block_tx_size_limit: u64,

    /// Maximum data availability size (in bytes) for a single transaction.
    ///
    /// This represents the compressed size for data availability purposes. Transactions
    /// exceeding this size will be rejected with
    /// [`MegaTxLimitExceededError::DataAvailabilitySizeLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub single_tx_da_size_limit: u64,

    /// Maximum cumulative data availability size (in bytes) for all transactions in a block.
    ///
    /// This applies to the **compressed** transaction size for data availability. For
    /// uncompressed transaction body limits, see
    /// [`block_tx_size_limit`](Self::block_tx_size_limit).
    ///
    /// Transactions that would cause the cumulative block DA size to exceed this limit are
    /// rejected with [`MegaBlockLimitExceededError::DataAvailabilitySizeLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub block_da_size_limit: u64,

    /// Maximum data size for a single transaction.
    pub single_tx_data_limit: u64,

    /// Maximum cumulative data size (in bytes) generated from block execution.
    ///
    /// This limit is checked **after** transaction execution but **before** committing state
    /// changes. It tracks the total data produced by all transactions in the block, including
    /// logs, return data, and other execution artifacts.
    ///
    /// Transactions that would cause the cumulative block data to exceed this limit are
    /// rejected with [`MegaBlockLimitExceededError::DataLimit`], and their state changes
    /// are **not committed**.
    ///
    /// Note: Transaction-level data limits are enforced separately in the `MegaEVM`.
    /// Transactions exceeding transaction-level limits may still be included but marked
    /// as failed.
    pub block_data_limit: u64,

    /// Maximum key-value updates for a single transaction.
    pub single_tx_kv_update_limit: u64,

    /// Maximum cumulative key-value updates allowed in a block.
    ///
    /// This limit is checked **after** transaction execution but **before** committing state
    /// changes. It tracks the total number of storage slot updates (SSTORE operations) across
    /// all transactions in the block.
    ///
    /// Transactions that would cause the cumulative KV updates to exceed this limit are
    /// rejected with [`MegaBlockLimitExceededError::KVUpdateLimit`], and their state changes
    /// are **not committed**.
    ///
    /// Note: Transaction-level KV update limits are enforced separately in the `MegaEVM`.
    /// Transactions exceeding transaction-level limits may still be included but marked
    /// as failed.
    pub block_kv_update_limit: u64,

    /// Maximum compute gas limit for a single transaction.
    pub single_tx_compute_gas_limit: u64,

    /// Maximum cumulative compute gas limit for all transactions in a block.
    ///
    /// This limit is checked **after** transaction execution but **before** committing state
    /// changes. It tracks the total compute gas consumed by all transactions in the block.
    ///
    /// Transactions that would cause the cumulative block compute gas to exceed this limit are
    /// rejected with [`MegaBlockLimitExceededError::ComputeGasLimit`], and their state changes
    /// are **not committed**.
    pub block_compute_gas_limit: u64,
}

impl BlockLimits {
    /// Creates a new block limits instance with no limits.
    pub fn no_limits() -> Self {
        Self {
            single_tx_gas_limit: u64::MAX,
            block_gas_limit: u64::MAX,
            single_tx_size_limit: u64::MAX,
            block_tx_size_limit: u64::MAX,
            single_tx_da_size_limit: u64::MAX,
            block_da_size_limit: u64::MAX,
            single_tx_data_limit: u64::MAX,
            block_data_limit: u64::MAX,
            single_tx_kv_update_limit: u64::MAX,
            block_kv_update_limit: u64::MAX,
            single_tx_compute_gas_limit: u64::MAX,
            block_compute_gas_limit: u64::MAX,
        }
    }

    /// Fits the block limits to the equivalence spec. Overrides those limits with the
    /// `MegaSpecId::EQUIVALENCE` spec limits.
    pub fn fit_equivalence(self) -> Self {
        let tx_runtime_limits = EvmTxRuntimeLimits::equivalence();
        Self {
            single_tx_da_size_limit: tx_runtime_limits.tx_data_size_limit,
            single_tx_kv_update_limit: tx_runtime_limits.tx_kv_updates_limit,
            single_tx_compute_gas_limit: tx_runtime_limits.tx_compute_gas_limit,
            ..self
        }
    }

    /// Fits the block limits to the mini-rex spec. Overrides those limits with the
    /// `MegaSpecId::MINI_REX` spec limits.
    pub fn fit_mini_rex(self) -> Self {
        let tx_runtime_limits = EvmTxRuntimeLimits::mini_rex();
        Self {
            single_tx_da_size_limit: tx_runtime_limits.tx_data_size_limit,
            single_tx_kv_update_limit: tx_runtime_limits.tx_kv_updates_limit,
            single_tx_compute_gas_limit: tx_runtime_limits.tx_compute_gas_limit,
            block_data_limit: crate::constants::mini_rex::BLOCK_DATA_LIMIT,
            block_kv_update_limit: crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT,
            ..self
        }
    }
}

impl BlockLimits {
    /// Set a custom single transaction gas limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction gas limit. Transactions with gas limits
    /// exceeding this value will be rejected.
    ///
    /// # Parameters
    ///
    /// - `limit`: Maximum gas limit for a single transaction
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let limits = BlockLimits::default()
    ///     .with_single_tx_gas_limit(15_000_000);
    /// ```
    pub fn with_single_tx_gas_limit(mut self, limit: u64) -> Self {
        self.single_tx_gas_limit = limit;
        self
    }

    /// Set a custom block gas limit.
    ///
    /// **Warning**: This setting is **overridden** during block executor initialization.
    /// The block gas limit is automatically set from the EVM block environment's gas limit.
    /// This method exists for testing purposes only.
    ///
    /// # Parameters
    ///
    /// - `limit`: Block gas limit (will be overridden at runtime)
    pub fn with_block_gas_limit(mut self, limit: u64) -> Self {
        self.block_gas_limit = limit;
        self
    }

    /// Set a custom single transaction size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction size limit.
    pub fn with_single_tx_size_limit(mut self, limit: u64) -> Self {
        self.single_tx_size_limit = limit;
        self
    }

    /// Set a custom block transaction size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction size limit.
    pub fn with_block_tx_size_limit(mut self, limit: u64) -> Self {
        self.block_tx_size_limit = limit;
        self
    }

    /// Set a custom single transaction data availability size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction data availability size limit.
    pub fn with_single_tx_da_size_limit(mut self, limit: u64) -> Self {
        self.single_tx_da_size_limit = limit;
        self
    }

    /// Set a custom block data availability size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified data availability size limit.
    pub fn with_block_da_size_limit(mut self, limit: u64) -> Self {
        self.block_da_size_limit = limit;
        self
    }

    /// Set a custom single transaction data limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction data limit.
    pub fn with_single_tx_data_limit(mut self, limit: u64) -> Self {
        self.single_tx_data_limit = limit;
        self
    }

    /// Set a custom block data limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified data limit.
    pub fn with_block_data_limit(mut self, limit: u64) -> Self {
        self.block_data_limit = limit;
        self
    }

    /// Set a custom single transaction KV update limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction KV update limit.
    pub fn with_single_tx_kv_update_limit(mut self, limit: u64) -> Self {
        self.single_tx_kv_update_limit = limit;
        self
    }

    /// Set a custom block KV update limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified KV update limit.
    pub fn with_block_kv_update_limit(mut self, limit: u64) -> Self {
        self.block_kv_update_limit = limit;
        self
    }

    /// Set a custom single transaction compute gas limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction compute gas limit.
    pub fn with_single_tx_compute_gas_limit(mut self, limit: u64) -> Self {
        self.single_tx_compute_gas_limit = limit;
        self
    }

    /// Set a custom block compute gas limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified block compute gas limit.
    pub fn with_block_compute_gas_limit(mut self, limit: u64) -> Self {
        self.block_compute_gas_limit = limit;
        self
    }
    /// Create a new block limiter from these limits.
    ///
    /// This converts the limit configuration into a stateful [`BlockLimiter`] that tracks
    /// resource usage and enforces the configured limits throughout block execution.
    ///
    /// # Returns
    ///
    /// A new [`BlockLimiter`] initialized with zero usage for all tracked resources.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let limits = BlockLimits::default().with_block_data_limit(5_000);
    /// let limiter = limits.to_block_limiter();
    /// ```
    pub fn to_block_limiter(self) -> BlockLimiter {
        BlockLimiter {
            limits: self,
            block_gas_used: 0,
            block_data_used: 0,
            block_kv_updates_used: 0,
            block_tx_size_used: 0,
            block_da_size_used: 0,
            block_compute_gas_used: 0,
        }
    }

    /// Convert the block limits to the runtime limits for a single transaction.
    pub fn to_evm_tx_runtime_limits(&self) -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits {
            tx_data_size_limit: self.single_tx_data_limit,
            tx_kv_updates_limit: self.single_tx_kv_update_limit,
            tx_compute_gas_limit: self.single_tx_compute_gas_limit,
        }
    }
}

/// Stateful block resource limiter that tracks usage and enforces limits.
///
/// This struct maintains cumulative resource usage throughout block execution and validates
/// that each transaction stays within both transaction-level and block-level limits. It
/// provides a two-phase checking mechanism:
///
/// 1. **Pre-execution**: Validates limits that can be determined before execution (gas limit,
///    transaction size, DA size)
/// 2. **Post-execution**: Validates limits based on execution results (data size, KV updates) and
///    updates the usage accumulators
///
/// # Resource Tracking
///
/// The limiter tracks five cumulative metrics:
/// - Gas consumed by all transactions
/// - Total transaction body size
/// - Total data availability size
/// - Total execution data generated
/// - Total key-value storage updates
///
/// # Usage Pattern
///
/// ```rust,ignore
/// let mut limiter = BlockLimits::default().to_block_limiter();
///
/// for tx in transactions {
///     // Pre-execution check
///     limiter.pre_execution_check(
///         tx.hash(),
///         tx.gas_limit(),
///         tx.size(),
///         tx.da_size(),
///     )?;
///
///     let result = execute_transaction(tx);
///
///     // Post-execution check and update
///     limiter.post_execution_check(
///         tx.hash(),
///         result.gas_used,
///         tx.size(),
///         tx.da_size(),
///         result.data_size,
///         result.kv_updates,
///     )?;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct BlockLimiter {
    /// The configured resource limits.
    pub limits: BlockLimits,

    /// Cumulative gas consumed by all committed transactions in the block.
    pub block_gas_used: u64,

    /// Cumulative encoded size of all transaction bodies in the block (uncompressed).
    pub block_tx_size_used: u64,

    /// Cumulative data availability size for all transactions in the block (compressed).
    pub block_da_size_used: u64,

    /// Cumulative execution data generated by all transactions in the block.
    ///
    /// This includes logs, return data, and other execution artifacts.
    pub block_data_used: u64,

    /// Cumulative key-value storage updates performed by all transactions in the block.
    ///
    /// This tracks the total number of SSTORE operations across all transactions.
    pub block_kv_updates_used: u64,

    /// Cumulative compute gas consumed by all transactions in the block.
    pub block_compute_gas_used: u64,
}

impl BlockLimiter {
    /// Create a new block limiter with the specified limits.
    ///
    /// Initializes a limiter with all resource usage counters set to zero.
    ///
    /// # Parameters
    ///
    /// - `limits`: The resource limit configuration
    ///
    /// # Returns
    ///
    /// A new limiter ready to track resource usage
    pub fn new(limits: BlockLimits) -> Self {
        Self {
            limits,
            block_gas_used: 0,
            block_data_used: 0,
            block_kv_updates_used: 0,
            block_tx_size_used: 0,
            block_da_size_used: 0,
            block_compute_gas_used: 0,
        }
    }

    /// Validate transaction against pre-execution limits.
    ///
    /// This method validates that a transaction can be included in the block by checking
    /// limits that are known before execution:
    /// - Single transaction gas limit
    /// - Remaining block gas capacity
    /// - Single transaction size limit
    /// - Remaining block transaction size capacity
    /// - Single transaction DA size limit
    /// - Remaining block DA size capacity
    ///
    /// **Important**: This method does **not** modify any state. It only performs validation.
    /// Call [`post_execution_check`](Self::post_execution_check) after execution to update
    /// usage counters.
    ///
    /// # Parameters
    ///
    /// - `tx_hash`: Transaction hash for error reporting
    /// - `gas_limit`: Transaction's declared gas limit
    /// - `tx_size`: Transaction's encoded size in bytes (EIP-2718 encoding)
    /// - `da_size`: Transaction's compressed data availability size in bytes
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Transaction passes all pre-execution checks
    /// - `Err(BlockExecutionError)`: Transaction violates one or more limits
    ///
    /// # Errors
    ///
    /// Returns [`BlockExecutionError::Validation`] containing either:
    /// - [`MegaTxLimitExceededError`]: Transaction exceeds its own limits (invalid transaction)
    /// - [`MegaBlockLimitExceededError`]: Transaction would exceed block capacity
    /// - [`BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`][] - Insufficient
    ///   remaining gas in block
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut limiter = BlockLimits::default().to_block_limiter();
    ///
    /// limiter.pre_execution_check(
    ///     tx.hash(),
    ///     tx.gas_limit(),
    ///     tx.encode_2718_len() as u64,
    ///     tx.estimated_da_size(),
    /// )?;
    /// ```
    pub fn pre_execution_check(
        &self,
        tx_hash: TxHash,
        gas_limit: u64,
        tx_size: u64,
        da_size: u64,
    ) -> Result<(), BlockExecutionError> {
        // Check single transaction gas limit
        if gas_limit > self.limits.single_tx_gas_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaTxLimitExceededError::TransactionGasLimit {
                    tx_gas_limit: gas_limit,
                    limit: self.limits.single_tx_gas_limit,
                }),
            }));
        }

        // Check block gas limit
        if self.block_gas_used + gas_limit > self.limits.block_gas_limit {
            return Err(BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: gas_limit,
                    block_available_gas: self.limits.block_gas_limit - self.block_gas_used,
                },
            ));
        }

        // Check single transaction size limit
        if tx_size > self.limits.single_tx_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaTxLimitExceededError::TransactionSizeLimit {
                    tx_size,
                    limit: self.limits.single_tx_size_limit,
                }),
            }));
        }

        // Check block transaction size limit
        if tx_size + self.block_tx_size_used > self.limits.block_tx_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::TransactionSizeLimit {
                    block_used: self.block_tx_size_used,
                    tx_used: tx_size,
                    limit: self.limits.block_tx_size_limit,
                }),
            }));
        }

        // Check single transaction data availability size limit
        if da_size > self.limits.single_tx_da_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaTxLimitExceededError::DataAvailabilitySizeLimit {
                    da_size,
                    limit: self.limits.single_tx_da_size_limit,
                }),
            }));
        }

        // Check block data availability size limit
        if da_size + self.block_da_size_used > self.limits.block_da_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                    block_used: self.block_da_size_used,
                    tx_used: da_size,
                    limit: self.limits.block_da_size_limit,
                }),
            }));
        }

        Ok(())
    }

    /// Validate transaction result and update usage counters.
    ///
    /// This method is called **after** transaction execution to:
    /// 1. Validate that the transaction's resource consumption doesn't exceed block limits
    /// 2. Update the limiter's cumulative usage counters
    ///
    /// The following limits are checked:
    /// - Block data limit (logs, return data, etc.)
    /// - Block KV update limit (SSTORE operations)
    ///
    /// Gas, transaction size, and DA size are **not** checked here (already validated in
    /// pre-execution), but their usage counters are updated.
    ///
    /// **Important**: If this method returns an error, the transaction's state changes should
    /// **not** be committed to the database.
    ///
    /// # Parameters
    ///
    /// - `tx_hash`: Transaction hash for error reporting
    /// - `gas_used`: Actual gas consumed during execution
    /// - `tx_size_used`: Transaction's encoded size in bytes (should match pre-execution value)
    /// - `da_size_used`: Transaction's DA size in bytes (should match pre-execution value)
    /// - `data_size_used`: Total data generated by execution (logs, return data, etc.)
    /// - `kv_updates_used`: Number of storage slot updates (SSTORE operations)
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Transaction passes all checks; usage counters updated
    /// - `Err(BlockExecutionError)`: Transaction violates a limit; **do not commit state**
    ///
    /// # Errors
    ///
    /// Returns [`BlockExecutionError::Validation`] with [`BlockValidationError::InvalidTx`]
    /// containing [`MegaBlockLimitExceededError`] if the transaction would cause the block
    /// to exceed:
    /// - [`MegaBlockLimitExceededError::DataLimit`]: Block data limit
    /// - [`MegaBlockLimitExceededError::KVUpdateLimit`]: Block KV update limit
    ///
    /// # State Modification
    ///
    /// On success, this method updates all cumulative usage counters:
    /// - `block_gas_used += gas_used`
    /// - `block_tx_size_used += tx_size_used`
    /// - `block_da_size_used += da_size_used`
    /// - `block_data_used += data_size_used`
    /// - `block_kv_updates_used += kv_updates_used`
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = execute_transaction(tx);
    ///
    /// limiter.post_execution_check(
    ///     tx.hash(),
    ///     result.gas_used(),
    ///     tx.encode_2718_len() as u64,
    ///     tx.estimated_da_size(),
    ///     result.data_size(),
    ///     result.kv_updates(),
    /// )?;
    ///
    /// // Only commit if post_execution_check passed
    /// db.commit(result.state);
    /// ```
    pub fn post_execution_check<T: RecoveredTx<MegaTxEnvelope>>(
        &mut self,
        outcome: &BlockMegaTransactionOutcome<T>,
    ) -> Result<(), BlockExecutionError> {
        // Block gas limit. No need to check here since it's checked before transaction execution.
        self.block_gas_used += outcome.result.gas_used();

        // Block tx size limit, no need to check here since it's checked before transaction
        // execution.
        self.block_tx_size_used += outcome.tx_size;

        // Block da size limit, no need to check here since it's checked before transaction
        // execution.
        self.block_da_size_used += outcome.da_size;

        // Block data limit
        if self.block_data_used + outcome.data_size > self.limits.block_data_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: outcome.tx.tx().tx_hash(),
                error: Box::new(MegaBlockLimitExceededError::DataLimit {
                    block_used: self.block_data_used,
                    tx_used: outcome.data_size,
                    limit: self.limits.block_data_limit,
                }),
            }));
        }
        self.block_data_used += outcome.data_size;

        // Block kv updates limit
        if self.block_kv_updates_used + outcome.kv_updates > self.limits.block_kv_update_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: outcome.tx.tx().tx_hash(),
                error: Box::new(MegaBlockLimitExceededError::KVUpdateLimit {
                    block_used: self.block_kv_updates_used,
                    tx_used: outcome.kv_updates,
                    limit: self.limits.block_kv_update_limit,
                }),
            }));
        }
        self.block_kv_updates_used += outcome.kv_updates;

        // Block compute gas limit
        if self.block_compute_gas_used + outcome.compute_gas_used >
            self.limits.block_compute_gas_limit
        {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: outcome.tx.tx().tx_hash(),
                error: Box::new(MegaBlockLimitExceededError::ComputeGasLimit {
                    block_used: self.block_compute_gas_used,
                    tx_used: outcome.compute_gas_used,
                    limit: self.limits.block_compute_gas_limit,
                }),
            }));
        }
        self.block_compute_gas_used += outcome.compute_gas_used;

        Ok(())
    }
}
