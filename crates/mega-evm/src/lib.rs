//! The EVM implementation for the `MegaETH`.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg_attr(not(feature = "std"), macro_use)]
#[cfg(not(feature = "std"))]
extern crate alloc;

mod access;
mod block;
pub mod constants;
mod evm;
mod external;
mod limit;
mod system;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;
mod types;

pub use access::*;
pub use block::*;
pub use evm::*;
pub use external::*;
pub use limit::*;
pub use system::*;
pub use types::*;

/* Re-export of upstream types */
pub use alloy_consensus;
pub use alloy_eips;
pub use alloy_evm;
pub use alloy_op_evm;
pub use alloy_op_hardforks;
pub use alloy_primitives;
pub use alloy_sol_types;
pub use op_alloy_consensus;
pub use op_alloy_flz;
pub use op_revm;
pub use revm::{self, context::either::Either, primitives::HashMap};

/* Alias of the mega-evm types */
/// Alias for [`MegaTransaction`]
pub type Transaction = MegaTransaction;
/// Alias for [`MegaSpecId`]
pub type SpecId = MegaSpecId;
/// Alias for [`MegaHaltReason`]
pub type HaltReason = MegaHaltReason;
/// Alias for [`MegaTransactionError`]
pub type TransactionError = MegaTransactionError;
/// Alias for [`MegaPrecompiles`]
pub type Precompiles = MegaPrecompiles;
/// Alias for [`MegaTxType`]
pub type TxType = MegaTxType;
/// Alias for [`MegaInstructions`]
pub type Instructions<DB, ExtEnvs> = MegaInstructions<DB, ExtEnvs>;
/// Alias for [`MegaHandler`]
pub type Handler<EVM, ERROR, FRAME> = MegaHandler<EVM, ERROR, FRAME>;
/// Alias for [`MegaEvm`]
pub type Evm<DB, INSP, ExtEnvs> = MegaEvm<DB, INSP, ExtEnvs>;
/// Alias for [`MegaEvmFactory`]
pub type EvmFactory<ExtEnvs> = MegaEvmFactory<ExtEnvs>;
/// Alias for [`MegaContext`]
pub type Context<DB, ExtEnvs> = MegaContext<DB, ExtEnvs>;
/// Alias for [`MegaBlockExecutor`]
pub type BlockExecutor<C, E, R> = MegaBlockExecutor<C, E, R>;
/// Alias for [`MegaBlockExecutorFactory`]
pub type BlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder> =
    MegaBlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder>;
