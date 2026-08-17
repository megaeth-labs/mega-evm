//! REX6 checkpoint compute-gas accounting with V0 gas-clamp enforcement (prototype).
//!
//! Plain opcodes run the raw revm instructions with no per-opcode recording; compute gas
//! settles as an interpreter-gas delta at each checkpoint (storage-gas opcodes, CALL/CREATE
//! family, volatile opcodes, `GAS`, frame entry/exit). Enforcement inside plain segments is
//! the V0 gas clamp: the interpreter's visible gas is clamped to the compute headroom, so
//! revm's own per-opcode gas checks stop a crossing opcode at the clamp boundary *before it
//! executes* — zero overshoot.
//!
//! These tests pin the three sides of the design:
//!
//! - **Precision invariant**: non-exceeding transactions produce accounting totals (and receipts)
//!   bit-identical to per-opcode recording, including under an active clamp (`GAS` observability).
//! - **Exact enforcement**: a crossing inside a plain segment halts at the crossing opcode with its
//!   cost excluded — earlier than per-opcode enforcement, which executes the crossing opcode first.
//! - **Bounded loops**: a detention cap inside a checkpoint-free arithmetic loop is enforced by the
//!   clamp, not deferred to frame end.

use crate::common::{transact, transact_default, CALLER, CONTRACT};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{GAS, MSTORE, POP, RETURN, SSTORE, STOP, TIMESTAMP};

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

fn db_with_code(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
}

/// Measures the intrinsic compute gas (tx base cost) with a STOP-only contract.
fn intrinsic_compute_gas() -> u64 {
    transact_default(
        MegaSpecId::REX6,
        db_with_code(BytecodeBuilder::default().append(STOP).build()),
    )
    .compute_gas
}

/// A countdown loop of cheap opcodes with no checkpoint inside the loop body:
///
/// ```text
/// [prefix] PUSH2 iterations; loop: JUMPDEST; PUSH1 1; SWAP1; SUB; DUP1; PUSH1 loop; JUMPI; STOP
/// ```
///
/// Each iteration executes 7 plain opcodes for 26 gas. `prefix` is prepended verbatim and
/// participates in the jump-target offset.
fn countdown_loop_code_with_prefix(prefix: &[u8], iterations: u16) -> Bytes {
    let mut code = prefix.to_vec();
    code.push(0x61); // PUSH2
    code.extend_from_slice(&iterations.to_be_bytes());
    let loop_target = code.len() as u8;
    code.push(0x5b); // JUMPDEST
    code.extend_from_slice(&[0x60, 0x01]); // PUSH1 1
    code.push(0x90); // SWAP1
    code.push(0x03); // SUB
    code.push(0x80); // DUP1
    code.extend_from_slice(&[0x60, loop_target]); // PUSH1 loop
    code.push(0x57); // JUMPI
    code.push(0x00); // STOP
    Bytes::from(code)
}

/// A straight line of `pairs` PUSH1/POP pairs (5 gas, 2 plain opcodes each), then
/// `SSTORE(7, 99)` and STOP.
fn plain_run_then_sstore_code(pairs: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..pairs {
        builder = builder.push_number(1u64);
        builder = builder.append(POP);
    }
    builder
        .push_u256(U256::from(99u64)) // value
        .push_u256(U256::from(7u64)) // slot
        .append(SSTORE)
        .append(STOP)
        .build()
}

/// Precision invariant on a plain-opcode hot loop: with no limit crossing, checkpoint
/// accounting (REX6) must produce the same compute-gas total and receipt `gas_used` as
/// per-opcode accounting (REX5) — the segment delta telescopes over exactly the opcodes the
/// per-opcode wrappers would have recorded, and the loop's only settlement is the frame-end
/// checkpoint.
#[test]
fn test_checkpoint_plain_loop_totals_match_rex5() {
    let code = countdown_loop_code_with_prefix(&[], 500);
    let r5 = transact_default(MegaSpecId::REX5, db_with_code(code.clone()));
    let r6 = transact_default(MegaSpecId::REX6, db_with_code(code));

    assert!(r5.is_success(), "REX5 loop must succeed: {:?}", r5.result);
    assert!(r6.is_success(), "REX6 loop must succeed: {:?}", r6.result);
    assert_eq!(
        r5.compute_gas, r6.compute_gas,
        "checkpoint totals must telescope to the per-opcode sum"
    );
    assert_eq!(r5.gas_used, r6.gas_used, "receipt gas must be unchanged");
}

/// `GAS` observability under an active clamp: with a tight detention cap the interpreter's
/// visible gas is clamped for the whole post-access run, yet the value `GAS` pushes must be
/// the true remaining — the checkpoint prologue restores the clamp before the raw
/// instruction reads the counter. The returned word and the receipt must be bit-identical
/// to per-opcode REX5, where no clamp exists at all.
#[test]
fn test_v0_clamp_is_unobservable_via_gas_opcode() {
    // TIMESTAMP; POP; GAS; PUSH1 0; MSTORE; PUSH1 32; PUSH1 0; RETURN
    let code =
        Bytes::from(vec![TIMESTAMP, POP, GAS, 0x60, 0x00, MSTORE, 0x60, 0x20, 0x60, 0x00, RETURN]);
    // Detention cap 1,000: far below the interpreter's remaining gas, so the clamp is
    // active at the GAS opcode; the tx itself stays far below the cap (non-exceeding).
    let limits = |spec| {
        let mut l = EvmTxRuntimeLimits::from_spec(spec);
        l.block_env_access_compute_gas_limit = 1_000;
        l
    };

    let r5 = transact(MegaSpecId::REX5, db_with_code(code.clone()), limits(MegaSpecId::REX5));
    let r6 = transact(MegaSpecId::REX6, db_with_code(code), limits(MegaSpecId::REX6));

    assert!(r5.is_success(), "REX5 must succeed: {:?}", r5.result);
    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert_eq!(
        r5.result.output(),
        r6.result.output(),
        "GAS must push the true remaining under the clamp"
    );
    assert_eq!(r5.compute_gas, r6.compute_gas, "compute totals must be identical");
    assert_eq!(r5.gas_used, r6.gas_used, "receipt gas must be identical");
}

/// V0 exact enforcement in a plain segment: the limit is placed mid-way through a straight
/// plain-opcode run. REX5 (per-opcode) executes the crossing opcode and then halts, so its
/// recorded usage exceeds the limit; REX6 (clamp) fake-OOGs the crossing opcode at the
/// clamp boundary *before executing it*, so its recorded usage stays at or below the limit
/// and the halt lands one opcode earlier — zero overshoot.
#[test]
fn test_v0_clamp_halts_at_crossing_opcode_without_executing_it() {
    let code = plain_run_then_sstore_code(200); // 200 * 5 = 1,000 gas of plain opcodes
    let intrinsic = intrinsic_compute_gas();

    // Trip the limit ~300 gas into the 1,000-gas plain run, well before the SSTORE.
    let compute_limit = intrinsic + 300;
    let limits =
        |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(compute_limit);

    let r5 = transact(MegaSpecId::REX5, db_with_code(code.clone()), limits(MegaSpecId::REX5));
    let r6 = transact(MegaSpecId::REX6, db_with_code(code), limits(MegaSpecId::REX6));

    assert!(!r5.is_success(), "REX5 must stop on the tight compute limit: {:?}", r5.result);
    assert!(!r6.is_success(), "REX6 must stop on the tight compute limit: {:?}", r6.result);

    // Per-opcode enforcement records the crossing opcode: usage strictly exceeds the limit.
    assert!(
        r5.compute_gas > compute_limit,
        "REX5 executes the crossing opcode before halting; compute={} limit={compute_limit}",
        r5.compute_gas
    );
    // Clamp enforcement stops the crossing opcode before execution: usage stays at or
    // below the limit.
    assert!(
        r6.compute_gas <= compute_limit,
        "REX6 must not execute the crossing opcode; compute={} limit={compute_limit}",
        r6.compute_gas
    );
    assert!(
        r6.compute_gas > compute_limit - 12,
        "REX6 must stop at the clamp boundary, not earlier; compute={} limit={compute_limit}",
        r6.compute_gas
    );
}

/// V0 bounds detention inside a checkpoint-free plain loop — the shape that broke V1
/// (which deferred enforcement to frame end, overshooting by the whole loop). The clamp
/// stops the loop at the cap boundary regardless of the absence of checkpoints.
#[test]
fn test_v0_clamp_bounds_detention_inside_plain_loop() {
    // TIMESTAMP marks volatile access (detention cap = usage + 1,000), then a
    // 10,000-iteration countdown loop (~260k gas) with no checkpoint inside.
    let code = countdown_loop_code_with_prefix(&[TIMESTAMP, POP], 10_000);
    let intrinsic = intrinsic_compute_gas();

    let limits = |spec| {
        let mut l = EvmTxRuntimeLimits::from_spec(spec);
        l.block_env_access_compute_gas_limit = 1_000;
        l
    };

    let r5 = transact(MegaSpecId::REX5, db_with_code(code.clone()), limits(MegaSpecId::REX5));
    let r6 = transact(MegaSpecId::REX6, db_with_code(code), limits(MegaSpecId::REX6));

    assert!(!r5.is_success(), "REX5 must halt on the detention cap: {:?}", r5.result);
    assert!(!r6.is_success(), "REX6 must halt on the detention cap: {:?}", r6.result);

    // The detained limit is usage-at-access + 1,000 ≈ intrinsic + TIMESTAMP + 1,000.
    let detained_limit_upper = intrinsic + 2 + 1_000;
    // Per-opcode enforcement executes the crossing opcode: usage strictly exceeds the cap.
    assert!(
        r5.compute_gas > detained_limit_upper - 12,
        "REX5 must stop near the detention cap; compute={}",
        r5.compute_gas
    );
    // Clamp enforcement stops at the boundary without executing the crossing opcode,
    // despite the loop containing no checkpoint at all.
    assert!(
        r6.compute_gas <= detained_limit_upper,
        "REX6 must not overshoot the detention cap; compute={} cap≈{detained_limit_upper}",
        r6.compute_gas
    );
    assert!(
        r6.compute_gas > detained_limit_upper - 30,
        "REX6 must stop at the clamp boundary, not earlier; compute={}",
        r6.compute_gas
    );

    // Attribution parity: a clamp-stopped detention exceed must classify as
    // `VolatileDataAccessOutOfGas` exactly like per-opcode enforcement, even though the
    // recorded usage never crossed the detained limit (the crossing opcode was stopped
    // before executing).
    let halt_reason = |label: &str, r: &crate::common::Outcome| match &r.result {
        revm::context::result::ExecutionResult::Halt { reason, .. } => format!("{reason:?}"),
        other => panic!("{label}: expected a halt, got {other:?}"),
    };
    let r5_reason = halt_reason("REX5", &r5);
    let r6_reason = halt_reason("REX6", &r6);
    assert!(
        r5_reason.starts_with("VolatileDataAccessOutOfGas"),
        "REX5 must attribute the halt to volatile detention; got {r5_reason}"
    );
    assert!(
        r6_reason.starts_with("VolatileDataAccessOutOfGas"),
        "REX6 clamp halt must keep the volatile detention attribution; got {r6_reason}"
    );
}
