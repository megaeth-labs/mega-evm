//! The EVM implementation for the `MegaETH` Satin engine.
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg_attr(not(feature = "std"), macro_use)]
#[cfg(not(feature = "std"))]
extern crate alloc;

mod block;
mod evm;
mod external;

pub use block::*;
pub use evm::*;
pub use external::*;
