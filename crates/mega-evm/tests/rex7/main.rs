//! Tests for the `REX7` spec.
//!
//! - `checkpoint_settlement` — checkpoint compute-gas settlement: per-transaction totals stay
//!   bit-identical to per-opcode recording, and the two places where the models diverge.
//! - `frame_loop_parity` — one case per branch of the frame lifecycle that can end a frame, each
//!   run through both frame loops and compared on everything a transaction produces, state
//!   included: the loops share one body, and an observation-only inspector adds nothing to it.
//! - `gas_clamp` — gas-clamp enforcement: a crossing opcode is stopped before it executes, and the
//!   resulting out-of-gas is restored and reclassified by the constraint that bound the clamp.
//! - `clamp_classification` — which constraint a clamp binds to, including the exact-value case,
//!   and the ABI payload / halt fields a clamp-induced exceed reports.
//! - `checkpoint_families` — one parity case per checkpoint opcode the REX7 table wires, so the set
//!   is covered exhaustively rather than through representatives.
//! - `create_code_deposit_charge` — a CREATE's canonical code-deposit compute gas is weighed before
//!   it is recorded, so a creation that fails at its frame exit is charged nothing for a deposit
//!   the EVM never makes.
//! - `inspector_settlement_window` — the two windows where a rewrite lands after the accounting
//!   that should have read it: a terminating opcode's `step_end`, whose counter edit reaches
//!   nobody, and a precompile's classification, whose split has to follow the callback rather than
//!   the recording site.
//! - `interception_gas` — the gas an inspector puts into a synthetic outcome, over the four sizings
//!   it can choose relative to the envelope it was handed, and the halt direction where the choice
//!   reaches nothing.
//! - `interceptor_resume` — the two ways a CALL returns without a child frame ever running: a
//!   system contract interceptor's synthetic result, and a precompile.
//! - `keyless_synthetic_halt` — the `KeylessDeploy` interceptor's synthetic halts: the two that
//!   keep the envelope book the unperformed part as destroyed, the rescued one books nothing.
//! - `latch_surfacing` — where a latched data-size / KV-update / state-growth exceed becomes a
//!   stop.
//! - `measured_inspector` — the shim every inspector is wrapped in: gas an inspector writes into an
//!   interpreter counter or a frame's gas limit is measured at the callback boundary, booked, and
//!   kept out of enforcement, with the clamp re-derived on the spot; reviving a failed creation is
//!   refused; an observation-only inspector is bit-identical to no inspector at all.
//! - `gas_surface` — the closed enumeration one level below the cheat matrix: every field of every
//!   gas-carrying object an inspector callback is handed, each with a verdict, pinned against what
//!   upstream's own `Debug` renders.
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
//! - `frame_init_reject_burn` — the budget a refused frame init decides the fate of: the halting
//!   rejections (a CREATE onto an occupied address) swallow it and book it as destroyed, the
//!   returning ones hand it back and book nothing, and a precompile is settled from what its own
//!   recording site staged; then through the deposit-receipt rewrite and the sandbox merge that run
//!   after the booking.
//! - `call_body_halt_charges` — what a CALL-family body already charged when it halts: the
//!   value-transfer surcharge and the return-range memory expansion stay in the open segment and
//!   settle as work, rather than being dropped by the wrapper's tail.
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
//! - `precompile_halt` — a precompile that halts exceptionally is split the same way an interpreter
//!   frame is, and at the same settlement point: executed work enforces, the unused forwarded
//!   envelope does not.
//! - `pre_execution_intrinsic_reject` — the one envelope-keeping synthetic halt REX7 cannot reach:
//!   for an ordinary transaction an intrinsic overrun is a validation error from REX5 on, and a
//!   validation error produces no receipt for any lane to account for.
//! - `refund_and_state_gas` — the two numbers on a receipt the conservation law cannot see: the
//!   EIP-3529 refund, measured at the callback boundary because the EVM produces refunds too, and
//!   the EIP-8037 state-gas dimension, settled from the transaction's final figures because
//!   `MegaETH` produces none of it and revm propagates it by replacement.
//! - `deposit_receipt_rewrite` — the transactions that break that last step. A failed OP deposit
//!   does get a receipt, rebuilt to report its whole gas limit after every settlement has run; the
//!   boundary that rebuilds it books the difference as destroyed without moving what enforces.
//! - `result_space_tripwire` — every `InstructionResult` variant has an explicit destroyed-
//!   remainder class (swallow / return / unreachable), with no catch-all, so a revm bump that adds
//!   a variant fails to compile until a human assigns it; the early-fail arms of frame init are
//!   listed beside it for the upgrade diff that those arms have no type-level tie to.

mod burn_split;
mod call_body_halt_charges;
mod charge_on_reject;
mod checkpoint_families;
mod checkpoint_settlement;
mod checkpoint_static_fee_edges;
mod clamp_classification;
mod common;
mod conservation_terms;
mod create_code_deposit_charge;
mod deposit_receipt_rewrite;
mod detention_window;
mod double_exceed_corner;
mod exceptional_halt;
mod frame_init_reject_burn;
mod frame_loop_parity;
mod gas_clamp;
mod gas_leakage;
mod gas_surface;
mod guard_pass_static_gas;
mod inspector_cheat_matrix;
mod inspector_settlement_window;
mod interception_gas;
mod interceptor_resume;
mod keyless_synthetic_halt;
mod latch_surfacing;
mod late_frame_local;
mod measured_inspector;
mod modexp_gas;
mod opcode_set_parity;
mod parity_shapes;
mod pre_execution_intrinsic_reject;
mod precompile_halt;
mod refund_and_state_gas;
mod result_space_tripwire;
