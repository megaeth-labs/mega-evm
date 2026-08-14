//! A precompile that halts exceptionally is split the same way as an interpreter frame.
//!
//! A precompile runs inside `frame_init` and comes back as a result, so it never reaches the
//! interpreter-frame halt settlement. REX7 therefore splits it at the precompile recording site:
//!
//! - **Executed** — the work the precompile actually performed (the KZG fixed fee when verification
//!   ran; zero when the input was rejected before any work). This is enforcing.
//! - **Destroyed** — the rest of the parent-frame loss, which is the caller-supplied forwarded
//!   envelope, not the REX5-capped effective limit. This is reported and never enforced.
//!
//! Through REX6 the same recording site stays single-lane: success / revert still charge spent,
//! a KZG verification failure still charges the fixed fee, and every other error still charges
//! the (capped) limit as enforcing usage.

use crate::common::{transact, transact_default, Outcome, CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{address, Address, Bytes};
use mega_evm::{
    kzg_point_evaluation,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{CALL, INVALID, POP, STOP};

/// KZG point evaluation.
const KZG: Address = address!("000000000000000000000000000000000000000a");
/// blake2f. Rejects any input whose length is not 213 bytes, before charging anything.
const BLAKE2F: Address = address!("0000000000000000000000000000000000000009");

/// Gas every probed CALL forwards. Far above both precompiles' real costs, so the destroyed
/// remainder dominates every other term and the REX5 forwarded-gas cap is a no-op unless a
/// test tightens the compute limit.
const FORWARDED: u64 = 1_000_000;

/// A CALL forwarding [`FORWARDED`] gas to `target`, with 32 bytes of calldata from `mem[0..]` —
/// enough for both precompiles to reject it as malformed. The success flag is popped so the
/// caller survives, and `tail_pairs` plain pairs run afterwards.
fn call_then_work(target: Address, tail_pairs: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default()
        .mstore(0, [0xAAu8; 32])
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(32u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(target)
        .push_number(FORWARDED)
        .append(CALL)
        .append(POP);
    for _ in 0..tail_pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder.append(STOP).build()
}

/// Runs `code`, optionally deploying `callee` at [`CALLEE`].
fn run(
    spec: MegaSpecId,
    code: Bytes,
    callee: Option<Bytes>,
    limits: EvmTxRuntimeLimits,
) -> Outcome {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, alloy_primitives::U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, alloy_primitives::U256::from(ONE_ETH));
    if let Some(callee) = callee {
        db = db.account_code(CALLEE, callee);
    }
    transact(spec, db, limits)
}

fn default_limits(spec: MegaSpecId) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::from_spec(spec)
}

fn stop_code() -> Bytes {
    BytecodeBuilder::default().append(STOP).build()
}

fn invalid_code() -> Bytes {
    BytecodeBuilder::default().append(INVALID).build()
}

/// REX7 splits a precompile halt by actual work: KZG verification failure enforces the
/// fixed fee and destroys the rest of the forwarded envelope; a generic error (blake2f
/// malformed input) enforces nothing and destroys the whole envelope. An interpreter
/// frame that `INVALID`s in the same position is the control — same destroyed amount,
/// same unenforced remainder.
#[test]
fn test_precompile_halt_splits_executed_work_from_the_destroyed_envelope() {
    let spec = MegaSpecId::REX7;
    let limits = default_limits(spec);
    let base = run(spec, call_then_work(CALLEE, 0), Some(stop_code()), limits);
    let kzg = run(spec, call_then_work(KZG, 0), None, limits);
    let blake = run(spec, call_then_work(BLAKE2F, 0), None, limits);
    let interpreter = run(spec, call_then_work(CALLEE, 0), Some(invalid_code()), limits);

    for (label, r) in
        [("baseline", &base), ("kzg", &kzg), ("blake2f", &blake), ("interpreter", &interpreter)]
    {
        assert!(r.is_success(), "{label}: the caller must absorb the failure: {:?}", r.result);
    }

    let delta = |r: &Outcome| {
        (
            r.compute_gas as i64 - base.compute_gas as i64,
            r.enforced() as i64 - base.enforced() as i64,
            r.destroyed as i64 - base.destroyed as i64,
            r.gas_used as i64 - base.gas_used as i64,
        )
    };

    let (kzg_dc, kzg_de, kzg_dd, kzg_dg) = delta(&kzg);
    let (blake_dc, blake_de, blake_dd, blake_dg) = delta(&blake);
    let (interp_dc, interp_de, interp_dd, interp_dg) = delta(&interpreter);

    for (label, dg) in [("kzg", kzg_dg), ("blake2f", blake_dg), ("interpreter", interp_dg)] {
        assert!(
            dg >= FORWARDED as i64,
            "{label}: the forwarded envelope must actually be lost; Δgas_used={dg}",
        );
    }

    assert_eq!(
        interp_dd, FORWARDED as i64,
        "the control frame destroys exactly the forwarded envelope",
    );
    assert_eq!(interp_de, 0, "and none of the control frame's envelope is enforced");
    assert_eq!(
        interp_dc, FORWARDED as i64,
        "the control's reported total is the destroyed envelope on top of the caller",
    );

    assert_eq!(
        kzg_de,
        kzg_point_evaluation::GAS_COST as i64,
        "a KZG verification failure enforces the fixed fee, not the forwarded envelope",
    );
    assert_eq!(
        kzg_dd,
        (FORWARDED - kzg_point_evaluation::GAS_COST) as i64,
        "the rest of the forwarded envelope is destroyed, not enforced",
    );
    assert_eq!(kzg_dc, FORWARDED as i64, "the reported total covers the whole parent-frame loss",);

    assert_eq!(blake_de, 0, "a generic precompile error performed no work, so nothing enforces");
    assert_eq!(
        blake_dd, FORWARDED as i64,
        "the whole forwarded envelope is destroyed on the generic error arm",
    );
    assert_eq!(blake_dc, FORWARDED as i64, "the reported total still covers the parent-frame loss");
}

/// Through REX6 the same three shapes stay on the historical single-lane recording: KZG
/// charges the fixed fee as enforcing usage, the generic error charges the (capped) limit
/// as enforcing usage, and nothing is booked as destroyed.
#[test]
fn test_rex6_precompile_halt_accounting_is_unchanged() {
    let spec = MegaSpecId::REX6;
    let limits = default_limits(spec);
    let base = run(spec, call_then_work(CALLEE, 0), Some(stop_code()), limits);
    let kzg = run(spec, call_then_work(KZG, 0), None, limits);
    let blake = run(spec, call_then_work(BLAKE2F, 0), None, limits);
    let interpreter = run(spec, call_then_work(CALLEE, 0), Some(invalid_code()), limits);

    for (label, r) in
        [("baseline", &base), ("kzg", &kzg), ("blake2f", &blake), ("interpreter", &interpreter)]
    {
        assert!(r.is_success(), "{label}: the caller must absorb the failure: {:?}", r.result);
        assert_eq!(r.destroyed, 0, "{label}: REX6 has no destroyed lane");
    }

    let d = |r: &Outcome| r.compute_gas as i64 - base.compute_gas as i64;
    assert_eq!(
        d(&kzg),
        kzg_point_evaluation::GAS_COST as i64,
        "REX6 KZG still records only the fixed fee",
    );
    assert_eq!(d(&blake), FORWARDED as i64, "REX6 generic error still records the whole envelope");
    assert_eq!(
        d(&interpreter),
        0,
        "REX6 attributes neither the failing opcode nor the destroyed remainder",
    );
    assert_eq!(kzg.enforced(), kzg.compute_gas, "REX6 KZG charge is entirely enforcing");
    assert_eq!(blake.enforced(), blake.compute_gas, "REX6 generic charge is entirely enforcing");
}

/// The generic-arm destroyed remainder is not enforcing, so work after the failing CALL can
/// still run under the same compute limit that REX6 spends entirely on the envelope. REX6
/// keeps starving the tail — that single-lane charge is frozen.
#[test]
fn test_generic_precompile_halt_does_not_starve_the_tail() {
    const TAIL_PAIRS: usize = 2_000;

    let base = run(
        MegaSpecId::REX7,
        call_then_work(CALLEE, TAIL_PAIRS),
        Some(stop_code()),
        default_limits(MegaSpecId::REX7),
    );
    assert!(base.is_success(), "the baseline shape must fit: {:?}", base.result);

    let limit = base.compute_gas + 5_000;
    let limits7 = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(limit);
    let limits6 = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX6).with_tx_compute_gas_limit(limit);

    let blake7 = run(MegaSpecId::REX7, call_then_work(BLAKE2F, TAIL_PAIRS), None, limits7);
    let interp7 =
        run(MegaSpecId::REX7, call_then_work(CALLEE, TAIL_PAIRS), Some(invalid_code()), limits7);
    let blake6 = run(MegaSpecId::REX6, call_then_work(BLAKE2F, TAIL_PAIRS), None, limits6);
    let interp6 =
        run(MegaSpecId::REX6, call_then_work(CALLEE, TAIL_PAIRS), Some(invalid_code()), limits6);

    assert!(
        blake7.is_success(),
        "REX7 does not enforce the generic-arm envelope, so the tail must run: {:?}",
        blake7.result,
    );
    assert!(
        interp7.is_success(),
        "the interpreter control's destroyed remainder is not enforcing either: {:?}",
        interp7.result,
    );
    assert!(
        !blake6.is_success(),
        "REX6 still enforces the generic-arm envelope, so the same tail must starve: {:?}",
        blake6.result,
    );
    assert!(
        interp6.is_success(),
        "REX6 still attributes nothing to an interpreter halt, so that tail survives: {:?}",
        interp6.result,
    );
}

/// When the REX5 forwarded-gas cap binds (`effective < gas_limit`), the parent still burns
/// the caller-supplied envelope. The gap is parent-frame loss, not work, so it lands in
/// the destroyed remainder rather than disappearing from both lanes.
#[test]
fn test_destroyed_remainder_includes_the_forwarded_cap_gap() {
    let unconstrained = run(
        MegaSpecId::REX7,
        call_then_work(CALLEE, 0),
        Some(stop_code()),
        default_limits(MegaSpecId::REX7),
    );
    assert!(unconstrained.is_success(), "calibration run must succeed: {:?}", unconstrained.result);

    // Comfortable room for the caller and the CALL body, nowhere near the forwarded envelope.
    // The cap therefore binds: effective remaining is a few tens of thousands, forwarded is 1M.
    let limit = unconstrained.compute_gas + 50_000;
    let limits = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(limit);

    let blake = run(MegaSpecId::REX7, call_then_work(BLAKE2F, 0), None, limits);
    assert!(blake.is_success(), "the caller must absorb the generic error: {:?}", blake.result);
    assert_eq!(
        blake.enforced() - unconstrained.enforced(),
        0,
        "the generic arm still performed no work under the cap",
    );
    assert_eq!(
        blake.destroyed, FORWARDED,
        "destroyed is the caller-supplied envelope, including the cap gap; \
         recording the effective limit instead would report only the leftover headroom",
    );
}

/// Unconstrained default-limit reading, so a regression in the caller's own cost is visible
/// next to the split numbers rather than only inside a delta.
#[test]
fn test_kzg_halt_default_limits_report_the_full_parent_loss() {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, alloy_primitives::U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, call_then_work(KZG, 0))
        .account_balance(CONTRACT, alloy_primitives::U256::from(ONE_ETH));
    let r = transact_default(MegaSpecId::REX7, db);
    assert!(r.is_success(), "the caller must absorb the KZG failure: {:?}", r.result);
    assert_eq!(
        r.destroyed,
        FORWARDED - kzg_point_evaluation::GAS_COST,
        "default limits still leave the unused envelope in the destroyed lane",
    );
}
