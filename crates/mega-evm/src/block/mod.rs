//! Block execution abstraction for the `MegaETH` EVM.
//!
//! This module provides block execution functionality specifically tailored for the `MegaETH`
//! chain, built on top of the Optimism EVM (`op-revm`) with MegaETH-specific customizations and
//! optimizations.
//!
//! # Architecture
//!
//! The block execution system consists of three main components:
//!
//! 2. **`BlockExecutorFactory`**: Factory for creating block executors with `MegaETH`
//!    specifications
//! 3. **`BlockExecutor`**: The actual executor that processes transactions within a block
//!
//! # EVM Specifications
//!
//! `MegaETH` supports two EVM specifications:
//!
//! - **`EQUIVALENCE`**: Maintains equivalence with Optimism Isthmus EVM (default)
//! - **`MINI_REX`**: Enhanced version with quadratic LOG costs and disabled SELFDESTRUCT
//!
//! # Performance Considerations
//!
//! The `MegaETH` block executor is optimized for high-performance blockchain operations:
//!
//! - Efficient delegation to the underlying Optimism EVM implementation
//! - Minimal overhead for MegaETH-specific features
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
