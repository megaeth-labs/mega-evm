//! Block execution abstraction for the `MegaETH` EVM.
//!
//! This module provides comprehensive block execution functionality specifically tailored for
//! the `MegaETH` chain, built on top of the Optimism EVM (`op-revm`) with MegaETH-specific
//! customizations and optimizations.
//!
//! # Architecture
//!
//! The block execution system consists of five main components:
//!
//! 1. **[`MegaBlockExecutorFactory`]**: Factory for creating block executors with `MegaETH`
//!    specifications
//! 2. **[`MegaBlockExecutor`]**: The actual executor that processes transactions within a block
//! 3. **[`BlockLimits`]**: Configuration for block-level resource limits
//! 4. **[`BlockLimiter`]**: Stateful tracker that enforces limits and accumulates resource usage
//! 5. **[`MegaBlockExecutionCtx`]**: Context for block execution including parent hash and limits
//!
//! # Resource Limit Enforcement
//!
//! `MegaETH` implements a comprehensive resource limiting system to prevent `DoS` attacks and
//! ensure fair resource allocation. The system enforces limits at two levels:
//!
//! ## Transaction-level Limits
//!
//! Individual transactions are validated against:
//! - Gas limit per transaction
//! - Transaction body size
//! - Data availability (DA) size
//!
//! ## Block-level Limits
//!
//! Cumulative resource usage across all transactions in a block is validated against:
//! - Total gas consumption
//! - Total transaction size
//! - Total DA size
//! - Total execution data (logs, return data)
//! - Total key-value storage updates
//!
//! Limits are checked in two phases:
//! - **Pre-execution**: Gas, transaction size, and DA size (fast, no execution needed)
//! - **Post-execution**: Data size and KV updates (after execution, before commit)
//!
//! # EVM Specifications
//!
//! `MegaETH` supports two EVM specifications:
//!
//! - **`EQUIVALENCE`**: Maintains equivalence with Optimism Isthmus EVM (default)
//! - **`MINI_REX`**: Enhanced version with additional resource limits and modified gas costs
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use mega_evm::{MegaBlockExecutorFactory, MegaBlockExecutionCtx, BlockLimits};
//!
//! // Configure block limits
//! let limits = BlockLimits::default()
//!     .with_block_txs_encode_size_limit(1_000_000)
//!     .with_block_txs_data_limit(10_000);
//!
//! // Create execution context
//! let ctx = MegaBlockExecutionCtx::new(parent_hash, parent_beacon_root, extra_data)
//!     .with_block_limits(limits);
//!
//! // Create executor factory
//! let factory = MegaBlockExecutorFactory::new(spec, evm_factory, receipt_builder);
//!
//! // Create and use executor
//! let mut executor = factory.create_executor_with_config(&mut state, ctx, evm_config);
//! executor.apply_pre_execution_changes()?;
//! executor.execute_transaction(tx)?;
//! let (evm, result) = executor.finish()?;
//! ```
//!
//! # Performance Considerations
//!
//! The `MegaETH` block executor is optimized for high-performance blockchain operations:
//!
//! - Efficient delegation to the underlying Optimism EVM implementation
//! - Minimal overhead for MegaETH-specific features
//! - Two-phase limit checking minimizes unnecessary execution
//! - Support for parallel execution through access tracking
//! - Optimized gas calculations for modified opcodes

mod executor;
mod factory;
mod helpers;
mod limit;
mod result;

pub use executor::*;
pub use factory::*;
pub use helpers::*;
pub use limit::*;
pub use result::*;
