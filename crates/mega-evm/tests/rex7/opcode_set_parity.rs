//! REX7: the opcode set the table exposes must be exactly the one REX6 exposes.
//!
//! Every table through REX6 is built from scratch — `mini_rex` starts from an
//! all-`control::unknown` array and wires the opcodes it supports — so an opcode revm gains in a
//! future release stays unknown there until someone wires it. The REX7 table starts from revm's own
//! table instead, which makes the same release silently hand REX7 an opcode REX6 does not have.
//! That is a fail-open construction, and the four slots REX7 inherits back from REX6 (`DUPN`,
//! `SWAPN`, `EXCHANGE`, `SLOTNUM`) were found by reading revm's table by hand.
//!
//! This file replaces that reading with a check. It probes all 256 opcodes under both specs and
//! asserts they agree on one bit: whether the table dispatched `control::unknown`.
//!
//! ## What the probe reads, and what it does not
//!
//! The probe is a one-byte contract holding the opcode alone, run as a whole transaction. Its stack
//! is empty, so a supported opcode usually halts on a stack underflow — the probe deliberately does
//! not compare the two specs' full results, only whether each reports `OpcodeNotFound`.
//! `control::unknown` is the only handler that produces it, so the bit says exactly which slots
//! hold that handler, and nothing about how the surrounding wrappers differ.
//!
//! Two limits follow from reading a single bit. An opcode revm activates for the Ethereum spec
//! `MegaSpecId` maps to is caught, because REX7 would then execute it (or reject it some other way)
//! while REX6 still answers `OpcodeNotFound` — but an opcode revm wires and gates behind a later
//! fork is caught only because its `NotActivated` rejection is a different reason, not because the
//! opcode sets truly diverge. And an immediate-taking opcode whose immediate fails to decode maps
//! to `OpcodeNotFound` in revm, so an activated opcode of that shape could read as unknown here.
//! Both are conservative in the direction that matters: the check fails loudly and asks for an
//! explicit decision rather than passing quietly.

use crate::common::{transact_default, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::MemoryDatabase, EthHaltReason, MegaHaltReason, MegaSpecId, OpHaltReason,
};
use revm::{bytecode::opcode::OpCode, context::result::ExecutionResult};

/// Runs the one-byte probe for `opcode` under `spec` and reports whether the table dispatched
/// `control::unknown`.
fn dispatches_unknown(spec: MegaSpecId, opcode: u8) -> bool {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, Bytes::from(vec![opcode]));
    matches!(
        transact_default(spec, db).result,
        ExecutionResult::Halt {
            reason: MegaHaltReason::Base(OpHaltReason::Base(EthHaltReason::OpcodeNotFound)),
            ..
        }
    )
}

/// Names `opcode` for a failure message, falling back to its hex value when revm does not know it.
fn opcode_name(opcode: u8) -> String {
    OpCode::new(opcode).map_or_else(|| format!("{opcode:#04x}"), |op| op.as_str().to_string())
}

/// The guard: no opcode may be unknown under one spec and dispatched under the other.
#[test]
fn test_rex7_opcode_set_matches_rex6() {
    let divergent: Vec<String> = (0..=u8::MAX)
        .filter_map(|opcode| {
            let rex6 = dispatches_unknown(MegaSpecId::REX6, opcode);
            let rex7 = dispatches_unknown(MegaSpecId::REX7, opcode);
            (rex6 != rex7).then(|| {
                let known_to = if rex7 { "REX6" } else { "REX7" };
                format!("{} ({opcode:#04x}): dispatched only by {known_to}", opcode_name(opcode))
            })
        })
        .collect();

    assert!(
        divergent.is_empty(),
        "REX7 must expose the same opcode set as REX6, but they disagree on:\n  {}\n\
         Wire the opcode into both tables, or inherit the slot from REX6 in `mod rex7`.",
        divergent.join("\n  ")
    );
}

/// The probe has to be able to answer both ways, or the guard above passes vacuously — a run where
/// every transaction failed before reaching the opcode would report no divergence at all.
#[test]
fn test_opcode_probe_discriminates_unknown_from_dispatched() {
    // 0x0c is unassigned in the EVM: unknown under both specs, in revm's table and in MegaETH's.
    const UNASSIGNED: u8 = 0x0c;
    // `PUSH0` needs no operands, so it runs to completion under both.
    let dispatched = revm::bytecode::opcode::PUSH0;

    for spec in [MegaSpecId::REX6, MegaSpecId::REX7] {
        assert!(
            dispatches_unknown(spec, UNASSIGNED),
            "{spec:?}: an unassigned opcode must report OpcodeNotFound"
        );
        assert!(
            !dispatches_unknown(spec, dispatched),
            "{spec:?}: PUSH0 must not report OpcodeNotFound"
        );
    }
}
