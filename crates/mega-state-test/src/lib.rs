//! The execution-spec state-test runner of the Satin engine.
//!
//! It runs Ethereum's execution-spec state-test fixtures through [`MegaEvm`](mega_evm::MegaEvm)
//! in one of two [modes](Mode):
//!
//! - **Equivalence** — Satin's machinery (its handler, frame lifecycle, Host and instruction table)
//!   priced as the fixture's own fork prices it, with every dimension only `MegaETH` prices turned
//!   off (`MegaContext::with_neutral_cfg`). It is a gate: every test the fork's entries define
//!   passes, is skipped for a [reason the reference runner shares](skips), or fails on a
//!   [registered deviation](deviations) — a place Satin differs from Ethereum on purpose.
//! - **Satin** — the same fixtures under Satin's own configuration, counted by outcome. It is a
//!   report: Satin prices state, history and the access entries it pressed back on purpose, so
//!   almost every stateful fixture differs, and the count is what is worth watching.
//!
//! The fixture types are the revm fork's own ([`types`]), the ones its reference runner reads, so
//! the two runners parse, skip and count the same population.
//!
//! The [`blockchain`] module runs the execution-spec blockchain tests — chains of blocks, many
//! transactions to a block — through Satin's block executor in equivalence mode's configuration,
//! and judges every block's gas used, logs bloom, receipts root and state root, and every block the
//! fixture expects to be refused.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod blockchain;
pub mod deviations;
pub mod exceptions;
mod fork;
mod mode;
pub mod roots;
pub mod runner;
pub mod skips;
pub mod witness;

pub use fork::*;
pub use mode::*;

/// The execution-spec state-test fixture types, as the revm fork's reference runner reads
/// them.
pub use revm::statetest_types as types;
