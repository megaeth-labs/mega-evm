//! REX7 V0 gas-clamp enforcement.
//!
//! Plain opcodes under checkpoint accounting record nothing, so nothing checks a limit while a
//! plain segment runs. Enforcement instead comes from the interpreter itself: at every checkpoint
//! and frame entry / resume the visible remaining gas is clamped down to the compute headroom — the
//! tighter of the frame-local budget and the TX-level (possibly detained) limit — and the hidden
//! remainder is remembered along with the constraint that bound it. revm's own per-opcode gas check
//! then stops a crossing opcode at the clamp boundary *before it executes*, and the frame's final
//! result restores the hidden gas and reclassifies the out-of-gas as the limit exceed it stands
//! for.
//!
//! These tests pin the three sides of that mechanism:
//!
//! - **Unobservable while within limits**: `GAS` reads the true counter even under an active clamp,
//!   so a transaction that never exceeds a limit is bit-identical to per-opcode accounting.
//! - **Exact enforcement**: the crossing opcode never runs, its cost never enters the recorded
//!   usage, and usage therefore stops at or below the limit — including inside a checkpoint-free
//!   arithmetic loop, where deferring to the next checkpoint would overshoot by the whole loop.
//! - **Faithful reclassification**: frame-local exceeds revert to the parent, TX-level exceeds halt
//!   the transaction with the remaining gas rescued, and a detention exceed keeps reporting
//!   `VolatileDataAccessOutOfGas`.

use crate::common::{
    transact, transact_default, transact_with_gas_limit, Outcome, CALLEE, CONTRACT, ONE_ETH,
};
use alloy_primitives::{Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaHaltReason, MegaSpecId,
};
use revm::bytecode::opcode::{
    CALL, DUP1, EXTCODECOPY, GAS, JUMPDEST, JUMPI, MSTORE, POP, RETURN, SSTORE, STOP, SUB, SWAP1,
    TIMESTAMP,
};

/// Slot the outer contract stores the CALL success flag into.
const CALL_RESULT_SLOT: u64 = 0x10;
/// Slot a callee writes to, so a reverted sub-frame can be told from a committed one.
const CALLEE_SLOT: u64 = 0x11;

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(crate::common::CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

/// A countdown loop of cheap opcodes with no checkpoint anywhere inside the loop body:
///
/// ```text
/// [prefix] PUSH2 iterations; loop: JUMPDEST; PUSH1 1; SWAP1; SUB; DUP1; PUSH1 loop; JUMPI; STOP
/// ```
///
/// Each iteration runs seven plain opcodes for 26 gas. `prefix` is prepended verbatim and
/// participates in the jump-target offset.
fn countdown_loop_code(prefix: &[u8], iterations: u16) -> Bytes {
    let mut code = prefix.to_vec();
    code.push(0x61); // PUSH2
    code.extend_from_slice(&iterations.to_be_bytes());
    let loop_target = u8::try_from(code.len()).expect("loop target must fit in a PUSH1");
    code.push(JUMPDEST);
    code.extend_from_slice(&[0x60, 0x01]); // PUSH1 1
    code.push(SWAP1);
    code.push(SUB);
    code.push(DUP1);
    code.extend_from_slice(&[0x60, loop_target]); // PUSH1 loop
    code.push(JUMPI);
    code.push(STOP);
    Bytes::from(code)
}

/// `pairs` PUSH1/POP pairs — plain opcodes that record nothing of their own.
fn plain_filler(builder: BytecodeBuilder, pairs: usize) -> BytecodeBuilder {
    let mut builder = builder;
    for _ in 0..pairs {
        builder = builder.push_number(1u64).append(POP);
    }
    builder
}

/// Per-spec runtime limits with the TX compute gas limit replaced.
fn compute_limit(limit: u64) -> impl Fn(MegaSpecId) -> EvmTxRuntimeLimits {
    move |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(limit)
}

/// Per-spec runtime limits with the block-environment detention cap replaced.
fn detention_cap(cap: u64) -> impl Fn(MegaSpecId) -> EvmTxRuntimeLimits {
    move |spec| {
        let mut limits = EvmTxRuntimeLimits::from_spec(spec);
        limits.block_env_access_compute_gas_limit = cap;
        limits
    }
}

/// The compute gas a transaction running `code` uses when nothing constrains it.
fn unconstrained_compute_gas(code: Bytes) -> u64 {
    transact_default(MegaSpecId::REX7, base_db(code)).compute_gas
}

/// `GAS` must observe the true remaining gas even while a clamp is outstanding — the checkpoint
/// prologue restores the hidden gas before the raw instruction reads the counter.
///
/// A tight detention cap keeps the clamp active for the whole post-access run while the transaction
/// itself stays far inside every limit, so the stored reading, the compute total and the receipt
/// must all match per-opcode REX6, where no clamp exists at all.
#[test]
fn test_clamp_is_unobservable_via_the_gas_opcode() {
    let code = plain_filler(BytecodeBuilder::default(), 5)
        .append(TIMESTAMP)
        .append(POP)
        .append(GAS)
        .push_u256(U256::from(CALL_RESULT_SLOT))
        .append(SSTORE)
        .append(STOP)
        .build();
    // A cap two orders of magnitude below the frame's remaining EVM gas, so the clamp is active at
    // the GAS opcode, but well above what the rest of this transaction spends, so nothing is ever
    // exceeded.
    let limits = detention_cap(1_000_000);

    let r6 = transact(MegaSpecId::REX6, base_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, base_db(code), limits(MegaSpecId::REX7));

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);

    let slot = U256::from(CALL_RESULT_SLOT);
    let r7_reading = r7.storage_value(CONTRACT, slot);
    assert!(!r7_reading.is_zero(), "the GAS reading must be non-zero");
    assert_eq!(
        r6.storage_value(CONTRACT, slot),
        r7_reading,
        "GAS must push the true remaining gas, not the clamped value",
    );
    assert_eq!(r6.compute_gas, r7.compute_gas, "compute totals must be identical");
    assert_eq!(r6.gas_used, r7.gas_used, "receipt gas must be identical");
}

/// The crossing opcode is stopped before it executes, so its cost never enters the recorded usage.
///
/// The limit is placed partway through a straight plain-opcode run. REX6 executes the crossing
/// opcode and only then records it, so its usage ends up strictly over the limit; REX7 clamps the
/// visible gas to the headroom, so revm rejects the crossing opcode at the boundary and usage stops
/// exactly at the limit.
#[test]
fn test_crossing_opcode_is_stopped_before_it_executes() {
    let code = plain_filler(BytecodeBuilder::default(), 200).append(STOP).build();
    let intrinsic = unconstrained_compute_gas(BytecodeBuilder::default().append(STOP).build());
    let full_run = unconstrained_compute_gas(code.clone());
    // Trip the limit halfway through the plain run.
    let limit = intrinsic + (full_run - intrinsic) / 2;
    let limits = compute_limit(limit);

    let r6 = transact(MegaSpecId::REX6, base_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, base_db(code), limits(MegaSpecId::REX7));

    assert!(!r6.is_success(), "REX6 must stop on the tight compute limit: {:?}", r6.result);
    assert!(!r7.is_success(), "REX7 must stop on the tight compute limit: {:?}", r7.result);

    assert!(
        r6.compute_gas > limit,
        "REX6 records the crossing opcode before halting; compute={} limit={limit}",
        r6.compute_gas
    );
    assert_eq!(
        r7.compute_gas, limit,
        "REX7 must stop exactly at the limit: the crossing opcode never runs, and the headroom it \
         could not pay for is what the frame burns",
    );
}

/// A TX-level crossing halts the transaction, and the gas the clamp was hiding is rescued for the
/// sender rather than burned.
///
/// The top-level frame's compute budget equals the TX-level remaining, so the TX limit is what
/// binds and the halt must propagate.
#[test]
fn test_tx_level_clamp_exceed_halts_with_the_hidden_gas_rescued() {
    // ~260k gas of plain opcodes with no checkpoint inside the loop at all.
    let code = countdown_loop_code(&[], 10_000);
    let intrinsic = unconstrained_compute_gas(BytecodeBuilder::default().append(STOP).build());
    let limit = intrinsic + 5_000;

    let r7 = transact(MegaSpecId::REX7, base_db(code), compute_limit(limit)(MegaSpecId::REX7));

    assert!(!r7.is_success(), "the tight compute limit must halt the transaction: {:?}", r7.result);
    assert!(
        matches!(r7.halt_reason("REX7"), MegaHaltReason::ComputeGasLimitExceeded { .. }),
        "a TX-level clamp exceed must report the compute-gas limit; got {:?}",
        r7.halt_reason("REX7"),
    );
    assert_eq!(r7.compute_gas, limit, "usage must stop at the limit, not past it");
    assert!(
        r7.gas_used < 200_000,
        "the clamp-hidden gas must be rescued, not burned; gas_used={}",
        r7.gas_used
    );
}

/// A frame-local crossing reverts the sub-frame and lets the caller continue.
///
/// A nested frame's compute budget is 98/100 of its parent's remaining budget, so it is always
/// tighter than the TX-level remaining — the clamp binds frame-locally, and the clamp-induced
/// out-of-gas must be reclassified into the ordinary frame-local revert rather than a TX halt.
#[test]
fn test_frame_local_clamp_exceed_reverts_to_the_parent() {
    // The callee writes a slot and then burns far more compute than its frame budget allows. The
    // write is passed as the loop's prefix so the loop's jump target accounts for it.
    let prologue =
        BytecodeBuilder::default().sstore(U256::from(CALLEE_SLOT), U256::from(0x77)).build_vec();
    let callee = countdown_loop_code(&prologue, 10_000);

    // The caller returns the CALL's success flag. A nested frame may consume up to 98/100 of its
    // parent's compute budget, so the caller's own tail has to be cheap enough to fit in the
    // remainder — a storage write would push the caller over its budget too.
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(50_000_000u64) // gas
        .append(CALL)
        .push_number(0u64) // memory offset
        .append(MSTORE)
        .push_number(32u64) // length
        .push_number(0u64) // offset
        .append(RETURN)
        .build();

    let intrinsic = unconstrained_compute_gas(BytecodeBuilder::default().append(STOP).build());
    // Enough headroom for the caller's own work and the callee's SSTORE, far short of its loop.
    let limits = compute_limit(intrinsic + 100_000);
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let r6 = transact(MegaSpecId::REX6, build_db(), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, build_db(), limits(MegaSpecId::REX7));

    for (label, r) in [("REX6", &r6), ("REX7", &r7)] {
        assert!(
            r.is_success(),
            "{label}: the outer transaction survives a frame-local exceed: {:?}",
            r.result
        );
        assert_eq!(
            r.result.output().map(|o| U256::from_be_slice(o)),
            Some(U256::ZERO),
            "{label}: the CALL must report failure",
        );
        assert!(
            r.storage_value(CALLEE, U256::from(CALLEE_SLOT)).is_zero(),
            "{label}: the reverted sub-frame's storage write must be discarded",
        );
    }
    assert!(
        r7.compute_gas < r6.compute_gas,
        "REX7 stops the callee before the crossing opcode, so it records less than REX6; \
         REX6={} REX7={}",
        r6.compute_gas,
        r7.compute_gas
    );
}

/// A detention crossing keeps its `VolatileDataAccessOutOfGas` attribution.
///
/// Detention lowers the TX-level limit to `usage_at_access + cap`, so it is the TX-level constraint
/// that binds. The usual detained-exceed predicate needs usage to have crossed the detained limit,
/// which clamp enforcement never lets happen — the attribution has to survive on the clamp's own
/// record of what bound it.
#[test]
fn test_detention_clamp_exceed_keeps_the_volatile_attribution() {
    let code = countdown_loop_code(&[TIMESTAMP, POP], 10_000);
    let limits = detention_cap(1_000);

    let r6 = transact(MegaSpecId::REX6, base_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, base_db(code), limits(MegaSpecId::REX7));

    assert!(!r6.is_success(), "REX6 must halt on the detention cap: {:?}", r6.result);
    assert!(!r7.is_success(), "REX7 must halt on the detention cap: {:?}", r7.result);
    for (label, r) in [("REX6", &r6), ("REX7", &r7)] {
        assert!(
            matches!(r.halt_reason(label), MegaHaltReason::VolatileDataAccessOutOfGas { .. }),
            "{label}: the halt must be attributed to volatile detention; got {:?}",
            r.halt_reason(label),
        );
    }
}

/// The clamp bounds a detention cap inside a checkpoint-free arithmetic loop — the shape that makes
/// checkpoint-deferred enforcement unbounded, since the loop body contains no checkpoint at all and
/// the whole ~260k-gas loop would otherwise run to completion before anything checked.
#[test]
fn test_clamp_bounds_detention_inside_a_checkpoint_free_loop() {
    let cap = 1_000;
    let code = countdown_loop_code(&[TIMESTAMP, POP], 10_000);
    let intrinsic = unconstrained_compute_gas(BytecodeBuilder::default().append(STOP).build());
    let limits = detention_cap(cap);

    let r6 = transact(MegaSpecId::REX6, base_db(code.clone()), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, base_db(code.clone()), limits(MegaSpecId::REX7));
    let unconstrained = unconstrained_compute_gas(code);

    // The detained limit is `usage_at_access + cap`; the access happens two opcodes in, so
    // `intrinsic + TIMESTAMP + cap` bounds it from above.
    let detained_upper = intrinsic + 2 + cap;
    assert!(
        unconstrained > 250_000,
        "the loop must be far larger than the cap to make the test meaningful; loop={unconstrained}"
    );
    assert!(
        r7.compute_gas <= detained_upper,
        "REX7 must not overshoot the detention cap; compute={} cap≈{detained_upper}",
        r7.compute_gas
    );
    // Per-opcode enforcement stops within one opcode of the cap; the clamp must not stop earlier.
    assert!(
        r7.compute_gas + 32 >= r6.compute_gas,
        "REX7 must stop at the clamp boundary, not before it; REX6={} REX7={}",
        r6.compute_gas,
        r7.compute_gas
    );
}

/// The adjudicated double-exceed corner: when the crossing opcode outruns both the true EVM
/// remaining and the compute headroom, the compute classification wins.
///
/// A memory expansion far larger than the transaction's whole gas limit is unaffordable either way.
/// REX6 reports revm's memory out-of-gas and burns the frame; REX7 reports the compute-gas limit
/// and rescues the clamp-hidden remainder for the sender. The two are indistinguishable at the
/// frame boundary — an out-of-gas carries no opcode cost — and this direction favours the sender
/// without opening anything new: a caller that wants to avoid the burn can already REVERT.
#[test]
fn test_double_exceed_prefers_the_compute_classification() {
    // A ~7.5 MB memory offset: the expansion costs on the order of 10^8 gas, well past the
    // transaction's gas limit below.
    let code = plain_filler(BytecodeBuilder::default(), 5)
        .push_number(0u64) // value
        .push_number(7_500_000u64) // offset
        .append(MSTORE)
        .append(STOP)
        .build();
    let gas_limit = 1_000_000;
    let intrinsic = unconstrained_compute_gas(BytecodeBuilder::default().append(STOP).build());
    // Headroom well below the frame's true remaining, so the clamp is outstanding at the MSTORE.
    let limits = compute_limit(intrinsic + 1_000);

    let r6 = transact_with_gas_limit(
        MegaSpecId::REX6,
        base_db(code.clone()),
        limits(MegaSpecId::REX6),
        gas_limit,
    );
    let r7 = transact_with_gas_limit(
        MegaSpecId::REX7,
        base_db(code),
        limits(MegaSpecId::REX7),
        gas_limit,
    );

    assert!(!r6.is_success(), "REX6 must fail on the unaffordable expansion: {:?}", r6.result);
    assert!(!r7.is_success(), "REX7 must fail on the unaffordable expansion: {:?}", r7.result);
    assert!(
        matches!(r6.halt_reason("REX6"), MegaHaltReason::Base(_)),
        "REX6 reports revm's own out-of-gas; got {:?}",
        r6.halt_reason("REX6"),
    );
    assert!(
        matches!(r7.halt_reason("REX7"), MegaHaltReason::ComputeGasLimitExceeded { .. }),
        "REX7 must classify the double exceed as a compute exceed; got {:?}",
        r7.halt_reason("REX7"),
    );
    assert_eq!(r6.gas_used, gas_limit, "REX6 burns the whole gas limit");
    assert!(
        r7.gas_used < gas_limit,
        "REX7 must rescue the clamp-hidden gas; gas_used={} limit={gas_limit}",
        r7.gas_used
    );
}

/// Every checkpoint kind has to restore the clamp before its body runs and re-apply it afterwards,
/// or the segments around it would either observe clamped gas or run unbounded. Exercising them in
/// one transaction that stays inside every limit pins the round trip: any asymmetry between the
/// restore and the re-clamp shows up as a compute-gas or receipt difference against REX6.
#[test]
fn test_clamp_round_trips_through_every_checkpoint_kind() {
    let callee = plain_filler(BytecodeBuilder::default(), 5).append(STOP).build();
    let code = plain_filler(BytecodeBuilder::default(), 5)
        .append(TIMESTAMP)
        .append(POP)
        .append(GAS)
        .append(POP)
        .sstore(U256::from(1), U256::from(0x22))
        .push_u256(U256::from(1))
        .append(revm::bytecode::opcode::SLOAD)
        .append(POP)
        .push_address(CALLEE)
        .append(revm::bytecode::opcode::BALANCE)
        .append(POP);
    let code = plain_filler(code, 5)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(500_000u64) // gas
        .append(CALL)
        .append(POP);
    let code = plain_filler(code, 5)
        .mstore(0, [0x33u8; 32])
        .push_number(0xabcu64) // topic0
        .push_number(32u64) // len
        .push_number(0u64) // offset
        .append(revm::bytecode::opcode::LOG1)
        .append(STOP)
        .build();

    // A detention cap that engages at the TIMESTAMP but is never binding, so the clamp is
    // outstanding across every later checkpoint.
    let limits = detention_cap(1_000_000);
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());

    let r6 = transact(MegaSpecId::REX6, build_db(), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, build_db(), limits(MegaSpecId::REX7));

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);
    assert_outcomes_identical(&r6, &r7);
}

fn assert_outcomes_identical(r6: &Outcome, r7: &Outcome) {
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "execution result must be identical",
    );
    assert_eq!(r6.compute_gas, r7.compute_gas, "compute gas must be identical");
    assert_eq!(r6.gas_used, r7.gas_used, "receipt gas_used must be identical");
    assert_eq!(
        (r6.data_size, r6.kv_updates, r6.state_growth),
        (r7.data_size, r7.kv_updates, r7.state_growth),
        "the non-compute dimensions must be identical",
    );
}

/// A volatile checkpoint whose own body crosses the compute limit must behave identically under
/// both accounting models.
///
/// The prologue restores the clamp before the body runs, so an `EXTCODECOPY` large enough to cross
/// the limit is metered on the true counter and recorded per opcode exactly as REX6 records it —
/// the clamp plays no part. What the checkpoint form has to reproduce is the tail: the detention
/// cap is applied on a frame-local exceed (a revert the per-opcode layering carries past the cap)
/// and skipped on a TX-level exceed (an out-of-gas halt that layering short-circuits on). Pinning
/// the halt, the recorded usage and the resulting detained limit together covers both the metering
/// and that ordering.
#[test]
fn test_volatile_body_crossing_the_limit_matches_per_opcode() {
    // ~1.5 MB of EXTCODECOPY against the block beneficiary: the copy plus the memory expansion cost
    // millions of gas, and the account load marks beneficiary access.
    let callee = BytecodeBuilder::default()
        .push_number(1_500_000u64) // length
        .push_number(0u64) // offset
        .push_number(0u64) // destOffset
        .push_address(Address::ZERO) // the default block beneficiary
        .append(EXTCODECOPY)
        .append(STOP)
        .build();
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(50_000_000u64) // gas
        .append(CALL)
        .append(POP)
        .append(STOP)
        .build();
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());
    let full = transact_default(MegaSpecId::REX7, build_db()).compute_gas;
    assert!(full > 4_000_000, "the copy must dominate the transaction; compute={full}");

    // Just under what the transaction needs, so the crossing lands inside the EXTCODECOPY body
    // rather than in a plain segment.
    let tx_limit = full - full / 100;
    let limits = move |spec| {
        let mut limits = EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(tx_limit);
        limits.block_env_access_compute_gas_limit = 1_000;
        limits
    };

    let r6 = transact(MegaSpecId::REX6, build_db(), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, build_db(), limits(MegaSpecId::REX7));

    assert!(!r6.is_success(), "REX6 must stop on the tight compute limit: {:?}", r6.result);
    assert_eq!(
        format!("{:?}", r6.result),
        format!("{:?}", r7.result),
        "the halt must be identical",
    );
    assert_eq!(
        r6.compute_gas, r7.compute_gas,
        "the body is metered on the true counter under both models; REX6={} REX7={}",
        r6.compute_gas, r7.compute_gas
    );
    assert_eq!(
        r6.detained_compute_gas_limit, r7.detained_compute_gas_limit,
        "the detention tail must fire — or not fire — at the same point under both models",
    );
}
