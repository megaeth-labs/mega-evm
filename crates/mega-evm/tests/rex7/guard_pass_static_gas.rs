//! Guard-pass static-gas position under REX7 checkpoint accounting.
//!
//! Charge-on-reject debits a disabled opcode's static entry on the reject arm only. A passing
//! guard must charge at the same position the baseline REX7 handler used. For the
//! checkpoint-wrapped families that is after [`checkpoint_prologue!`] restores the true
//! counter, so a compute headroom of `static_gas − 1` lets the body run and reports a
//! frame-local `MegaLimitExceeded` rather than a clamp halt that never reads the host.
//!
//! `headroom = static_gas − 1` and `headroom = static_gas` are the two edges: the first
//! overshoots after the body, the second can afford the charge. `TIMESTAMP` is exercised in
//! the top-level frame and in a nested child; `SELFBALANCE` covers the dedicated checkpoint
//! handler at the top-level edges. `remainingComputeGas` is the value
//! `MegaLimitControl.remainingComputeGas` would return after the transaction: the TX-level
//! remaining, which is what the interceptor reads once the frames have been popped.

use crate::common::{
    base_db, context, drive, rex7_compute_limit, stop_only, transact, Outcome, CALLEE, CALLER,
    CONTRACT, DEFAULT_TX_GAS_LIMIT,
};
use alloy_primitives::{Bytes, U256};
use alloy_sol_types::{SolCall as _, SolError};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IMegaLimitControl, LimitKind, MegaEvm, MegaLimitExceeded, MegaSpecId,
    MegaTransaction, MegaTransactionNew as _, VolatileDataAccess, LIMIT_CONTROL_ADDRESS,
};
use revm::{
    bytecode::opcode::{CALL, MLOAD, POP, SELFBALANCE, SSTORE, STATICCALL, STOP, TIMESTAMP},
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    handler::EvmTr,
};

/// `TIMESTAMP` static gas — the unconditional-family representative.
const TIMESTAMP_STATIC_GAS: u64 = 2;

/// `SELFBALANCE` static gas (`LOW`) — the dedicated-checkpoint representative.
const SELFBALANCE_STATIC_GAS: u64 = 5;

/// Slot the nested caller stores its `remainingComputeGas` reading into.
const REMAINING_SLOT: u64 = 0xc0;

/// Codex / knife-edge program: `TIMESTAMP; POP; STOP`.
fn timestamp_pop_stop() -> Bytes {
    BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build()
}

/// The same checkpoint with nothing after it, so `headroom = static_gas` can finish.
fn timestamp_stop() -> Bytes {
    BytecodeBuilder::default().append(TIMESTAMP).stop().build()
}

/// Dedicated-checkpoint sibling of [`timestamp_pop_stop`]: `SELFBALANCE; POP; STOP`.
fn selfbalance_pop_stop() -> Bytes {
    BytecodeBuilder::default().append(SELFBALANCE).append(POP).stop().build()
}

/// Dedicated-checkpoint sibling of [`timestamp_stop`]: `SELFBALANCE; STOP`.
fn selfbalance_stop() -> Bytes {
    BytecodeBuilder::default().append(SELFBALANCE).stop().build()
}

struct GuardPassRun {
    outcome: Outcome,
    accessed: VolatileDataAccess,
    /// Post-tx TX-level remaining — what `remainingComputeGas` returns with no frame on the stack.
    remaining_compute_gas: u64,
}

fn run(code: Bytes, limits: EvmTxRuntimeLimits) -> GuardPassRun {
    run_db(base_db(code), limits)
}

fn run_db(mut db: MemoryDatabase, limits: EvmTxRuntimeLimits) -> GuardPassRun {
    let mut evm = MegaEvm::new(context(&mut db, MegaSpecId::REX7, limits));
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(DEFAULT_TX_GAS_LIMIT)
        .build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = drive(MegaSpecId::REX7, &mut evm, tx);
    let remaining_compute_gas =
        evm.ctx_ref().additional_limit.borrow().current_call_remaining_compute_gas();
    let accessed = evm.ctx_ref().volatile_data_tracker.borrow().get_volatile_data_accessed();
    GuardPassRun { outcome, accessed, remaining_compute_gas }
}

fn intrinsic_stop() -> Outcome {
    transact(
        MegaSpecId::REX7,
        base_db(stop_only()),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
    )
}

fn assert_timestamp_marked(label: &str, run: &GuardPassRun) {
    assert!(
        run.accessed.contains(VolatileDataAccess::TIMESTAMP),
        "{label}: TIMESTAMP body must run and mark detention; accessed={:?}",
        run.accessed
    );
}

fn assert_timestamp_unmarked(label: &str, run: &GuardPassRun) {
    assert!(
        !run.accessed.contains(VolatileDataAccess::TIMESTAMP),
        "{label}: TIMESTAMP must not have marked; accessed={:?}",
        run.accessed
    );
}

fn decode_top_revert(label: &str, outcome: &Outcome) -> MegaLimitExceeded {
    match &outcome.result {
        ExecutionResult::Revert { output, .. } => MegaLimitExceeded::abi_decode(output)
            .unwrap_or_else(|e| panic!("{label}: revert is not MegaLimitExceeded: {e}")),
        other => panic!("{label}: expected Revert(MegaLimitExceeded), got {other:?}"),
    }
}

fn call_callee(builder: BytecodeBuilder, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(CALLEE)
        .push_number(gas)
        .append(CALL)
}

fn store_remaining_compute_gas(builder: BytecodeBuilder, slot: u64) -> BytecodeBuilder {
    builder
        .mstore(0, IMegaLimitControl::remainingComputeGasCall::SELECTOR)
        .push_number(32u64)
        .push_number(0u64)
        .push_number(4u64)
        .push_number(0u64)
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(1_000_000u64)
        .append(STATICCALL)
        .append(POP)
        .push_number(0u64)
        .append(MLOAD)
        .push_u256(U256::from(slot))
        .append(SSTORE)
}

/// Parent that only calls the child and stops — used to place the compute knife edge on
/// the child's first opcode rather than on a later `remainingComputeGas` CALL.
fn nested_db(child: Bytes) -> MemoryDatabase {
    let parent = call_callee(BytecodeBuilder::default(), 50_000_000).append(STOP).build();
    base_db(parent).account_code(CALLEE, child)
}

/// Same parent, then a `remainingComputeGas` read. Only used when the TX compute limit is
/// unconstrained, so the CALL itself is not the binding opcode.
fn nested_db_with_remaining_read(child: Bytes) -> MemoryDatabase {
    let parent = store_remaining_compute_gas(
        call_callee(BytecodeBuilder::default(), 50_000_000),
        REMAINING_SLOT,
    )
    .append(STOP)
    .build();
    base_db(parent).account_code(CALLEE, child)
}

/// Codex reproduction: `TIMESTAMP; POP; STOP` with compute limit `intrinsic + 1`.
///
/// Headroom at `TIMESTAMP` is `static_gas − 1`. Charging after the prologue records the body
/// and reverts `MegaLimitExceeded` with compute `intrinsic + 2`. Charging before the prologue
/// lets the clamp stop the opcode: a TX-level `Halt(ComputeGasLimitExceeded)` with compute
/// `intrinsic + 1` and no detention mark.
#[test]
fn test_top_frame_timestamp_one_below_static_gas_reverts_after_the_body() {
    let intrinsic = intrinsic_stop();
    assert_eq!(intrinsic.compute_gas, 21_000, "intrinsic compute is 21_000");
    let limit = intrinsic.compute_gas + TIMESTAMP_STATIC_GAS - 1;
    assert_eq!(limit, 21_001);

    let run = run(timestamp_pop_stop(), rex7_compute_limit(limit));

    let decoded = decode_top_revert("headroom=static-1", &run.outcome);
    assert_eq!(decoded.kind, LimitKind::ComputeGas.as_u8());
    // Top-frame remaining after intrinsic equals the headroom (`static_gas − 1`). The
    // per-opcode record path classifies that as frame-local, so the payload names the
    // frame budget (1), not the TX compute limit (21001).
    assert_eq!(
        decoded.limit,
        TIMESTAMP_STATIC_GAS - 1,
        "the payload names the top-frame budget that the body overshot"
    );
    assert_eq!(
        run.outcome.compute_gas,
        intrinsic.compute_gas + TIMESTAMP_STATIC_GAS,
        "the body charge is recorded; compute must be 21002, not the clamp's 21001"
    );
    assert_eq!(run.outcome.compute_gas, 21_002);
    assert_eq!(
        run.outcome.gas_used,
        run.outcome.compute_gas + intrinsic.storage_overhead(),
        "receipt gas is compute plus the intrinsic storage component"
    );
    assert_eq!(run.outcome.gas_used, 60_002);
    assert_timestamp_marked("headroom=static-1", &run);
    assert_eq!(
        run.remaining_compute_gas, 0,
        "usage is past the limit, so remainingComputeGas is zero"
    );
}

/// Neighbouring edge: headroom equals the static fee, so `TIMESTAMP` itself can finish.
///
/// Nothing follows the checkpoint (`TIMESTAMP; STOP`) so the transaction continues rather
/// than walking into a zero-headroom clamp on `POP`.
#[test]
fn test_top_frame_timestamp_at_static_gas_continues() {
    let intrinsic = intrinsic_stop();
    let limit = intrinsic.compute_gas + TIMESTAMP_STATIC_GAS;
    assert_eq!(limit, 21_002);

    let run = run(timestamp_stop(), rex7_compute_limit(limit));

    assert!(
        run.outcome.is_success(),
        "headroom=static_gas must let TIMESTAMP finish; got {:?}",
        run.outcome.result
    );
    assert_eq!(run.outcome.compute_gas, intrinsic.compute_gas + TIMESTAMP_STATIC_GAS);
    assert_eq!(run.outcome.gas_used, run.outcome.compute_gas + intrinsic.storage_overhead(),);
    assert_timestamp_marked("headroom=static_gas", &run);
    assert_eq!(run.remaining_compute_gas, 0);
}

/// Nested sibling of the Codex edge: the child's first opcode is `TIMESTAMP`, and the TX
/// compute limit leaves `static_gas − 1` of headroom when that opcode is reached.
#[test]
fn test_nested_timestamp_one_below_static_gas_reverts_after_the_body() {
    let empty = run_db(nested_db(stop_only()), EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7));
    assert!(
        empty.outcome.is_success(),
        "calibration child must succeed: {:?}",
        empty.outcome.result
    );
    let before = empty.outcome.compute_gas;
    let limit = before + TIMESTAMP_STATIC_GAS - 1;

    let run = run_db(nested_db(timestamp_stop()), rex7_compute_limit(limit));

    assert_timestamp_marked("nested headroom=static-1", &run);
    assert_eq!(
        run.outcome.compute_gas,
        before + TIMESTAMP_STATIC_GAS,
        "the child body must be recorded; clamp-before-prologue would stop at {limit}"
    );
    match &run.outcome.result {
        ExecutionResult::Revert { output, .. } => {
            let decoded = MegaLimitExceeded::abi_decode(output)
                .unwrap_or_else(|e| panic!("nested revert is not MegaLimitExceeded: {e}"));
            assert_eq!(decoded.kind, LimitKind::ComputeGas.as_u8());
        }
        ExecutionResult::Success { .. } => {
            // Frame-local child revert absorbed by the parent. The TX remaining is what
            // `remainingComputeGas` would report; the body overshot, so it is zero.
        }
        ExecutionResult::Halt { reason, .. } => {
            panic!(
                "nested headroom=static-1 must not be a clamp Halt (the e4dfbca shape); \
                 got {reason:?} compute={} marked={}",
                run.outcome.compute_gas,
                run.accessed.contains(VolatileDataAccess::TIMESTAMP),
            );
        }
    }
    assert_eq!(
        run.remaining_compute_gas, 0,
        "remainingComputeGas after the body overshoot is zero"
    );
}

/// Nested sibling of the exact-fee edge: the child can afford `TIMESTAMP` and returns, so
/// the parent continues.
#[test]
fn test_nested_timestamp_at_static_gas_continues() {
    let empty = run_db(nested_db(stop_only()), EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7));
    let before = empty.outcome.compute_gas;
    let limit = before + TIMESTAMP_STATIC_GAS;

    let run = run_db(nested_db(timestamp_stop()), rex7_compute_limit(limit));

    assert_timestamp_marked("nested headroom=static_gas", &run);
    assert_eq!(run.outcome.compute_gas, before + TIMESTAMP_STATIC_GAS);
    assert!(
        run.outcome.is_success(),
        "headroom=static_gas must let the child TIMESTAMP finish and the parent continue; got {:?}",
        run.outcome.result
    );
    assert_eq!(run.remaining_compute_gas, 0);
}

/// Unconstrained nested read: after a child `TIMESTAMP` the parent's `remainingComputeGas`
/// is the detained remaining, which is how the mark is observed on chain.
#[test]
fn test_nested_timestamp_remaining_compute_gas_is_detained() {
    let empty = run_db(
        nested_db_with_remaining_read(stop_only()),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
    );
    let with_ts = run_db(
        nested_db_with_remaining_read(timestamp_stop()),
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7),
    );
    assert!(empty.outcome.is_success(), "empty child must succeed: {:?}", empty.outcome.result);
    assert!(
        with_ts.outcome.is_success(),
        "TIMESTAMP child must succeed: {:?}",
        with_ts.outcome.result
    );
    assert_timestamp_unmarked("empty child", &empty);
    assert_timestamp_marked("TIMESTAMP child", &with_ts);

    let empty_reading: u64 = empty
        .outcome
        .storage_value(CONTRACT, U256::from(REMAINING_SLOT))
        .try_into()
        .expect("remainingComputeGas fits in u64");
    let ts_reading: u64 = with_ts
        .outcome
        .storage_value(CONTRACT, U256::from(REMAINING_SLOT))
        .try_into()
        .expect("remainingComputeGas fits in u64");
    assert!(empty_reading > 0, "undetained remainingComputeGas must be non-zero");
    assert!(
        ts_reading < empty_reading,
        "TIMESTAMP detention must shrink remainingComputeGas; empty={empty_reading} ts={ts_reading}"
    );
    assert_eq!(with_ts.outcome.compute_gas, empty.outcome.compute_gas + TIMESTAMP_STATIC_GAS,);
}

/// Dedicated-checkpoint sibling of the Codex edge: `SELFBALANCE; POP; STOP` with compute
/// limit `intrinsic + 4`.
///
/// Headroom at `SELFBALANCE` is `static_gas − 1`. Charging after the prologue records the
/// body and reverts `MegaLimitExceeded` with compute `intrinsic + 5`. Charging before the
/// prologue (the e4dfbca shape) lets the clamp stop the opcode: a TX-level
/// `Halt(ComputeGasLimitExceeded)` with compute `intrinsic + 4`.
#[test]
fn test_top_frame_selfbalance_one_below_static_gas_reverts_after_the_body() {
    let intrinsic = intrinsic_stop();
    assert_eq!(intrinsic.compute_gas, 21_000, "intrinsic compute is 21_000");
    let limit = intrinsic.compute_gas + SELFBALANCE_STATIC_GAS - 1;
    assert_eq!(limit, 21_004);

    let run = run(selfbalance_pop_stop(), rex7_compute_limit(limit));

    let decoded = decode_top_revert("SELFBALANCE headroom=static-1", &run.outcome);
    assert_eq!(decoded.kind, LimitKind::ComputeGas.as_u8());
    // Top-frame remaining after intrinsic equals the headroom (`static_gas − 1`). The
    // per-opcode record path classifies that as frame-local, so the payload names the
    // frame budget (4), not the TX compute limit (21004).
    assert_eq!(
        decoded.limit,
        SELFBALANCE_STATIC_GAS - 1,
        "the payload names the top-frame budget that the body overshot"
    );
    assert_eq!(
        run.outcome.compute_gas,
        intrinsic.compute_gas + SELFBALANCE_STATIC_GAS,
        "the body charge is recorded; compute must be 21005, not the clamp's 21004"
    );
    assert_eq!(run.outcome.compute_gas, 21_005);
    assert_eq!(
        run.outcome.gas_used,
        run.outcome.compute_gas + intrinsic.storage_overhead(),
        "receipt gas is compute plus the intrinsic storage component"
    );
    assert_eq!(run.outcome.gas_used, 60_005);
    assert_eq!(
        run.remaining_compute_gas, 0,
        "usage is past the limit, so remainingComputeGas is zero"
    );
}

/// Neighbouring edge: headroom equals the static fee, so `SELFBALANCE` itself can finish.
///
/// Nothing follows the checkpoint (`SELFBALANCE; STOP`) so the transaction continues rather
/// than walking into a zero-headroom clamp on `POP`.
#[test]
fn test_top_frame_selfbalance_at_static_gas_continues() {
    let intrinsic = intrinsic_stop();
    let limit = intrinsic.compute_gas + SELFBALANCE_STATIC_GAS;
    assert_eq!(limit, 21_005);

    let run = run(selfbalance_stop(), rex7_compute_limit(limit));

    assert!(
        run.outcome.is_success(),
        "headroom=static_gas must let SELFBALANCE finish; got {:?}",
        run.outcome.result
    );
    assert_eq!(run.outcome.compute_gas, intrinsic.compute_gas + SELFBALANCE_STATIC_GAS);
    assert_eq!(run.outcome.gas_used, run.outcome.compute_gas + intrinsic.storage_overhead(),);
    assert_eq!(run.remaining_compute_gas, 0);
}
