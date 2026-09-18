//! The EVM implementation for the `MegaETH` Satin engine.
//!
//! Satin runs a single spec, [`MegaSpecId::SATIN`], on op-revm's Karst handler with EIP-8037
//! state gas and the EIP-2780 intrinsic cost. The legacy engine (specs `Equivalence` through
//! `Rex7`) is a separate crate line; nothing here executes a legacy spec.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

mod access;
mod block;
pub mod constants;
mod evm;
mod external;
mod limit;
pub mod system;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
mod types;

pub use block::*;
pub use evm::*;
pub use external::*;
pub use types::*;

/* Re-export of upstream crates, so consumers build against the exact versions used here */
pub use alloy_consensus;
pub use alloy_evm;
pub use alloy_hardforks;
pub use alloy_op_evm;
pub use alloy_primitives;
pub use alloy_sol_types;
pub use op_alloy_consensus;
pub use op_revm;
pub use revm;

/* Short aliases of the mega-evm types */
/// Alias for [`MegaSpecId`]
pub type SpecId = MegaSpecId;
/// Alias for [`MegaTransaction`]
pub type Transaction = MegaTransaction;
/// Alias for [`MegaHaltReason`]
pub type HaltReason = MegaHaltReason;
/// Alias for [`MegaTransactionError`]
pub type TransactionError = MegaTransactionError;
/// Alias for [`MegaTxType`]
pub type TxType = MegaTxType;
/// Alias for [`MegaEvm`]
pub type Evm<DB, INSP, ExtEnvs> = MegaEvm<DB, INSP, ExtEnvs>;
/// Alias for [`MegaEvmFactory`]
pub type EvmFactory<ExtEnvFactory> = MegaEvmFactory<ExtEnvFactory>;
/// Alias for [`MegaContext`]
pub type Context<DB, ExtEnvs> = MegaContext<DB, ExtEnvs>;
/// Alias for [`MegaBlockExecutor`]
pub type BlockExecutor<E> = MegaBlockExecutor<E>;
