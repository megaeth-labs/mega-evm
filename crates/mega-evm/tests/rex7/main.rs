//! Tests for the `REX7` spec.
//!
//! - `checkpoint_settlement` — checkpoint compute-gas settlement: per-transaction totals stay
//!   bit-identical to per-opcode recording, and the two places where the models diverge.
//! - `gas_clamp` — gas-clamp enforcement: a crossing opcode is stopped before it executes, and the
//!   resulting out-of-gas is restored and reclassified by the constraint that bound the clamp.
//! - `clamp_classification` — which constraint a clamp binds to, including the exact-value case,
//!   and the ABI payload / halt fields a clamp-induced exceed reports.
//! - `checkpoint_families` — one parity case per checkpoint opcode the REX7 table wires, so the set
//!   is covered exhaustively rather than through representatives.
//! - `interceptor_resume` — the two ways a CALL returns without a child frame ever running: a
//!   system contract interceptor's synthetic result, and a precompile.
//! - `latch_surfacing` — where a latched data-size / KV-update / state-growth exceed becomes a
//!   stop.
//! - `gas_leakage` — the three paths a per-frame gas mechanism can leak through (interception,
//!   TX-level rescue, frame return), each with a clamp outstanding.
//! - `opcode_set_parity` — all 256 opcodes probed under both specs, so the REX7 table cannot gain
//!   an opcode REX6 does not have when revm's base table grows one.
//! - `parity_shapes` — parity on the transaction shapes that enter through a different door:
//!   EIP-7702 authorizations, the `KeylessDeploy` sandbox, system-originated (exempt) transactions,
//!   the REX5 storage-call stipend, and oracle hints.
//! - `double_exceed_corner` — the adjudicated corner swept one gas at a time, so the classification
//!   is shown to be stable rather than merely correct at one point.
//! - `exceptional_halt` — every exceptional-halt classification, in both frame positions: the
//!   frame's whole burned budget settles as compute gas without changing the receipt.
//! - `burn_split` — which half of that budget enforces: the work the frame performed does, the
//!   remainder it destroyed does not, and both boundaries (a checkpoint's storage charge, revm's
//!   post-action create rejects) land on the right side.
//! - `charge_on_reject` — a `disableVolatileDataAccess` rejection still pays the opcode's static
//!   fee, which segment settlement records as compute; REX6 keeps the historical zero-charge
//!   revert.
//! - `guard_pass_static_gas` — a passing guard charges the checkpoint static fee after the prologue
//!   restores true gas, so a compute headroom of `static_gas − 1` records the body rather than
//!   stopping at the clamp.
//! - `detention_window` — an underfunded CALL / EXTCODECOPY that OOGs before the target load does
//!   not mark beneficiary access; that order is specified, so the frozen-window tripwire stays
//!   silent.
//! - `checkpoint_static_fee_edges` — a table-prepaid checkpoint (`GAS`, `LOG1`) whose static fee
//!   exceeds the clamp headroom is a plain-segment crossing; `CREATE`'s 32,000 is charged inside
//!   the body, so the same headroom runs the body and then reverts.
//! - `precompile_halt` — a precompile that halts exceptionally is split at the recording site the
//!   same way an interpreter frame is: executed work enforces, the unused forwarded envelope does
//!   not.

mod burn_split;
mod charge_on_reject;
mod checkpoint_families;
mod checkpoint_settlement;
mod checkpoint_static_fee_edges;
mod clamp_classification;
mod common;
mod detention_window;
mod double_exceed_corner;
mod exceptional_halt;
mod gas_clamp;
mod gas_leakage;
mod guard_pass_static_gas;
mod interceptor_resume;
mod latch_surfacing;
mod modexp_gas;
mod opcode_set_parity;
mod parity_shapes;
mod precompile_halt;
