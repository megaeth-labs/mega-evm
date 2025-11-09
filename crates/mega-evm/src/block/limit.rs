//! Block-level resource limit enforcement for `MegaETH`.
//!
//! This module provides comprehensive limit tracking and enforcement for block-level resources
//! in the `MegaETH` EVM. It implements a two-phase checking system:
//!
//! 1. **Pre-execution checks**: Validates gas limits, transaction size, and data availability
//!    size before transaction execution to ensure the transaction can fit in the block.
//! 2. **Post-execution checks**: Validates actual resource consumption (data size, KV updates)
//!    after execution but before committing state changes.
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
//! ## Transaction-level Limits
//!
//! These limits apply to individual transactions and reject transactions that exceed them:
//! - `single_tx_gas_limit`: Maximum gas a single transaction can use
//! - `single_tx_size_limit`: Maximum encoded size of a transaction
//! - `single_tx_da_size_limit`: Maximum data availability size for a transaction
//!
//! ## Block-level Limits
//!
//! These limits apply to the cumulative resource usage of all transactions in a block:
//! - `block_gas_limit`: Total gas available in the block (auto-set from EVM config)
//! - `block_tx_size_limit`: Total transaction body size allowed in the block
//! - `block_da_size_limit`: Total data availability size allowed in the block
//! - `block_data_limit`: Total data generated from execution (post-execution check)
//! - `block_kv_update_limit`: Total key-value updates allowed (post-execution check)
//!
//! # Example
//!
//! ```rust,ignore
//! use mega_evm::{BlockLimits, BlockLimiter};
//!
//! // Configure custom limits
//! let limits = BlockLimits::default()
//!     .with_block_tx_size_limit(1_000_000)
//!     .with_block_data_limit(10_000);
//!
//! // Create limiter to track usage
//! let mut limiter = limits.to_block_limiter();
//!
//! // Pre-execution check
//! limiter.pre_execution_check(tx_hash, gas_limit, tx_size, da_size)?;
//!
//! // ... execute transaction ...
//!
//! // Post-execution check and update
//! limiter.post_execution_check(tx_hash, gas_used, tx_size, da_size, data_size, kv_updates)?;
//! ```

use alloy_evm::block::{BlockExecutionError, BlockValidationError};
use alloy_primitives::TxHash;

use crate::{MegaBlockLimitExceededError, MegaTxLimitExceededError};

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
    // ================================
    // Pre-execution limits: Checked before transaction execution
    // ================================
    /// Maximum gas limit for a single transaction.
    ///
    /// Transactions with gas limits exceeding this value will be rejected with
    /// [`MegaTxLimitExceededError::TransactionGasLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub single_tx_gas_limit: u64,

    /// Total gas limit for the block.
    ///
    /// This field is **automatically set** from the EVM block environment's gas limit during
    /// block executor initialization. Manual configuration is ignored.
    ///
    /// Transactions that would cause the cumulative block gas to exceed this limit are rejected
    /// with [`BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`].
    ///
    /// Default: `u64::MAX` (overridden at runtime)
    pub(crate) block_gas_limit: u64,

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

    // ================================
    // Post-execution limits: Checked after transaction execution
    // ================================
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
    ///
    /// Default: [`crate::constants::mini_rex::BLOCK_DATA_LIMIT`]
    pub block_data_limit: u64,

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
    ///
    /// Default: [`crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT`]
    pub block_kv_update_limit: u64,
}

impl Default for BlockLimits {
    fn default() -> Self {
        Self {
            single_tx_gas_limit: u64::MAX,
            block_gas_limit: u64::MAX,
            single_tx_size_limit: u64::MAX,
            block_tx_size_limit: u64::MAX,
            single_tx_da_size_limit: u64::MAX,
            block_da_size_limit: u64::MAX,
            block_data_limit: crate::constants::mini_rex::BLOCK_DATA_LIMIT,
            block_kv_update_limit: crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT,
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

    /// Set a custom block data limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified data limit.
    pub fn with_block_data_limit(mut self, limit: u64) -> Self {
        self.block_data_limit = limit;
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
        }
    }
}

/// Stateful block resource limiter that tracks usage and enforces limits.
///
/// This struct maintains cumulative resource usage throughout block execution and validates
/// that each transaction stays within both transaction-level and block-level limits. It
/// provides a two-phase checking mechanism:
///
/// 1. **Pre-execution**: Validates limits that can be determined before execution
///    (gas limit, transaction size, DA size)
/// 2. **Post-execution**: Validates limits based on execution results (data size, KV updates)
///    and updates the usage accumulators
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
    pub fn post_execution_check(
        &mut self,
        tx_hash: TxHash,
        gas_used: u64,
        tx_size_used: u64,
        da_size_used: u64,
        data_size_used: u64,
        kv_updates_used: u64,
    ) -> Result<(), BlockExecutionError> {
        // Block gas limit. No need to check here since it's checked before transaction execution.
        self.block_gas_used += gas_used;

        // Block tx size limit, no need to check here since it's checked before transaction
        // execution.
        self.block_tx_size_used += tx_size_used;

        // Block da size limit, no need to check here since it's checked before transaction
        // execution.
        self.block_da_size_used += da_size_used;

        // Block data limit
        if self.block_data_used + data_size_used > self.limits.block_data_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::DataLimit {
                    block_used: self.block_data_used,
                    tx_used: data_size_used,
                    limit: self.limits.block_data_limit,
                }),
            }));
        }
        self.block_data_used += data_size_used;

        // Block kv updates limit
        if self.block_kv_updates_used + kv_updates_used > self.limits.block_kv_update_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::KVUpdateLimit {
                    block_used: self.block_kv_updates_used,
                    tx_used: kv_updates_used,
                    limit: self.limits.block_kv_update_limit,
                }),
            }));
        }
        self.block_kv_updates_used += kv_updates_used;

        Ok(())
    }
}
