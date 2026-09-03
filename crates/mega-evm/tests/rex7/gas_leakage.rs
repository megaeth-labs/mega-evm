//! REX7: the three gas-leakage paths, exercised with a clamp outstanding.
//!
//! Any mechanism that hides, grants or adjusts gas per frame has to be unwound on every way out of
//! a frame, or system-held gas leaks back to the parent or the sender. The gas clamp is such a
//! mechanism — it hides part of the interpreter's gas — and the three paths that have to handle it
//! are the ones the leakage checklist names:
//!
//! 1. **System contract interception** short-circuits `frame_init` and synthesizes a result with no
//!    child frame. The clamp must already be restored when the CALL checkpoint publishes the frame,
//!    and the caller's counter must come back whole on resume.
//! 2. **Gas rescue on a TX-level exceed** captures the frame's remaining gas for the sender. It
//!    must capture the true remaining — neither the clamped view (which would burn the hidden gas)
//!    nor the sum of both (which would refund it twice).
//! 3. **Frame return** hands the frame's gas back to its parent. The restore must happen before
//!    anything reads or charges that gas, and identically on success, on revert and on a limit
//!    exceed.
//!
//! The probes are chosen so a leak changes an observable, not just an internal: the parent's own
//! `GAS` reading after the child returns, the receipt's independence from the transaction gas
//! limit, and whether a code deposit that costs more than the clamp left visible can be paid at
//! all.

use crate::common::{
    assert_outcomes_identical, base_db as common_base_db, compute_limit, countdown_loop_code,
    detention_cap, plain_filler, transact, transact_with_gas_limit, Outcome, CALLEE, CONTRACT,
};
use alloy_primitives::{Bytes, U256};
use alloy_sol_types::SolCall as _;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaLimitControl, MegaHaltReason, MegaSpecId, LIMIT_CONTROL_ADDRESS,
    LIMIT_CONTROL_CODE,
};
use revm::bytecode::opcode::{CALL, CREATE, GAS, MSTORE, POP, RETURN, SSTORE, STOP, TIMESTAMP};

/// Slot the caller stores its post-return `GAS` reading into.
const GAS_READING_SLOT: u64 = 0x40;
/// Slot a callee writes, so a committed sub-frame can be told from a reverted one.
const CALLEE_SLOT: u64 = 0x41;

fn base_db(code: Bytes) -> MemoryDatabase {
    common_base_db(code).account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
}

/// A CALL to `target` forwarding `gas`, with no arguments and no return data, followed by the
/// caller reading `GAS` and storing it.
///
/// The stored reading is the probe: it is the caller's own view of its counter after the child's
/// gas has been merged back, so any gas the child failed to hand back — or handed back twice —
/// shows up in it.
fn call_then_store_gas(target: revm::primitives::Address, forwarded: u64) -> Bytes {
    plain_filler(BytecodeBuilder::default(), 5)
        .append(TIMESTAMP)
        .append(POP)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(target)
        .push_number(forwarded)
        .append(CALL)
        .append(POP)
        .append(GAS)
        .push_u256(U256::from(GAS_READING_SLOT))
        .append(SSTORE)
        .append(STOP)
        .build()
}

/// Runs `code` under both specs with `limits` and returns `(REX6, REX7)`.
fn run_both(
    build_db: impl Fn() -> MemoryDatabase,
    limits: &impl Fn(MegaSpecId) -> EvmTxRuntimeLimits,
) -> (Outcome, Outcome) {
    let r6 = transact(MegaSpecId::REX6, build_db(), limits(MegaSpecId::REX6));
    let r7 = transact(MegaSpecId::REX7, build_db(), limits(MegaSpecId::REX7));
    (r6, r7)
}

/// Asserts both arms succeeded and read back the same post-return `GAS` value.
fn assert_same_gas_reading(label: &str, r6: &Outcome, r7: &Outcome) {
    assert!(r6.is_success(), "{label}/REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "{label}/REX7 must succeed: {:?}", r7.result);
    let slot = U256::from(GAS_READING_SLOT);
    let reading = r7.storage_value(CONTRACT, slot);
    assert!(!reading.is_zero(), "{label}: the GAS reading must be non-zero");
    assert_eq!(
        r6.storage_value(CONTRACT, slot),
        reading,
        "{label}: the caller's counter after the return must match per-opcode accounting",
    );
    assert_outcomes_identical(label, r6, r7);
}

/// Leakage path 1 — the interception short-circuit, probed from the caller's own counter.
///
/// The interceptor produces a synthetic result without a child frame ever existing, so nothing on
/// that path unwinds a clamp. The clamp therefore has to be already restored when the CALL
/// checkpoint publishes the frame, and re-applied only once the caller resumes. Reading `GAS` right
/// after the CALL is the caller's direct view of that: a clamp that survived into `frame_init`, or
/// one restored twice, moves this reading.
#[test]
fn test_interception_short_circuit_leaves_the_callers_counter_whole() {
    let code = plain_filler(BytecodeBuilder::default(), 5)
        .append(TIMESTAMP)
        .append(POP)
        .mstore(0, IMegaLimitControl::remainingComputeGasCall::SELECTOR)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(4u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(1_000_000u64)
        .append(CALL)
        .append(POP)
        .append(GAS)
        .push_u256(U256::from(GAS_READING_SLOT))
        .append(SSTORE)
        .append(STOP)
        .build();
    let (r6, r7) = run_both(|| base_db(code.clone()), &detention_cap(1_000_000));
    assert_same_gas_reading("interception short-circuit", &r6, &r7);
}

/// Leakage path 2 — the TX-level rescue, probed by varying the transaction gas limit.
///
/// A TX-level compute exceed stops at the compute limit, so how much EVM gas the transaction was
/// given cannot change what it consumed. The clamp-hidden amount, on the other hand, is exactly
/// `gas_limit − headroom` and moves one-for-one with the gas limit — so if the rescue captured the
/// clamped view (burning the hidden gas) or the true remaining plus the hidden amount (refunding it
/// twice), the receipt would track the gas limit. It must not.
#[test]
fn test_tx_level_rescue_is_independent_of_the_transaction_gas_limit() {
    let code = countdown_loop_code(&[], 10_000);
    let intrinsic = transact(
        MegaSpecId::REX7,
        base_db(BytecodeBuilder::default().append(STOP).build()),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
    )
    .compute_gas;
    let limit = intrinsic + 5_000;
    let limits = compute_limit(limit);

    let mut readings = Vec::new();
    for spec in [MegaSpecId::REX6, MegaSpecId::REX7] {
        let small = transact_with_gas_limit(spec, base_db(code.clone()), limits(spec), 1_000_000);
        let large = transact_with_gas_limit(spec, base_db(code.clone()), limits(spec), 50_000_000);
        assert!(!small.is_success(), "{spec:?}: the tight compute limit must stop the tx");
        assert!(!large.is_success(), "{spec:?}: the tight compute limit must stop the tx");
        assert_eq!(
            small.compute_gas, large.compute_gas,
            "{spec:?}: the stop point must not depend on the transaction gas limit",
        );
        assert_eq!(
            small.gas_used, large.gas_used,
            "{spec:?}: the rescued gas must be the true remaining, so the receipt cannot track the \
             transaction gas limit; 1M limit -> {} and 50M limit -> {}",
            small.gas_used, large.gas_used
        );
        readings.push((small.compute_gas, small.gas_used));
    }
    let (r7_compute, r7_gas_used) = readings[1];
    assert_eq!(r7_compute, limit, "REX7 stops exactly at the compute limit");
    assert!(
        r7_gas_used < 1_000_000,
        "the clamp-hidden gas must reach the sender, not the burn; gas_used={r7_gas_used}",
    );
}

/// The same rescue probe with gas detention as the binding constraint, so the reclassification path
/// (`VolatileDataAccessOutOfGas` on a clamp-latched detention exceed) is the one under test.
#[test]
fn test_detained_rescue_is_independent_of_the_transaction_gas_limit() {
    let code = countdown_loop_code(&[TIMESTAMP, POP], 10_000);
    let limits = detention_cap(1_000);

    for spec in [MegaSpecId::REX6, MegaSpecId::REX7] {
        let small = transact_with_gas_limit(spec, base_db(code.clone()), limits(spec), 1_000_000);
        let large = transact_with_gas_limit(spec, base_db(code.clone()), limits(spec), 50_000_000);
        for (label, r) in [("1M", &small), ("50M", &large)] {
            assert!(!r.is_success(), "{spec:?}/{label}: the detention cap must stop the tx");
            assert!(
                matches!(r.halt_reason(label), MegaHaltReason::VolatileDataAccessOutOfGas { .. }),
                "{spec:?}/{label}: the halt must keep the volatile attribution; got {:?}",
                r.halt_reason(label),
            );
        }
        assert_eq!(
            small.gas_used, large.gas_used,
            "{spec:?}: a detained stop must rescue the true remaining, so the receipt cannot track \
             the transaction gas limit; 1M limit -> {} and 50M limit -> {}",
            small.gas_used, large.gas_used
        );
    }
}

/// Leakage path 3 — frame return, on the success arm.
///
/// The callee ends inside a plain segment, so its clamp is still outstanding when the frame
/// produces its result. Restoring it there is what lets the unspent remainder flow back to the
/// caller; the caller's `GAS` reading is what shows whether it did.
#[test]
fn test_frame_return_restores_the_clamp_on_success() {
    let callee = plain_filler(BytecodeBuilder::default(), 20).append(STOP).build();
    let code = call_then_store_gas(CALLEE, 1_000_000);
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());
    let (r6, r7) = run_both(build_db, &detention_cap(1_000_000));
    assert_same_gas_reading("frame return / success", &r6, &r7);
}

/// The same probe on the revert arm: the unwinding has to be unconditional, or one of the two exit
/// paths leaks.
#[test]
fn test_frame_return_restores_the_clamp_on_revert() {
    let callee = plain_filler(BytecodeBuilder::default(), 20)
        .sstore(U256::from(CALLEE_SLOT), U256::from(0x77))
        .revert()
        .build();
    let code = call_then_store_gas(CALLEE, 1_000_000);
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());
    let (r6, r7) = run_both(build_db, &detention_cap(1_000_000));
    assert_same_gas_reading("frame return / revert", &r6, &r7);
    for (label, r) in [("REX6", &r6), ("REX7", &r7)] {
        assert!(
            r.storage_value(CALLEE, U256::from(CALLEE_SLOT)).is_zero(),
            "{label}: the reverted sub-frame's write must be discarded",
        );
    }
}

/// Frame return on the exceed arm: the callee outruns its own frame-local compute budget, so its
/// clamp turns into a fake out-of-gas that is restored and reclassified into a revert. The gas the
/// clamp was hiding still belongs to the caller.
///
/// The two models stop the callee at different points — REX7 stops the crossing opcode before it
/// runs — so the caller resumes with different amounts and the readings are not comparable. What is
/// comparable is that the caller survives, sees a failed CALL, and is left with gas of the right
/// order rather than a burned or doubled counter.
#[test]
fn test_frame_local_exceed_returns_the_hidden_gas_to_the_parent() {
    let prologue =
        BytecodeBuilder::default().sstore(U256::from(CALLEE_SLOT), U256::from(0x77)).build_vec();
    let callee = countdown_loop_code(&prologue, 10_000);
    let code = plain_filler(BytecodeBuilder::default(), 5)
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(CALLEE)
        .push_number(50_000_000u64)
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    let intrinsic = transact(
        MegaSpecId::REX7,
        base_db(BytecodeBuilder::default().append(STOP).build()),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
    )
    .compute_gas;
    let build_db = || base_db(code.clone()).account_code(CALLEE, callee.clone());
    let (r6, r7) = run_both(build_db, &compute_limit(intrinsic + 100_000));

    for (label, r) in [("REX6", &r6), ("REX7", &r7)] {
        assert!(
            r.is_success(),
            "{label}: the caller survives a frame-local exceed: {:?}",
            r.result
        );
        assert_eq!(
            r.result.output().map(|o| U256::from_be_slice(o)),
            Some(U256::ZERO),
            "{label}: the CALL must report failure",
        );
        assert!(
            r.storage_value(CALLEE, U256::from(CALLEE_SLOT)).is_zero(),
            "{label}: the reverted sub-frame's write must be discarded",
        );
    }
    // The caller forwarded (63/64 of) tens of millions of gas and got a failed call back. Only the
    // callee's actual work may be gone: a clamp that was not restored would have stranded the
    // hidden millions in the child.
    assert!(
        r7.gas_used < 1_000_000,
        "the gas the clamp hid inside the callee must return to the caller; gas_used={}",
        r7.gas_used
    );
    assert!(
        r7.gas_used < r6.gas_used + 100_000,
        "REX7 must not consume materially more than per-opcode accounting; REX6={} REX7={}",
        r6.gas_used,
        r7.gas_used
    );
}

/// Frame return, ordering arm: the clamp must be restored *before* the code-deposit charge.
///
/// The deposit costs `CODEDEPOSIT_STORAGE_GAS` (10,000) per byte of deployed code, while the
/// compute it records is 200 per byte. Sizing the detention cap between the two — comfortably above
/// the deposit's compute, far below its EVM gas — leaves a CREATE that can only be paid for out of
/// the counter the clamp was hiding. If the charge saw the clamped copy, this deployment would fail
/// out of gas despite the transaction being nowhere near any limit.
#[test]
fn test_create_return_restores_the_clamp_before_the_code_deposit_charge() {
    let runtime = vec![STOP; 100];
    let initcode = BytecodeBuilder::default().return_with_data(&runtime).build_vec();
    let len = initcode.len() as u64;
    let code = plain_filler(BytecodeBuilder::default(), 5)
        .append(TIMESTAMP)
        .append(POP)
        .mstore(0, &initcode)
        .push_number(len) // length
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    // 100 bytes of runtime code: 20,000 compute for the deposit, 1,000,000 EVM gas for it.
    let deposit_compute = runtime.len() as u64 * 200;
    let deposit_evm_gas = runtime.len() as u64 * 10_000;
    let cap = 80_000;
    assert!(
        deposit_compute < cap && cap < deposit_evm_gas,
        "the cap has to sit between the deposit's compute and its EVM gas",
    );

    let (r6, r7) = run_both(|| base_db(code.clone()), &detention_cap(cap));

    assert!(r6.is_success(), "REX6 must succeed: {:?}", r6.result);
    assert!(r7.is_success(), "REX7 must succeed: {:?}", r7.result);
    let created =
        r7.result.output().map(|o| U256::from_be_slice(o)).expect("CREATE must return output");
    assert!(!created.is_zero(), "the CREATE must have succeeded; a zero address means it OOG'd");
    assert_eq!(
        r6.result.output().map(|o| U256::from_be_slice(o)),
        Some(created),
        "both models must deploy to the same address",
    );
    assert_outcomes_identical("CREATE under a clamp", &r6, &r7);
}
