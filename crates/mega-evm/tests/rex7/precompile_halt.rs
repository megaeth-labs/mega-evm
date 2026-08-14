//! A precompile that halts exceptionally is split the same way as an interpreter frame.
//!
//! A precompile runs inside `frame_init` and comes back as a result, so it never reaches the
//! interpreter-frame halt settlement. REX7 therefore splits it at the precompile recording site:
//!
//! - **Executed** — the work the precompile actually performed (the KZG fixed fee when the call
//!   reached verification; zero when the input was rejected before any work). This is enforcing.
//! - **Destroyed** — the rest of the forwarded envelope, which is the caller-supplied forwarded
//!   envelope, not the REX5-capped effective limit. This is reported and never enforced.
//!
//! KZG therefore lands on both sides of the split depending on where it failed: an input whose
//! length is not 192 bytes is turned away at the doorway, before the commitment is read, while
//! any failure past that point means verification was under way and is priced at the whole fixed
//! fee.
//!
//! Through REX6 the same recording site stays single-lane: success / revert still charge spent,
//! every KZG failure past the wrapper's gas gate charges the fixed fee — doorway rejects
//! included — and every other error still charges the (capped) limit as enforcing usage.

use crate::common::{transact, transact_default, Outcome, CALLEE, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{address, Address, Bytes};
use mega_evm::{
    kzg_point_evaluation,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{CALL, INVALID, POP, STOP};
use sha2::{Digest, Sha256};

/// KZG point evaluation.
const KZG: Address = address!("000000000000000000000000000000000000000a");
/// blake2f. Rejects any input whose length is not 213 bytes, before charging anything.
const BLAKE2F: Address = address!("0000000000000000000000000000000000000009");

/// Gas every probed CALL forwards. Far above both precompiles' real costs, so the destroyed
/// remainder dominates every other term and the REX5 forwarded-gas cap is a no-op unless a
/// test tightens the compute limit.
const FORWARDED: u64 = 1_000_000;

/// Calldata short enough that every precompile probed here rejects it on length alone — for KZG
/// that is the doorway reject, for blake2f the generic malformed-input error.
fn malformed_calldata() -> Vec<u8> {
    vec![0xAAu8; 32]
}

/// The EIP-4844 point-evaluation test vector with the last byte of the proof flipped.
///
/// Still 192 bytes with a matching versioned hash, so KZG clears the length doorway and the
/// commitment comparison and fails inside proof verification — the priced side of the split.
fn verification_failure_calldata() -> Vec<u8> {
    let commitment = hex::decode(
        "8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca2\
         5f26936857bc3a7c2539ea8ec3a952b7",
    )
    .unwrap();
    let mut versioned_hash = Sha256::digest(&commitment).to_vec();
    versioned_hash[0] = 0x01; // VERSIONED_HASH_VERSION_KZG
    let z =
        hex::decode("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000").unwrap();
    let y =
        hex::decode("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9").unwrap();
    let proof = hex::decode(
        "a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc216074\
         4faf0070725e00b60ad9a026a15b1a8c",
    )
    .unwrap();

    let mut input = Vec::new();
    input.extend_from_slice(&versioned_hash);
    input.extend_from_slice(&z);
    input.extend_from_slice(&y);
    input.extend_from_slice(&commitment);
    input.extend_from_slice(&proof);
    assert_eq!(input.len(), 192, "the priced probe must clear the 192-byte doorway");
    let last = input.len() - 1;
    input[last] ^= 0x01;
    input
}

/// A CALL forwarding [`FORWARDED`] gas to `target`, with `calldata` laid out at `mem[0..]`. The
/// success flag is popped so the caller survives, and `tail_pairs` plain pairs run afterwards.
fn call_then_work(target: Address, calldata: &[u8], tail_pairs: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default()
        .mstore(0, calldata)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(calldata.len() as u64) // argsSize
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

/// The `(reported, enforced, destroyed, gas_used)` deltas of a failing call over a baseline
/// that runs byte-identical caller code against a STOP callee.
///
/// Sharing the calldata between case and baseline is what makes the deltas exact: the caller's
/// own MSTORE / memory-expansion / CALL cost cancels, leaving only what the failed call
/// contributed.
fn deltas(
    spec: MegaSpecId,
    target: Address,
    callee: Option<Bytes>,
    calldata: &[u8],
    limits: EvmTxRuntimeLimits,
    label: &str,
) -> (i64, i64, i64, i64) {
    let base = run(spec, call_then_work(CALLEE, calldata, 0), Some(stop_code()), limits);
    let case = run(spec, call_then_work(target, calldata, 0), callee, limits);
    assert!(base.is_success(), "{label}: the baseline must succeed: {:?}", base.result);
    assert!(case.is_success(), "{label}: the caller must absorb the failure: {:?}", case.result);
    (
        case.compute_gas as i64 - base.compute_gas as i64,
        case.enforced() as i64 - base.enforced() as i64,
        case.destroyed as i64 - base.destroyed as i64,
        case.gas_used as i64 - base.gas_used as i64,
    )
}

/// REX7 splits a precompile halt by actual work.
///
/// Four shapes, each measured against a baseline running the identical caller code against a
/// STOP callee:
///
/// - KZG fed a 192-byte input that fails proof verification — the call got past the length doorway,
///   so the fixed fee is the work performed and enforces; the rest of the forwarded envelope is
///   destroyed.
/// - KZG fed a 32-byte input — turned away at the length doorway before the commitment is read, so
///   nothing was performed and the whole envelope is destroyed.
/// - blake2f fed the same 32-byte input — the generic error arm, which behaves the same way.
/// - an interpreter frame that `INVALID`s in the same position — the control, which destroys the
///   whole envelope too.
#[test]
fn test_precompile_halt_splits_executed_work_from_the_destroyed_envelope() {
    let spec = MegaSpecId::REX7;
    let limits = default_limits(spec);
    let malformed = malformed_calldata();

    let (kzg_dc, kzg_de, kzg_dd, kzg_dg) =
        deltas(spec, KZG, None, &verification_failure_calldata(), limits, "kzg verification");
    let (door_dc, door_de, door_dd, door_dg) =
        deltas(spec, KZG, None, &malformed, limits, "kzg doorway");
    let (blake_dc, blake_de, blake_dd, blake_dg) =
        deltas(spec, BLAKE2F, None, &malformed, limits, "blake2f");
    let (interp_dc, interp_de, interp_dd, interp_dg) =
        deltas(spec, CALLEE, Some(invalid_code()), &malformed, limits, "interpreter");

    for (label, dg) in [
        ("kzg verification", kzg_dg),
        ("kzg doorway", door_dg),
        ("blake2f", blake_dg),
        ("interpreter", interp_dg),
    ] {
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
        "a KZG failure raised inside verification enforces the fixed fee, not the forwarded \
         envelope",
    );
    assert_eq!(
        kzg_dd,
        (FORWARDED - kzg_point_evaluation::GAS_COST) as i64,
        "the rest of the forwarded envelope is destroyed, not enforced",
    );
    assert_eq!(kzg_dc, FORWARDED as i64, "the reported total covers the whole forwarded envelope",);

    assert_eq!(
        door_de, 0,
        "a KZG input rejected on length performed no work, so nothing enforces — the fixed fee \
         must not be charged for a call that never read the commitment",
    );
    assert_eq!(door_dd, FORWARDED as i64, "the whole envelope is destroyed on a doorway reject");
    assert_eq!(door_dc, FORWARDED as i64, "the reported total still covers the envelope");

    assert_eq!(blake_de, 0, "a generic precompile error performed no work, so nothing enforces");
    assert_eq!(
        blake_dd, FORWARDED as i64,
        "the whole forwarded envelope is destroyed on the generic error arm",
    );
    assert_eq!(
        blake_dc, FORWARDED as i64,
        "the reported total still covers the forwarded envelope"
    );
}

/// Through REX6 the same shapes stay on the historical single-lane recording: every KZG failure
/// past the wrapper's gas gate charges the fixed fee as enforcing usage — doorway rejects
/// included, which is where REX7 now differs — the generic error charges the (capped) limit as
/// enforcing usage, and nothing is booked as destroyed.
#[test]
fn test_rex6_precompile_halt_accounting_is_unchanged() {
    let spec = MegaSpecId::REX6;
    let limits = default_limits(spec);
    let malformed = malformed_calldata();

    let kzg_verification =
        deltas(spec, KZG, None, &verification_failure_calldata(), limits, "kzg verification");
    let kzg_doorway = deltas(spec, KZG, None, &malformed, limits, "kzg doorway");
    let blake = deltas(spec, BLAKE2F, None, &malformed, limits, "blake2f");
    let interpreter = deltas(spec, CALLEE, Some(invalid_code()), &malformed, limits, "interpreter");

    for (label, (dc, de, dd, _)) in [
        ("kzg verification", kzg_verification),
        ("kzg doorway", kzg_doorway),
        ("blake2f", blake),
        ("interpreter", interpreter),
    ] {
        assert_eq!(dd, 0, "{label}: REX6 has no destroyed lane");
        assert_eq!(de, dc, "{label}: REX6 charges are entirely enforcing");
    }

    assert_eq!(
        kzg_verification.0,
        kzg_point_evaluation::GAS_COST as i64,
        "REX6 KZG still records only the fixed fee",
    );
    assert_eq!(
        kzg_doorway.0,
        kzg_point_evaluation::GAS_COST as i64,
        "REX6 charges the fixed fee for a doorway reject too",
    );
    assert_eq!(blake.0, FORWARDED as i64, "REX6 generic error still records the whole envelope");
    assert_eq!(
        interpreter.0, 0,
        "REX6 attributes neither the failing opcode nor the destroyed remainder",
    );
}

/// The generic-arm destroyed remainder is not enforcing, so work after the failing CALL can
/// still run under the same compute limit that REX6 spends entirely on the envelope. REX6
/// keeps starving the tail — that single-lane charge is frozen.
#[test]
fn test_generic_precompile_halt_does_not_starve_the_tail() {
    const TAIL_PAIRS: usize = 2_000;

    let malformed = malformed_calldata();
    let base = run(
        MegaSpecId::REX7,
        call_then_work(CALLEE, &malformed, TAIL_PAIRS),
        Some(stop_code()),
        default_limits(MegaSpecId::REX7),
    );
    assert!(base.is_success(), "the baseline shape must fit: {:?}", base.result);

    let limit = base.compute_gas + 5_000;
    let limits7 = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(limit);
    let limits6 = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX6).with_tx_compute_gas_limit(limit);

    let blake7 =
        run(MegaSpecId::REX7, call_then_work(BLAKE2F, &malformed, TAIL_PAIRS), None, limits7);
    let interp7 = run(
        MegaSpecId::REX7,
        call_then_work(CALLEE, &malformed, TAIL_PAIRS),
        Some(invalid_code()),
        limits7,
    );
    let blake6 =
        run(MegaSpecId::REX6, call_then_work(BLAKE2F, &malformed, TAIL_PAIRS), None, limits6);
    let interp6 = run(
        MegaSpecId::REX6,
        call_then_work(CALLEE, &malformed, TAIL_PAIRS),
        Some(invalid_code()),
        limits6,
    );

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
/// the caller-supplied envelope. The gap is part of the forwarded envelope, not work, so it lands
/// in the destroyed remainder rather than disappearing from both lanes.
#[test]
fn test_destroyed_remainder_includes_the_forwarded_cap_gap() {
    let malformed = malformed_calldata();
    let unconstrained = run(
        MegaSpecId::REX7,
        call_then_work(CALLEE, &malformed, 0),
        Some(stop_code()),
        default_limits(MegaSpecId::REX7),
    );
    assert!(unconstrained.is_success(), "calibration run must succeed: {:?}", unconstrained.result);

    // Comfortable room for the caller and the CALL body, nowhere near the forwarded envelope.
    // The cap therefore binds: effective remaining is a few tens of thousands, forwarded is 1M.
    let limit = unconstrained.compute_gas + 50_000;
    let limits = EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(limit);

    let blake = run(MegaSpecId::REX7, call_then_work(BLAKE2F, &malformed, 0), None, limits);
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

/// Unconstrained default-limit readings, so a regression in the caller's own cost is visible
/// next to the split numbers rather than only inside a delta. Both KZG sides are read here:
/// a failure inside verification leaves the fixed fee out of the destroyed lane, a doorway
/// reject leaves nothing out of it.
#[test]
fn test_kzg_halt_default_limits_report_the_full_parent_loss() {
    let run_default = |calldata: &[u8]| {
        let db = MemoryDatabase::default()
            .account_balance(CALLER, alloy_primitives::U256::from(10 * ONE_ETH))
            .account_code(CONTRACT, call_then_work(KZG, calldata, 0))
            .account_balance(CONTRACT, alloy_primitives::U256::from(ONE_ETH));
        let r = transact_default(MegaSpecId::REX7, db);
        assert!(r.is_success(), "the caller must absorb the KZG failure: {:?}", r.result);
        r
    };

    assert_eq!(
        run_default(&verification_failure_calldata()).destroyed,
        FORWARDED - kzg_point_evaluation::GAS_COST,
        "default limits still leave the unused envelope in the destroyed lane",
    );
    assert_eq!(
        run_default(&malformed_calldata()).destroyed,
        FORWARDED,
        "a doorway reject leaves the whole envelope in the destroyed lane",
    );
}
