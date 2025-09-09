//! The EVM implementation for the `MegaETH`.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![allow(unused_imports)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg_attr(not(feature = "std"), macro_use)]
#[cfg(not(feature = "std"))]
extern crate alloc;

pub mod constants;

mod context;
pub use context::*;

mod block;
pub use block::*;

mod evm;
pub use evm::*;

mod gas;
pub use gas::*;

mod handler;
pub use handler::*;

mod host;
pub use host::*;

mod instructions;
pub use instructions::*;

mod limit;
pub use limit::*;

mod result;
pub use result::*;

mod spec;
pub use spec::*;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

mod types;
pub use types::*;

/* Alias of the mega-evm types */
/// Alias for [`MegaTransaction`]
pub type Transaction = MegaTransaction;
/// Alias for [`MegaSpecId`]
pub type SpecId = MegaSpecId;
/// Alias for [`MegaHaltReason`]
pub type HaltReason = MegaHaltReason;
/// Alias for [`MegaPrecompiles`]
pub type Precompiles = MegaPrecompiles;
/// Alias for [`MegaTxType`]
pub type TxType = MegaTxType;
/// Alias for [`MegaInstructions`]
pub type Instructions<DB, Oracle> = MegaInstructions<DB, Oracle>;
/// Alias for [`MegaHandler`]
pub type Handler<EVM, ERROR, FRAME> = MegaHandler<EVM, ERROR, FRAME>;
/// Alias for [`MegaEvm`]
pub type Evm<DB, INSP, Oracle> = MegaEvm<DB, INSP, Oracle>;
/// Alias for [`MegaEvmFactory`]
pub type EvmFactory<Oracle> = MegaEvmFactory<Oracle>;
/// Alias for [`MegaContext`]
pub type Context<DB, Oracle> = MegaContext<DB, Oracle>;
/// Alias for [`MegaBlockExecutor`]
pub type BlockExecutor<C, E, R> = MegaBlockExecutor<C, E, R>;
/// Alias for [`MegaBlockExecutorFactory`]
pub type BlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder> =
    MegaBlockExecutorFactory<ChainSpec, EvmF, ReceiptBuilder>;
