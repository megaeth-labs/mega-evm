//! System contract and transaction helpers.
//!
//! # Pre-Block Helper Contract
//!
//! Deploy helpers and pre-block system-call helpers in this module
//! (`transact_deploy_*`, `transact_apply_pending_changes`, etc.) **must never** call
//! `db.commit(...)` directly. They return `Option<EvmState>` (or
//! `Option<ResultAndState<_>>`), and the block executor commits the collected outcomes
//! after running its `on_state` hook so the stateless witness generator captures the full
//! read/write set.
//!
//! Specifically:
//!
//! - Return `Ok(None)` only when the step is gated out entirely (wrong hardfork, etc.).
//! - On an idempotent "no change" path (e.g., the contract is already deployed with the correct
//!   code hash), return `Some(EvmState)` containing the observed account as a read-only entry
//!   (`Account { info, ..Default::default() }`, no `touched`/`created` flags). Returning `None`
//!   here loses the account from the witness read set.
//! - On a real-change path, include every account and slot the helper touched, including slots read
//!   in preparation for the change.
//!
//! See `crate::block` module docs for the executor side of this contract.

mod control;
mod intercept;
mod keyless_deploy;
mod limit_control;
mod oracle;
mod sequencer_registry;
mod tx;

pub use control::*;
pub use intercept::*;
pub use keyless_deploy::*;
pub use limit_control::*;
pub use oracle::*;
pub use sequencer_registry::*;
pub use tx::*;
