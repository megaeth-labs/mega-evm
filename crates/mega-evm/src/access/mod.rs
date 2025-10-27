//! Volatile data access tracking for the `MegaETH` EVM.
//!
//! Volatile data means those data that may be modified by the system (e.g., sequencer, oracle, or
//! payload builder). Once a transaction accesses volatile data, the system will immediate limit
//! the remaining gas in all message calls to a small amount of gas, forcing the transaction to
//! finish execution soon. These restrictions are necessary to prevent `DoS` attacks on EVM.

mod tracker;
mod volatile;

pub use tracker::*;
pub use volatile::*;
