//! Tests for the `REX7` spec.
//!
//! - `checkpoint_settlement` — checkpoint compute-gas settlement: per-transaction totals stay
//!   bit-identical to per-opcode recording, and the two places where the models diverge.
//! - `v0_clamp` — V0 gas-clamp enforcement: a crossing opcode is stopped before it executes, and
//!   the resulting out-of-gas is restored and reclassified by the constraint that bound the clamp.

mod checkpoint_settlement;
mod common;
mod modexp_gas;
mod v0_clamp;
