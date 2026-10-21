//! Randomized tests of the Satin engine: properties it must hold on any input, and a differential
//! under the neutral configuration against the fork's op-revm and revm's mainnet EVM.
//!
//! The generators are in `gen`, the runner in `harness`; each property is one test. See the
//! module docs of `harness` for the two modes and how a failure is reproduced.

mod blocks;
mod differential;
mod gen;
mod harness;
mod keyless;
mod prices;
mod properties;
mod regressions;
mod render;
#[path = "../shared/snapshot.rs"]
mod snapshot;
