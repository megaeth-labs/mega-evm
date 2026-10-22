//! Whole-block replay, on either engine, compared with the chain.
//!
//! One driver loads a block's inputs (the block, its receipts and the state its parent left,
//! from a block cache or over RPC), picks the engine the block runs on, hands the engine plain
//! data, and compares the receipts it gets back with the chain's. The two executors
//! ([`satin`] and, with the `legacy` feature, `legacy`) exchange nothing but plain data with the
//! driver, so the driver, the cache and the comparison are written once for both engines.

mod cmd;
mod exec;
mod inputs;
#[cfg(feature = "legacy")]
mod legacy;
mod record;
mod satin;
mod state;

pub use cmd::*;
pub use exec::*;
pub use inputs::*;
pub use record::*;
pub use state::*;
