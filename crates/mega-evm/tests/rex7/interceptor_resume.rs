//! REX7 checkpoint settlement across a CALL that never runs a child frame.
//!
//! Two call targets return to their caller without an EVM frame ever being created for them:
//!
//! - a **system contract interceptor**, which short-circuits inside `frame_init` and hands back a
//!   synthetic `FrameResult` carrying the full forwarded gas;
//! - a **precompile**, which revm executes inside `frame_init` and returns as a result rather than
//!   as a frame to run.
//!
//! Both take the CALL checkpoint on the way out and the frame-resume clamp on the way back, but
//! neither runs `AdditionalLimit::before_frame_init` against a real child. That makes them the two
//! places where the caller's segment settlement and the clamp round trip have to work without any
//! child-frame bookkeeping to lean on.
//!
//! What these tests pin:
//!
//! - the caller's open segment is settled **before** the interceptor reads the tracker, so a system
//!   contract that reports remaining compute gas reports the same number it reports under
//!   per-opcode accounting;
//! - the clamp is restored across the boundary, so the forwarded gas returns intact and the receipt
//!   does not depend on how much gas was forwarded;
//! - the caller's window re-opens on resume, so a limit crossing in the segment *after* the return
//!   is still stopped at the clamp boundary rather than overshooting to the next checkpoint.

use crate::common::{
    assert_outcomes_identical, base_db as common_base_db, compute_limit, detention_cap,
    plain_filler, transact, transact_default, Outcome, CALLEE, CONTRACT,
};
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall as _;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaLimitControl, MegaHaltReason, MegaSpecId, LIMIT_CONTROL_ADDRESS,
    LIMIT_CONTROL_CODE,
};
use revm::bytecode::opcode::{CALL, MLOAD, POP, SSTORE, STOP, TIMESTAMP};

/// The identity precompile: returns its input unchanged, and is executed inside `frame_init`.
const IDENTITY_PRECOMPILE: Address = address!("0000000000000000000000000000000000000004");

/// Slot the contract stores the value it observed through the CALL into.
const OBSERVED_SLOT: u64 = 0x20;
/// Slot a downstream checkpoint writes, so a halt before it is observable in state.
const DOWNSTREAM_SLOT: u64 = 0x21;

/// Memory offset the CALL's return data lands at, clear of the calldata at `0x00`.
const RET_OFFSET: u64 = 0x40;

/// Gas forwarded to the call target unless a test varies it.
const FORWARDED_GAS: u64 = 1_000_000;

fn base_db(code: Bytes) -> MemoryDatabase {
    common_base_db(code).account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
}

/// A CALL to `target` forwarding `forwarded_gas`, with `args_size` bytes of calldata taken from
/// `mem[0..]` and 32 bytes of return data written to `mem[RET_OFFSET..]`.
fn call_with_return_data(
    builder: BytecodeBuilder,
    target: Address,
    args_size: u64,
    forwarded_gas: u64,
) -> BytecodeBuilder {
    builder
        .push_number(32u64) // retSize
        .push_number(RET_OFFSET) // retOffset
        .push_number(args_size) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(target)
        .push_number(forwarded_gas)
        .append(CALL)
        .append(POP)
}

/// Everything up to and including the CALL into `MegaLimitControl.remainingComputeGas()`:
/// a plain run, the calldata write, and the CALL itself.
fn interceptor_prefix(prologue_volatile: bool, forwarded_gas: u64) -> BytecodeBuilder {
    let mut builder = plain_filler(BytecodeBuilder::default(), 5);
    if prologue_volatile {
        builder = builder.append(TIMESTAMP).append(POP);
    }
    let builder =
        plain_filler(builder, 5).mstore(0, IMegaLimitControl::remainingComputeGasCall::SELECTOR);
    call_with_return_data(builder, LIMIT_CONTROL_ADDRESS, 4, forwarded_gas)
}

/// Everything up to and including the CALL into the identity precompile.
fn precompile_prefix(prologue_volatile: bool, forwarded_gas: u64) -> BytecodeBuilder {
    let mut builder = plain_filler(BytecodeBuilder::default(), 5);
    if prologue_volatile {
        builder = builder.append(TIMESTAMP).append(POP);
    }
    let builder = plain_filler(builder, 5).mstore(0, [0x5au8; 32]);
    call_with_return_data(builder, IDENTITY_PRECOMPILE, 32, forwarded_gas)
}

/// Stores the 32 bytes the CALL returned into [`OBSERVED_SLOT`].
fn store_returned_word(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .push_number(RET_OFFSET)
        .append(MLOAD)
        .push_u256(U256::from(OBSERVED_SLOT))
        .append(SSTORE)
}

/// The compute gas a transaction running `code` uses when nothing constrains it.
fn unconstrained_compute_gas(code: Bytes) -> u64 {
    transact_default(MegaSpecId::REX7, base_db(code)).compute_gas
}

/// Runs `code` under both specs with `limits` and returns `(REX6, REX7)`.
fn run_both(
    code: &Bytes,
    limits: &impl Fn(MegaSpecId) -> EvmTxRuntimeLimits,
) -> (Outcome, Outcome) {
    let r6 = transact(MegaSpecId::REX6, base_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, base_db(code.clone()), limits(MegaSpecId::REX7));
    (r6, r7)
}

/// The interceptor reports the caller's remaining compute gas straight out of the tracker, so the
/// number it returns is a direct readout of how much of the caller's execution had been settled at
/// the moment `frame_init` ran.
///
/// Under per-opcode accounting every opcode ahead of the CALL is already recorded. Under checkpoint
/// accounting the whole plain segment ahead of it is still open until the CALL's checkpoint
/// prologue settles it — which runs at the CALL opcode, before `frame_init`. If that settlement
/// were deferred (to the resume, or to the frame's tail), the contract would observe more remaining
/// compute gas than REX6 reports. Comparing the stored word is what pins the ordering.
#[test]
fn test_interceptor_observes_the_settled_remaining_compute_gas() {
    let code = plain_filler(
        store_returned_word(plain_filler(interceptor_prefix(false, FORWARDED_GAS), 10)),
        10,
    )
    .append(STOP)
    .build();
    let (r6, r7) = run_both(&code, &EvmTxRuntimeLimits::from_spec);

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);

    let slot = U256::from(OBSERVED_SLOT);
    let observed = r7.storage_value(CONTRACT, slot);
    assert!(!observed.is_zero(), "the interceptor must have returned a remaining-gas reading");
    assert_eq!(
        r6.storage_value(CONTRACT, slot),
        observed,
        "the interceptor must observe the caller's segment already settled",
    );
    assert_outcomes_identical("limit-control interception", &r6, &r7);
}

/// The same readout while a clamp is outstanding.
///
/// A detention cap engaged before the CALL leaves the interpreter running on clamped gas through
/// the plain segment ahead of it. The CALL checkpoint has to restore the hidden gas before
/// `frame_init` runs, or the interceptor and the forwarding math would both be computed against a
/// counter missing the hidden remainder.
#[test]
fn test_interceptor_observes_the_same_reading_under_an_active_clamp() {
    let code = store_returned_word(plain_filler(interceptor_prefix(true, FORWARDED_GAS), 10))
        .append(STOP)
        .build();
    // Well above what this transaction spends, so detention is engaged but never binding.
    let (r6, r7) = run_both(&code, &detention_cap(1_000_000));

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);

    let slot = U256::from(OBSERVED_SLOT);
    let observed = r7.storage_value(CONTRACT, slot);
    assert!(!observed.is_zero(), "the interceptor must have returned a remaining-gas reading");
    assert_eq!(
        r6.storage_value(CONTRACT, slot),
        observed,
        "an outstanding clamp must not change what the interceptor observes",
    );
    assert_outcomes_identical("limit-control interception under a clamp", &r6, &r7);
}

/// Gas leakage path 1 — the system contract interception short-circuit.
///
/// The synthetic result carries `Gas::new(call_inputs.gas_limit)`: nothing is spent, so every gas
/// unit forwarded comes back. The receipt is therefore independent of the forwarded amount, and
/// that independence is what catches a clamp leak — if the clamp were still outstanding across
/// `frame_init`, or if the resume restored the forwarded amount rather than the hidden one, the two
/// runs below would not cost the same.
#[test]
fn test_interception_returns_the_forwarded_gas_regardless_of_the_amount() {
    let build = |forwarded| {
        store_returned_word(plain_filler(interceptor_prefix(true, forwarded), 10))
            .append(STOP)
            .build()
    };
    // Two PUSH3 operands, so the programs are byte-for-byte the same length and the only difference
    // is how much gas the interceptor is handed.
    let small = build(0x10_0000u64);
    let large = build(0x50_0000u64);
    assert_eq!(small.len(), large.len(), "the two programs must differ only in the operand");
    // A detention cap nowhere near binding, so a clamp is outstanding at the CALL in both REX7
    // arms.
    let limits = detention_cap(2_000_000);

    let (small6, small7) = run_both(&small, &limits);
    let (large6, large7) = run_both(&large, &limits);

    for (label, r) in [
        ("small REX6", &small6),
        ("small REX7", &small7),
        ("large REX6", &large6),
        ("large REX7", &large7),
    ] {
        assert!(r.is_success(), "{label}: must succeed: {:?}", r.result);
    }
    assert_eq!(
        small7.gas_used, large7.gas_used,
        "REX7: the interception must return the forwarded gas intact; 1M forwarded={} 5M \
         forwarded={}",
        small7.gas_used, large7.gas_used
    );
    assert_eq!(
        small6.gas_used, large6.gas_used,
        "REX6: same invariant, as the baseline the REX7 arm has to reproduce",
    );
    assert_outcomes_identical("1M forwarded to the interceptor", &small6, &small7);
    assert_outcomes_identical("5M forwarded to the interceptor", &large6, &large7);
}

/// Enforcement after an interceptor resume: the caller's settlement window re-opens at the resume,
/// so a crossing in the segment that follows is stopped at the clamp boundary.
///
/// The limit is placed inside the tail plain run, after the CALL has already returned. REX6
/// executes the crossing opcode and records it, so its usage ends up over the limit; REX7 stops
/// exactly at the limit and the downstream SSTORE never runs.
#[test]
fn test_crossing_after_an_interceptor_resume_stops_at_the_clamp_boundary() {
    let tail_pairs = 200;
    let code = plain_filler(interceptor_prefix(false, FORWARDED_GAS), tail_pairs)
        .sstore(U256::from(DOWNSTREAM_SLOT), U256::from(1))
        .append(STOP)
        .build();
    // The same program truncated at the resume, and at the end of the tail: the crossing goes
    // halfway between them, which is inside the tail and clear of both checkpoints.
    let at_resume =
        unconstrained_compute_gas(interceptor_prefix(false, FORWARDED_GAS).append(STOP).build());
    let after_tail = unconstrained_compute_gas(
        plain_filler(interceptor_prefix(false, FORWARDED_GAS), tail_pairs).append(STOP).build(),
    );
    assert!(after_tail > at_resume, "the tail must cost something; {at_resume} -> {after_tail}");
    let limit = at_resume + (after_tail - at_resume) / 2;

    let (r6, r7) = run_both(&code, &compute_limit(limit));

    assert!(!r6.is_success(), "REX6 must halt on the tight compute limit: {:?}", r6.result);
    assert!(!r7.is_success(), "REX7 must halt on the tight compute limit: {:?}", r7.result);
    assert!(
        r6.compute_gas > limit,
        "REX6 records the crossing opcode before halting; compute={} limit={limit}",
        r6.compute_gas
    );
    assert_eq!(
        r7.compute_gas, limit,
        "REX7 must stop exactly at the limit in the segment opened by the resume",
    );
    // The top-level frame's compute budget equals the TX-level remaining, and the clamp breaks that
    // tie towards the TX-level constraint, so a REX7 crossing reports the compute-gas limit.
    // (REX6's per-opcode check tries the frame budget first and reports the tie as a
    // frame-local exceed, which the top-level frame absorbs into a revert — the models classify
    // the tie differently.)
    assert!(
        matches!(r7.halt_reason("REX7"), MegaHaltReason::ComputeGasLimitExceeded { .. }),
        "REX7: the halt must report the compute-gas limit; got {:?}",
        r7.halt_reason("REX7"),
    );
    for (label, r) in [("REX6", &r6), ("REX7", &r7)] {
        assert!(
            r.storage_value(CONTRACT, U256::from(DOWNSTREAM_SLOT)).is_zero(),
            "{label}: the stop lands inside the tail, so the SSTORE after it never runs",
        );
    }
}

/// A precompile is executed inside `frame_init` and returns as a result, so like an interceptor it
/// resumes the caller without a child frame ever running. Unlike an interceptor it does spend gas,
/// so the resume merges a partially consumed budget back into the caller.
#[test]
fn test_precompile_resume_matches_per_opcode_accounting() {
    let code = plain_filler(
        store_returned_word(plain_filler(precompile_prefix(false, FORWARDED_GAS), 10)),
        10,
    )
    .append(STOP)
    .build();
    let (r6, r7) = run_both(&code, &EvmTxRuntimeLimits::from_spec);

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);
    assert_eq!(
        r7.storage_value(CONTRACT, U256::from(OBSERVED_SLOT)),
        U256::from_be_bytes([0x5au8; 32]),
        "the identity precompile must have returned its input",
    );
    assert_outcomes_identical("identity precompile", &r6, &r7);
}

/// The precompile resume with a clamp outstanding across the CALL.
#[test]
fn test_precompile_resume_under_an_active_clamp() {
    let code = store_returned_word(plain_filler(precompile_prefix(true, FORWARDED_GAS), 10))
        .append(STOP)
        .build();
    let (r6, r7) = run_both(&code, &detention_cap(1_000_000));

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);
    assert_outcomes_identical("identity precompile under a clamp", &r6, &r7);
}

/// Enforcement after a precompile resume, the counterpart of the interceptor case: the precompile
/// consumed part of the forwarded gas, so the resume re-clamps against a counter the child moved.
#[test]
fn test_crossing_after_a_precompile_resume_stops_at_the_clamp_boundary() {
    let tail_pairs = 200;
    let code = plain_filler(precompile_prefix(false, FORWARDED_GAS), tail_pairs)
        .sstore(U256::from(DOWNSTREAM_SLOT), U256::from(1))
        .append(STOP)
        .build();
    let at_resume =
        unconstrained_compute_gas(precompile_prefix(false, FORWARDED_GAS).append(STOP).build());
    let after_tail = unconstrained_compute_gas(
        plain_filler(precompile_prefix(false, FORWARDED_GAS), tail_pairs).append(STOP).build(),
    );
    let limit = at_resume + (after_tail - at_resume) / 2;

    let (r6, r7) = run_both(&code, &compute_limit(limit));

    assert!(!r6.is_success(), "REX6 must halt on the tight compute limit: {:?}", r6.result);
    assert!(!r7.is_success(), "REX7 must halt on the tight compute limit: {:?}", r7.result);
    assert!(
        r6.compute_gas > limit,
        "REX6 records the crossing opcode before halting; compute={} limit={limit}",
        r6.compute_gas
    );
    assert_eq!(
        r7.compute_gas, limit,
        "REX7 must stop exactly at the limit in the segment opened by the resume",
    );
    assert!(
        r7.storage_value(CONTRACT, U256::from(DOWNSTREAM_SLOT)).is_zero(),
        "the halt lands inside the tail, so the SSTORE after it never runs",
    );
}

/// The interception resume one frame down: the caller is a sub-frame, so the settlement and the
/// re-clamp happen against a frame-local compute budget rather than the TX-level remaining.
#[test]
fn test_nested_frame_interceptor_resume_matches_per_opcode_accounting() {
    let callee =
        plain_filler(store_returned_word(plain_filler(interceptor_prefix(false, 200_000), 10)), 10)
            .append(STOP)
            .build();
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(2_000_000u64) // gas
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let r6 = transact_default(MegaSpecId::REX6, build_db());
    let r7 = transact_default(MegaSpecId::REX7, build_db());

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);
    let slot = U256::from(OBSERVED_SLOT);
    assert!(
        !r7.storage_value(CALLEE, slot).is_zero(),
        "the nested interception must have returned a remaining-gas reading",
    );
    assert_eq!(
        r6.storage_value(CALLEE, slot),
        r7.storage_value(CALLEE, slot),
        "a nested caller must observe the same settled remaining compute gas",
    );
    assert_outcomes_identical("nested interception", &r6, &r7);
}
