//! Block-level resource limit enforcement for `MegaETH`.
//!
//! This module provides comprehensive limit tracking and enforcement for block-level resources
//! in the `MegaETH` EVM. It implements a two-phase checking system to optimize block construction
//! while preventing spam attacks and ensuring fair resource allocation.
//!
//! # Overview
//!
//! `MegaEVM` enforces **6 types of limits**, each with both **transaction-level** and
//! **block-level** variants:
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
//! - **Reason**: Transaction exceeds individual limit (e.g., gas > `tx_gas_limit`)
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
//! - **Result**: Transaction fails (status=0) but is **still included in block**
//! - **Reason**: Failed transactions consume resources and must be recorded on-chain
//!
//! ### Block-level Enforcement (Before Next Transaction)
//! - The first transaction that causes the block to exceed limits is **allowed to execute and be
//!   included**
//! - **Pre-execution check**: Before executing the next transaction, check if block limit has
//!   already been exceeded
//! - **Action**: If limit already exceeded, reject transaction before execution
//! - **Reason**: Maximize block utilization by not wasting valid transaction execution
//! - **Example**: Block has 10MB data limit. TX1 uses 6MB (included), TX2 uses 8MB (included, total
//!   14MB exceeds limit), TX3 is rejected before execution
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
//!    - Tx-level: `tx_gas_limit` - Maximum gas per transaction
//!    - Block-level: `block_gas_limit` - Total gas available in block
//!
//! 2. **Transaction Size Limit**
//!    - Tx-level: `tx_encode_size_limit` - Maximum encoded transaction size
//!    - Block-level: `block_txs_encode_size_limit` - Total uncompressed transaction size in block
//!
//! 3. **Data Availability Size Limit**
//!    - Tx-level: `tx_da_size_limit` - Maximum DA size per transaction
//!    - Block-level: `block_da_size_limit` - Total compressed DA size in block
//!    - **Note**: Deposit transactions are exempt from DA size limit checks
//!
//! ## Post-execution Limits (Checked during/after execution)
//!
//! 4. **Compute Gas Limit**
//!    - Tx-level: `tx_compute_gas_limit` - Maximum compute gas per transaction
//!    - Block-level: `block_compute_gas_limit` - Total compute gas in block
//!
//! 5. **Data Size Limit**
//!    - Tx-level: `tx_data_limit` - Maximum data per transaction
//!    - Block-level: `block_txs_data_limit` - Total execution data in block
//!    - Includes: tx data, logs, storage writes, account updates, contract code
//!
//! 6. **KV Update Limit**
//!    - Tx-level: `tx_kv_update_limit` - Maximum storage updates per transaction
//!    - Block-level: `block_kv_update_limit` - Total storage updates in block
//!    - Tracks: SSTORE operations and account updates
//!
//! # Deposit Transaction Exemptions
//!
//! Deposit transactions (Optimism Layer 1 to Layer 2 deposits) receive special treatment:
//!
//! - **DA Size Limit Exemption**: Deposit transactions are exempt from both transaction-level and
//!   block-level Data Availability size limit checks during pre-execution validation
//! - **Rationale**: Deposit transactions are trustless L1→L2 messages that cannot be censored. They
//!   must be included in blocks regardless of their DA size to maintain bridge integrity
//! - **Tracking**: While exempt from limit checks, deposit DA sizes are still tracked and
//!   accumulated in block DA usage counters for monitoring purposes
//! - **Other Limits**: Deposit transactions are still subject to all other limits (gas, tx size,
//!   compute gas, data size, KV updates)
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
//! 3. **Post-execution update** - [`BlockLimiter::post_execution_update_raw`]
//!    - Accumulates resource usage from the executed transaction into block-level counters
//!    - Does not validate post-execution limits; over-limit enforcement happens before admitting
//!      the next transaction in [`BlockLimiter::pre_execution_check`]
//!    - Compute gas accumulates into two counters, one reported and one enforced; see
//!      [`BlockLimiter::block_compute_gas_used`]
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
//! ## For `MINI_REX` Specification (Enhanced with Additional Limits)
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
//!     .with_tx_compute_gas_limit(mini_rex::TX_COMPUTE_GAS_LIMIT)
//!     .with_block_txs_data_limit(mini_rex::BLOCK_DATA_LIMIT)
//!     .with_block_kv_update_limit(mini_rex::BLOCK_KV_UPDATE_LIMIT);
//! ```
//!
//! # Error Handling
//!
//! ## Transaction-level Errors (Permanent Rejection)
//! - [`MegaTxLimitExceededError::TransactionGasLimit`] - Gas limit too high
//! - [`MegaTxLimitExceededError::TransactionEncodeSizeLimit`] - Transaction too large
//! - [`MegaTxLimitExceededError::DataAvailabilitySizeLimit`] - DA size too large
//!
//! ## Block-level Errors (Skip and Try Next)
//! - [`MegaBlockLimitExceededError::ComputeGasLimit`] - Would exceed block compute gas
//! - [`MegaBlockLimitExceededError::TransactionDataLimit`] - Would exceed block data limit
//! - [`MegaBlockLimitExceededError::KVUpdateLimit`] - Would exceed block KV updates
//! - [`MegaBlockLimitExceededError::TransactionEncodeSizeLimit`] - Would exceed block tx size
//! - [`MegaBlockLimitExceededError::DataAvailabilitySizeLimit`] - Would exceed block DA size
//! - [`BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`] - Insufficient gas
//!
//! # See Also
//!
//! - [`crate::evm::limit`] - Transaction-level limit enforcement during execution
//! - [`crate::MegaBlockExecutor`] - Block executor that orchestrates limit checks

#[cfg(not(feature = "std"))]
use alloc as std;
use std::boxed::Box;

use alloy_evm::block::{BlockExecutionError, BlockValidationError};
use alloy_primitives::TxHash;

use crate::{
    EvmTxRuntimeLimits, MegaBlockLimitExceededError, MegaHardfork, MegaTxLimitExceededError,
};

/// Configuration for block-level resource limits. The block-level resource limits are associated
/// with a specific `MegaHardfork` instead of a `MegaSpecId`. In constrast, `EvmTxRuntimeLimits` is
/// associated with a `MegaSpecId`.
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
/// - `block_txs_data_limit`: Set to [`crate::constants::mini_rex::BLOCK_DATA_LIMIT`]
/// - `block_kv_update_limit`: Set to [`crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT`]
/// - `block_gas_limit`: Auto-set from the EVM block environment (cannot be manually configured)
///
/// # Builder Pattern
///
/// Use builder methods to configure specific limits:
///
/// ```rust,ignore
/// let limits = BlockLimits::default()
///     .with_tx_gas_limit(15_000_000)
///     .with_block_txs_encode_size_limit(1_000_000)
///     .with_block_txs_data_limit(5_000);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockLimits {
    /// Maximum gas limit for a single transaction.
    ///
    /// Transactions with gas limits exceeding this value will be rejected with
    /// [`MegaTxLimitExceededError::TransactionGasLimit`].
    pub tx_gas_limit: u64,

    /// Total gas limit for the block.
    ///
    /// Transactions that would cause the cumulative block gas to exceed this limit are rejected
    /// with [`BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas`].
    pub block_gas_limit: u64,

    /// Maximum encoded size (in bytes) for a single transaction.
    ///
    /// This limit applies to the transaction's EIP-2718 encoded size. Transactions exceeding
    /// this size will be rejected with [`MegaTxLimitExceededError::TransactionEncodeSizeLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub tx_encode_size_limit: u64,

    /// Maximum cumulative size (in bytes) of all transaction bodies in a block.
    ///
    /// This limit applies to the **uncompressed** transaction size. For compressed data
    /// availability limits, see [`block_da_size_limit`](Self::block_da_size_limit).
    ///
    /// Transactions that would cause the cumulative block transaction size to exceed this
    /// limit are rejected with [`MegaBlockLimitExceededError::TransactionEncodeSizeLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub block_txs_encode_size_limit: u64,

    /// Maximum data availability size (in bytes) for a single transaction.
    ///
    /// This represents the compressed size for data availability purposes. Transactions
    /// exceeding this size will be rejected with
    /// [`MegaTxLimitExceededError::DataAvailabilitySizeLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub tx_da_size_limit: u64,

    /// Maximum cumulative data availability size (in bytes) for all transactions in a block.
    ///
    /// This applies to the **compressed** transaction size for data availability. For
    /// uncompressed transaction body limits, see
    /// [`block_txs_encode_size_limit`](Self::block_txs_encode_size_limit).
    ///
    /// Transactions that would cause the cumulative block DA size to exceed this limit are
    /// rejected with [`MegaBlockLimitExceededError::DataAvailabilitySizeLimit`].
    ///
    /// Default: `u64::MAX` (effectively unlimited)
    pub block_da_size_limit: u64,

    /// Maximum data size of a single transaction's execution outcome, i.e., encoded transaction
    /// object, encoded state changes, logs, etc. The data will need to be saved and transmitted
    /// over network.
    pub tx_data_limit: u64,

    /// Maximum cumulative data size (in bytes) of all transactions' execution outcomes from block
    /// execution.
    ///
    /// This limit is checked **after** transaction execution but **before** committing state
    /// changes. It tracks the total data produced by all transactions in the block, including
    /// logs, return data, and other execution artifacts.
    ///
    /// Transactions that would cause the cumulative block data to exceed this limit are
    /// rejected with [`MegaBlockLimitExceededError::TransactionDataLimit`], and their state
    /// changes are **not committed**.
    ///
    /// Note: Transaction-level data limits are enforced separately in the `MegaEVM`.
    /// Transactions exceeding transaction-level limits may still be included but marked
    /// as failed.
    pub block_txs_data_limit: u64,

    /// Maximum key-value updates for a single transaction.
    pub tx_kv_update_limit: u64,

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
    pub tx_compute_gas_limit: u64,

    /// Maximum cumulative compute gas limit for all transactions in a block.
    ///
    /// This limit is checked **after** transaction execution but **before** committing state
    /// changes. It tracks the total compute gas consumed by all transactions in the block.
    ///
    /// Transactions that would cause the cumulative block compute gas to exceed this limit are
    /// rejected with [`MegaBlockLimitExceededError::ComputeGasLimit`], and their state changes
    /// are **not committed**.
    pub block_compute_gas_limit: u64,

    /// Maximum state growth limit for a single transaction.
    pub tx_state_growth_limit: u64,

    /// Maximum cumulative state growth limit for all transactions in a block.
    ///
    /// This limit is checked **after** transaction execution but **before** committing state
    /// changes. It tracks the total state growth consumed by all transactions in the block.
    ///
    /// Transactions that would cause the cumulative block state growth to exceed this limit are
    /// rejected with [`MegaBlockLimitExceededError::StateGrowthLimit`], and their state changes
    /// are **not committed**.
    pub block_state_growth_limit: u64,

    /// Compute gas limit when accessing block environment data.
    ///
    /// When a transaction accesses volatile block environment data (e.g., TIMESTAMP, COINBASE),
    /// the compute gas is capped to this limit to prevent `DoS` attacks.
    pub block_env_access_compute_gas_limit: u64,

    /// Compute gas limit when accessing oracle data.
    ///
    /// When a transaction accesses the oracle contract, the compute gas is capped to this
    /// limit to prevent `DoS` attacks.
    pub oracle_access_compute_gas_limit: u64,
}

impl BlockLimits {
    /// Creates a new block limits instance with no limits.
    pub fn no_limits() -> Self {
        Self {
            tx_gas_limit: u64::MAX,
            block_gas_limit: u64::MAX,
            tx_encode_size_limit: u64::MAX,
            block_txs_encode_size_limit: u64::MAX,
            tx_da_size_limit: u64::MAX,
            block_da_size_limit: u64::MAX,
            tx_data_limit: u64::MAX,
            block_txs_data_limit: u64::MAX,
            tx_kv_update_limit: u64::MAX,
            block_kv_update_limit: u64::MAX,
            tx_compute_gas_limit: u64::MAX,
            block_compute_gas_limit: u64::MAX,
            tx_state_growth_limit: u64::MAX,
            block_state_growth_limit: u64::MAX,
            block_env_access_compute_gas_limit: u64::MAX,
            oracle_access_compute_gas_limit: u64::MAX,
        }
    }

    /// Creates a new block limits instance from a hardfork and a block gas limit.
    pub fn from_hardfork_and_block_gas_limit(hardfork: MegaHardfork, block_gas_limit: u64) -> Self {
        let spec = hardfork.spec_id();
        let tx_runtime_limits = EvmTxRuntimeLimits::from_spec(spec);
        let limits = Self::no_limits()
            .with_tx_runtime_limits(tx_runtime_limits)
            .with_block_gas_limit(block_gas_limit);
        match hardfork {
            MegaHardfork::Rex |
            MegaHardfork::Rex1 |
            MegaHardfork::Rex2 |
            MegaHardfork::Rex3 |
            MegaHardfork::Rex4 |
            MegaHardfork::Rex5 |
            MegaHardfork::Rex6 |
            MegaHardfork::Rex7 => Self {
                block_txs_data_limit: crate::constants::mini_rex::BLOCK_DATA_LIMIT,
                block_kv_update_limit: crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT,
                block_state_growth_limit: crate::constants::rex::BLOCK_STATE_GROWTH_LIMIT,
                ..limits
            },
            MegaHardfork::MiniRex | MegaHardfork::MiniRex2 => Self {
                block_txs_data_limit: crate::constants::mini_rex::BLOCK_DATA_LIMIT,
                block_kv_update_limit: crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT,
                ..limits
            },
            MegaHardfork::MiniRex1 => limits,
        }
    }
}

impl BlockLimits {
    /// Sets the related fields
    pub fn with_tx_runtime_limits(mut self, limits: EvmTxRuntimeLimits) -> Self {
        self.tx_data_limit = limits.tx_data_size_limit;
        self.tx_kv_update_limit = limits.tx_kv_updates_limit;
        self.tx_compute_gas_limit = limits.tx_compute_gas_limit;
        self.tx_state_growth_limit = limits.tx_state_growth_limit;
        self.block_env_access_compute_gas_limit = limits.block_env_access_compute_gas_limit;
        self.oracle_access_compute_gas_limit = limits.oracle_access_compute_gas_limit;
        self
    }

    /// Set a custom transaction gas limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction gas limit. Transactions with gas limits
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
    ///     .with_tx_gas_limit(15_000_000);
    /// ```
    pub fn with_tx_gas_limit(mut self, limit: u64) -> Self {
        self.tx_gas_limit = limit;
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

    /// Set a custom transaction encode size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction encode size limit.
    pub fn with_tx_encode_size_limit(mut self, limit: u64) -> Self {
        self.tx_encode_size_limit = limit;
        self
    }

    /// Set a custom block transactions encode size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified block transactions encode size limit.
    pub fn with_block_txs_encode_size_limit(mut self, limit: u64) -> Self {
        self.block_txs_encode_size_limit = limit;
        self
    }

    /// Set a custom transaction data availability size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction data availability size limit.
    pub fn with_tx_da_size_limit(mut self, limit: u64) -> Self {
        self.tx_da_size_limit = limit;
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

    /// Set a custom transaction data limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction data limit.
    pub fn with_tx_data_limit(mut self, limit: u64) -> Self {
        self.tx_data_limit = limit;
        self
    }

    /// Set a custom block transactions data limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified block transactions data limit.
    pub fn with_block_txs_data_limit(mut self, limit: u64) -> Self {
        self.block_txs_data_limit = limit;
        self
    }

    /// Set a custom transaction KV update limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction KV update limit.
    pub fn with_tx_kv_update_limit(mut self, limit: u64) -> Self {
        self.tx_kv_update_limit = limit;
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

    /// Set a custom transaction compute gas limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction compute gas limit.
    pub fn with_tx_compute_gas_limit(mut self, limit: u64) -> Self {
        self.tx_compute_gas_limit = limit;
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

    /// Set a custom transaction state growth limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction state growth limit.
    pub fn with_tx_state_growth_limit(mut self, limit: u64) -> Self {
        self.tx_state_growth_limit = limit;
        self
    }

    /// Set a custom block state growth limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified block state growth limit.
    pub fn with_block_state_growth_limit(mut self, limit: u64) -> Self {
        self.block_state_growth_limit = limit;
        self
    }

    /// Set a custom compute gas limit when accessing block environment data.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified block environment access compute gas limit.
    pub fn with_block_env_access_compute_gas_limit(mut self, limit: u64) -> Self {
        self.block_env_access_compute_gas_limit = limit;
        self
    }

    /// Set a custom compute gas limit when accessing oracle data.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified oracle access compute gas limit.
    pub fn with_oracle_access_compute_gas_limit(mut self, limit: u64) -> Self {
        self.oracle_access_compute_gas_limit = limit;
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
    /// let limits = BlockLimits::default().with_block_txs_data_limit(5_000);
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
            block_compute_gas_enforced: 0,
            block_state_growth_used: 0,
        }
    }

    /// Convert the block limits to the runtime limits for a single transaction.
    pub fn to_evm_tx_runtime_limits(&self) -> EvmTxRuntimeLimits {
        EvmTxRuntimeLimits {
            tx_data_size_limit: self.tx_data_limit,
            tx_kv_updates_limit: self.tx_kv_update_limit,
            tx_compute_gas_limit: self.tx_compute_gas_limit,
            tx_state_growth_limit: self.tx_state_growth_limit,
            block_env_access_compute_gas_limit: self.block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit: self.oracle_access_compute_gas_limit,
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
///     limiter.pre_execution_check(tx.hash(), tx.gas_limit(), tx.size(), tx.da_size(), is_deposit)?;
///
///     let outcome = execute_transaction(tx);
///
///     // Post-execution update (the executor commit path drives this internally)
///     limiter.post_execution_update_raw(
///         gas, tx_size, da_size, data, kv, compute, destroyed_compute, growth, is_deposit,
///     );
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
    /// This includes the encoded transaction object, logs, return data, and other execution
    /// artifacts.
    pub block_data_used: u64,

    /// Cumulative key-value storage updates performed by all transactions in the block.
    ///
    /// This tracks the total number of SSTORE operations across all transactions.
    pub block_kv_updates_used: u64,

    /// Cumulative compute gas consumed by all transactions in the block, as reported.
    ///
    /// This is the block's public compute-gas statistic: every transaction's full reported total,
    /// including the remainders Rex7+ exceptionally halted frames destroyed rather than performed.
    /// Admission does not read it —
    /// [`block_compute_gas_enforced`](Self::block_compute_gas_enforced) is the counter the
    /// block compute-gas limit is evaluated against.
    pub block_compute_gas_used: u64,

    /// The part of [`block_compute_gas_used`](Self::block_compute_gas_used) the block enforces:
    /// the same total with each transaction's destroyed remainder subtracted.
    ///
    /// Destroyed gas is not work the network performed, and no resource limit is evaluated against
    /// it at any level. A transaction whose reported total dwarfs its executed work would
    /// otherwise close the block's compute capacity for everyone behind it while having computed
    /// almost nothing. Before Rex7 nothing is ever destroyed, so this counter and the reported one
    /// advance in lockstep.
    pub block_compute_gas_enforced: u64,

    /// Cumulative state growth consumed by all transactions in the block.
    pub block_state_growth_used: u64,
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
            block_compute_gas_enforced: 0,
            block_state_growth_used: 0,
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
    /// Call [`post_execution_update_raw`](Self::post_execution_update_raw) after execution to
    /// update usage counters.
    ///
    /// # Parameters
    ///
    /// - `tx_hash`: Transaction hash for error reporting
    /// - `gas_limit`: Transaction's declared gas limit
    /// - `tx_size`: Transaction's encoded size in bytes (EIP-2718 encoding)
    /// - `da_size`: Transaction's compressed data availability size in bytes
    /// - `is_deposit`: Whether the transaction is an L1-to-L2 deposit (exempt from DA size limits)
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
    ///     tx.is_deposit(),
    /// )?;
    /// ```
    pub fn pre_execution_check(
        &self,
        tx_hash: TxHash,
        gas_limit: u64,
        tx_size: u64,
        da_size: u64,
        is_deposit: bool,
    ) -> Result<(), BlockExecutionError> {
        // Check single transaction gas limit
        if gas_limit > self.limits.tx_gas_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaTxLimitExceededError::TransactionGasLimit {
                    tx_gas_limit: gas_limit,
                    limit: self.limits.tx_gas_limit,
                }),
            }));
        }

        // Check block gas limit
        if self.block_gas_used.saturating_add(gas_limit) > self.limits.block_gas_limit {
            return Err(BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: gas_limit,
                    block_available_gas: self
                        .limits
                        .block_gas_limit
                        .saturating_sub(self.block_gas_used),
                },
            ));
        }

        // Check single transaction size limit
        if tx_size > self.limits.tx_encode_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaTxLimitExceededError::TransactionEncodeSizeLimit {
                    tx_size,
                    limit: self.limits.tx_encode_size_limit,
                }),
            }));
        }

        // Check block transaction size limit
        if tx_size.saturating_add(self.block_tx_size_used) > self.limits.block_txs_encode_size_limit
        {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::TransactionEncodeSizeLimit {
                    block_used: self.block_tx_size_used,
                    tx_used: tx_size,
                    limit: self.limits.block_txs_encode_size_limit,
                }),
            }));
        }

        // Deposit transactions are exempt from data availability size limits
        if !is_deposit {
            // Check single transaction data availability size limit
            if da_size > self.limits.tx_da_size_limit {
                return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    hash: tx_hash,
                    error: Box::new(MegaTxLimitExceededError::DataAvailabilitySizeLimit {
                        da_size,
                        limit: self.limits.tx_da_size_limit,
                    }),
                }));
            }

            // Check block data availability size limit
            if da_size.saturating_add(self.block_da_size_used) > self.limits.block_da_size_limit {
                return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    hash: tx_hash,
                    error: Box::new(MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                        block_used: self.block_da_size_used,
                        tx_used: da_size,
                        limit: self.limits.block_da_size_limit,
                    }),
                }));
            }
        }

        // Check block-level data limit
        if self.block_data_used >= self.limits.block_txs_data_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::TransactionDataLimit {
                    block_used: self.block_data_used,
                    limit: self.limits.block_txs_data_limit,
                }),
            }));
        }

        // Check block-level kv update limit
        if self.block_kv_updates_used >= self.limits.block_kv_update_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::KVUpdateLimit {
                    block_used: self.block_kv_updates_used,
                    limit: self.limits.block_kv_update_limit,
                }),
            }));
        }

        // Check block-level compute gas limit. The enforced counter is the one compared, and so
        // the one the error reports: destroyed remainders are reported in
        // `block_compute_gas_used` but never close the block's compute capacity.
        if self.block_compute_gas_enforced >= self.limits.block_compute_gas_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::ComputeGasLimit {
                    block_used: self.block_compute_gas_enforced,
                    limit: self.limits.block_compute_gas_limit,
                }),
            }));
        }

        // Check block-level state growth limit
        if self.block_state_growth_used >= self.limits.block_state_growth_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::StateGrowthLimit {
                    block_used: self.block_state_growth_used,
                    limit: self.limits.block_state_growth_limit,
                }),
            }));
        }

        Ok(())
    }

    /// Update usage counters after transaction execution.
    ///
    /// Called **after** transaction execution to accumulate the executed transaction's resource
    /// usage into the limiter's block-level counters. It does **not** validate post-execution
    /// limits — over-limit enforcement happens in
    /// [`pre_execution_check`](Self::pre_execution_check) before admitting the next transaction;
    /// the transaction may push the block over a limit, which is intentional to maximize block
    /// utilization. `is_deposit` gates only the DA-size counter: deposits are exempt from DA
    /// accounting.
    ///
    /// Compute gas arrives as two numbers, not one: `compute_gas_used` is the transaction's full
    /// reported total and `compute_gas_destroyed` is the part of it that Rex7+ destroyed rather
    /// than performed (0 before Rex7). The reported total lands in the public statistic and the
    /// difference in the counter the block compute-gas limit is evaluated against. Both come from
    /// the transaction's own report, so the block inherits whatever the transaction settled — it
    /// does not re-derive the split, and there is no second definition of it here.
    #[allow(clippy::too_many_arguments)]
    pub fn post_execution_update_raw(
        &mut self,
        gas_used: u64,
        tx_size: u64,
        da_size: u64,
        tx_data: u64,
        kv_updates: u64,
        compute_gas_used: u64,
        compute_gas_destroyed: u64,
        state_growth_used: u64,
        is_deposit: bool,
    ) {
        // Block gas limit. No need to check here since it's checked before transaction execution.
        self.block_gas_used = self.block_gas_used.saturating_add(gas_used);

        // Block tx size limit, no need to check here since it's checked before transaction
        // execution.
        self.block_tx_size_used = self.block_tx_size_used.saturating_add(tx_size);

        // Block da size limit, no need to check here since it's checked before transaction
        // execution. Only appliable for non-deposit transactions.
        if !is_deposit {
            self.block_da_size_used = self.block_da_size_used.saturating_add(da_size);
        }

        // Block data limit, no need to check here since we allow the last transaction to exceed the
        // limit.
        self.block_data_used = self.block_data_used.saturating_add(tx_data);

        // Block kv updates limit, no need to check here since we allow the last transaction to
        // exceed the limit.
        self.block_kv_updates_used = self.block_kv_updates_used.saturating_add(kv_updates);

        // Block compute gas limit, no need to check here since we allow the last transaction to
        // exceed the limit. Only the executed part advances the enforced counter.
        self.block_compute_gas_used = self.block_compute_gas_used.saturating_add(compute_gas_used);
        self.block_compute_gas_enforced = self
            .block_compute_gas_enforced
            .saturating_add(compute_gas_used.saturating_sub(compute_gas_destroyed));

        // Block state growth limit, no need to check here since we allow the last transaction to
        // exceed the limit.
        self.block_state_growth_used =
            self.block_state_growth_used.saturating_add(state_growth_used);
    }

    /// Returns true if any block-level limit has been reached or exceeded.
    ///
    /// Compute gas answers on the enforced counter, matching what
    /// [`pre_execution_check`](Self::pre_execution_check) would reject the next transaction on.
    pub fn is_block_limit_reached(&self) -> bool {
        self.block_gas_used >= self.limits.block_gas_limit ||
            self.block_tx_size_used >= self.limits.block_txs_encode_size_limit ||
            self.block_da_size_used >= self.limits.block_da_size_limit ||
            self.block_data_used >= self.limits.block_txs_data_limit ||
            self.block_kv_updates_used >= self.limits.block_kv_update_limit ||
            self.block_compute_gas_enforced >= self.limits.block_compute_gas_limit ||
            self.block_state_growth_used >= self.limits.block_state_growth_limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    fn limits_with_block_gas(block_gas_limit: u64) -> BlockLimits {
        let mut limits = BlockLimits::no_limits();
        limits.block_gas_limit = block_gas_limit;
        limits.tx_gas_limit = u64::MAX;
        limits
    }

    fn limits_with_block_tx_size(block_txs_encode_size_limit: u64) -> BlockLimits {
        let mut limits = BlockLimits::no_limits();
        limits.block_txs_encode_size_limit = block_txs_encode_size_limit;
        limits
    }

    fn limits_with_block_da_size(block_da_size_limit: u64) -> BlockLimits {
        let mut limits = BlockLimits::no_limits();
        limits.block_da_size_limit = block_da_size_limit;
        limits
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
        let mut limiter = BlockLimiter::new(limits_with_block_tx_size(1_000_000));
        limiter.block_tx_size_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, 0, u64::MAX, 0, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_pre_execution_check_block_da_size_addition_saturates() {
        let mut limiter = BlockLimiter::new(limits_with_block_da_size(1_000_000));
        limiter.block_da_size_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, 0, 0, u64::MAX, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_post_execution_update_raw_saturates_all_counters() {
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        limiter.block_gas_used = u64::MAX - 1;
        limiter.block_tx_size_used = u64::MAX - 1;
        limiter.block_da_size_used = u64::MAX - 1;
        limiter.block_data_used = u64::MAX - 1;
        limiter.block_kv_updates_used = u64::MAX - 1;
        limiter.block_compute_gas_used = u64::MAX - 1;
        limiter.block_compute_gas_enforced = u64::MAX - 1;
        limiter.block_state_growth_used = u64::MAX - 1;

        limiter.post_execution_update_raw(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            0,
            u64::MAX,
            false,
        );

        assert_eq!(limiter.block_gas_used, u64::MAX);
        assert_eq!(limiter.block_tx_size_used, u64::MAX);
        assert_eq!(limiter.block_da_size_used, u64::MAX);
        assert_eq!(limiter.block_data_used, u64::MAX);
        assert_eq!(limiter.block_kv_updates_used, u64::MAX);
        assert_eq!(limiter.block_compute_gas_used, u64::MAX);
        assert_eq!(limiter.block_compute_gas_enforced, u64::MAX);
        assert_eq!(limiter.block_state_growth_used, u64::MAX);
    }

    #[test]
    fn test_post_execution_update_raw_skips_da_for_deposits() {
        // Deposit transactions should not advance `block_da_size_used`, even when the value
        // would otherwise saturate.
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        limiter.block_da_size_used = 100;

        limiter.post_execution_update_raw(0, 0, u64::MAX, 0, 0, 0, 0, 0, true);

        assert_eq!(limiter.block_da_size_used, 100);
    }

    /// The two compute-gas counters accumulate different things: the reported one takes the
    /// transaction's whole total, the enforced one only the part the transaction performed. A
    /// destroyed remainder that leaked into the enforced counter would close the block's compute
    /// capacity for work that never happened.
    #[test]
    fn test_post_execution_update_raw_splits_the_compute_gas_lanes() {
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());

        limiter.post_execution_update_raw(0, 0, 0, 0, 0, 1_000_000, 900_000, 0, false);
        assert_eq!(limiter.block_compute_gas_used, 1_000_000, "the report takes the whole total");
        assert_eq!(limiter.block_compute_gas_enforced, 100_000, "enforcement takes only the work");

        // A second transaction that destroyed nothing advances both counters by the same amount.
        limiter.post_execution_update_raw(0, 0, 0, 0, 0, 50_000, 0, 0, false);
        assert_eq!(limiter.block_compute_gas_used, 1_050_000);
        assert_eq!(limiter.block_compute_gas_enforced, 150_000);
    }

    /// Nothing is ever destroyed before Rex7, so a block of transactions that report a zero
    /// destroyed part leaves the two counters equal at every step — the pre-Rex7 behaviour, which
    /// the split must reproduce byte for byte.
    #[test]
    fn test_compute_gas_lanes_coincide_without_a_destroyed_part() {
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());

        for compute in [21_000, 500, 1_234_567, 0] {
            limiter.post_execution_update_raw(0, 0, 0, 0, 0, compute, 0, 0, false);
            assert_eq!(
                limiter.block_compute_gas_used, limiter.block_compute_gas_enforced,
                "with nothing destroyed the reported and enforced counters must not diverge"
            );
        }
    }

    /// Admission compares the enforced counter, and the error it raises must state that same
    /// number: a rejected transaction's operator reads `block_used` to understand what filled the
    /// block, and the reported total would name a budget the block never spent.
    #[test]
    fn test_block_compute_gas_admission_reads_the_enforced_counter() {
        let mut limits = BlockLimits::no_limits();
        limits.block_compute_gas_limit = 1_000_000;
        let mut limiter = BlockLimiter::new(limits);

        // One transaction reporting far past the block limit, having performed almost none of it.
        limiter.post_execution_update_raw(0, 0, 0, 0, 0, 5_000_000, 4_950_000, 0, false);
        assert!(
            limiter.block_compute_gas_used > limits.block_compute_gas_limit,
            "the reported total must carry the destroyed remainder past the limit"
        );
        assert!(
            !limiter.is_block_limit_reached(),
            "a destroyed remainder must not fill the block's compute capacity"
        );
        assert!(
            limiter.pre_execution_check(B256::ZERO, 0, 0, 0, false).is_ok(),
            "the next transaction must still be admitted"
        );

        // Executed work fills it, and the error names the enforced counter.
        limiter.post_execution_update_raw(0, 0, 0, 0, 0, 950_000, 0, 0, false);
        assert!(limiter.is_block_limit_reached(), "executed work does fill the block");
        let error = limiter
            .pre_execution_check(B256::ZERO, 0, 0, 0, false)
            .expect_err("a full block must reject the next transaction");
        let message = format!("{error:?}");
        assert!(
            message.contains("ComputeGasLimit") && message.contains("block_used: 1000000"),
            "the error must report the counter that was compared, got {message}"
        );
    }
}
