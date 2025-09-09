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

mod host;
pub use host::*;

mod limit;
pub use limit::*;

mod instructions;
pub use instructions::*;

mod handler;
pub use handler::*;

mod spec;
pub use spec::*;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

mod types;
pub use types::*;
