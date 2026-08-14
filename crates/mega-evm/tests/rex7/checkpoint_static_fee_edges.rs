//! Table-prepaid checkpoint static-fee edges under REX7 gas-clamp enforcement.
//!
//! revm's `step()` pre-charges an opcode's gas-table entry before the handler runs. Under REX7 the
//! zeroed set is only the volatile-guarded family, so `GAS` (2) and `LOG0`–`LOG4` (375) keep a
//! non-zero entry. When the clamp's visible remainder is below that entry and true EVM gas is
//! still sufficient, the inherited per-opcode check stops the opcode before the body — a
//! plain-segment crossing: Halt(`ComputeGasLimitExceeded`), the fee never enters compute usage,
//! and the body has no observable effect.
//!
//! `CREATE` / `CREATE2` are the contrast: their table entry is 0 (revm charges 32,000 inside the
//! body, after the checkpoint restores the true counter). A compute headroom of `32_000 − 1` does
//! not stop them before the body; the body runs, the fee is recorded, and a top-frame exceed
//! surfaces as `Revert(MegaLimitExceeded)` with the created account discarded.
//!
//! Each family has two top-frame edges, calibrated so the named headroom is the remaining compute
//! at the opcode itself (prefix `PUSH` opcodes are measured out first).

use crate::common::{transact, Outcome, CALLER, CONTRACT, ONE_ETH};
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    constants::mini_rex::LOG_TOPIC_STORAGE_GAS,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitKind, MegaHaltReason, MegaLimitExceeded, MegaSpecId,
};
use revm::{
    bytecode::opcode::{CREATE, GAS, LOG1, STOP},
    context::result::ExecutionResult,
};

/// `GAS` static gas — prepaid by the inherited table.
const GAS_STATIC_GAS: u64 = 2;

/// `LOG0`–`LOG4` table entry (base LOG cost). Per-topic and per-byte costs are charged in the body.
const LOG_STATIC_GAS: u64 = 375;

/// `LOG1` with empty data: table 375 + one topic 375.
const LOG1_BODY_COMPUTE: u64 = 750;

/// `CREATE` body fee. The REX7 table entry is 0; revm charges this inside the body.
const CREATE_BODY_GAS: u64 = 32_000;

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

fn compute_limit(limit: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7).with_tx_compute_gas_limit(limit)
}

fn run(code: Bytes, limit: u64) -> Outcome {
    transact(MegaSpecId::REX7, base_db(code), compute_limit(limit))
}

fn unconstrained(code: Bytes) -> Outcome {
    transact(MegaSpecId::REX7, base_db(code), EvmTxRuntimeLimits::from_spec(MegaSpecId::REX7))
}

fn stop_only() -> Bytes {
    BytecodeBuilder::default().append(STOP).build()
}

fn storage_overhead(intrinsic: &Outcome) -> u64 {
    intrinsic.gas_used - intrinsic.compute_gas
}

fn account_nonce(outcome: &Outcome, address: Address) -> u64 {
    outcome.state.get(&address).map(|account| account.info.nonce).unwrap_or(0)
}

fn created_addresses(outcome: &Outcome) -> Vec<Address> {
    outcome
        .state
        .iter()
        .filter(|(_, account)| account.is_created())
        .map(|(address, _)| *address)
        .collect()
}

/// Transaction-level clamp crossing: Halt, usage stops at the limit, the opcode's fee is not in
/// compute, receipt gas is compute plus the intrinsic storage component.
fn assert_tx_level_crossing(label: &str, outcome: &Outcome, limit: u64, storage_overhead: u64) {
    match outcome.halt_reason(label) {
        MegaHaltReason::ComputeGasLimitExceeded { limit: reported, actual } => {
            assert_eq!(*reported, limit, "{label}: reported limit is the TX compute limit");
            assert_eq!(
                *actual, outcome.compute_gas,
                "{label}: reported actual is the transaction's final compute usage"
            );
        }
        other => panic!("{label}: expected ComputeGasLimitExceeded, got {other:?}"),
    }
    assert_eq!(
        outcome.compute_gas, limit,
        "{label}: crossing usage stops at the limit; the opcode's fee must not be recorded"
    );
    assert_eq!(
        outcome.gas_used,
        outcome.compute_gas + storage_overhead,
        "{label}: receipt gas is compute plus the intrinsic storage component"
    );
}

// ---------------------------------------------------------------------------------------------
// GAS (table static = 2)
// ---------------------------------------------------------------------------------------------

/// `GAS; STOP` with compute headroom `static − 1`.
///
/// The table pre-charges 2, so the clamp stops `GAS` before the handler. A charge after the
/// prologue would let the body run and record 2.
#[test]
fn test_gas_one_below_static_is_a_plain_segment_crossing() {
    let intrinsic = unconstrained(stop_only());
    assert_eq!(intrinsic.compute_gas, 21_000, "intrinsic compute is 21_000");
    let limit = intrinsic.compute_gas + GAS_STATIC_GAS - 1;
    assert_eq!(limit, 21_001);

    let outcome = run(BytecodeBuilder::default().append(GAS).append(STOP).build(), limit);

    assert_tx_level_crossing(
        "GAS headroom=static-1",
        &outcome,
        limit,
        storage_overhead(&intrinsic),
    );
}

/// Neighbouring edge: headroom equals the static fee, so `GAS` itself can finish.
#[test]
fn test_gas_at_static_executes_the_body() {
    let intrinsic = unconstrained(stop_only());
    let limit = intrinsic.compute_gas + GAS_STATIC_GAS;
    assert_eq!(limit, 21_002);

    let outcome = run(BytecodeBuilder::default().append(GAS).append(STOP).build(), limit);

    assert!(
        outcome.is_success(),
        "headroom=static_gas must let GAS finish; got {:?}",
        outcome.result
    );
    assert_eq!(outcome.compute_gas, intrinsic.compute_gas + GAS_STATIC_GAS);
    assert_eq!(outcome.gas_used, outcome.compute_gas + storage_overhead(&intrinsic));
}

// ---------------------------------------------------------------------------------------------
// LOG1 (table static = 375; empty-data body adds 375 for the topic)
// ---------------------------------------------------------------------------------------------

fn log1_operands(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder.push_number(0xabu64).push_number(0u64).push_number(0u64)
}

/// `LOG1; STOP` with compute headroom `table_static − 1` at the opcode.
///
/// The three operand `PUSH` opcodes are measured out first so the named headroom is the remainder
/// the clamp shows `LOG1`. The table pre-charges 375, so 374 stops the opcode before the body: no
/// log, no topic storage gas.
#[test]
fn test_log1_one_below_static_is_a_plain_segment_crossing() {
    let intrinsic = unconstrained(stop_only());
    let before = unconstrained(log1_operands(BytecodeBuilder::default()).append(STOP).build());
    let limit = before.compute_gas + LOG_STATIC_GAS - 1;

    let outcome =
        run(log1_operands(BytecodeBuilder::default()).append(LOG1).append(STOP).build(), limit);

    assert_tx_level_crossing(
        "LOG1 headroom=static-1",
        &outcome,
        limit,
        storage_overhead(&intrinsic),
    );
    assert!(
        outcome.result.logs().is_empty(),
        "LOG1 body must not run; logs={:?}",
        outcome.result.logs()
    );
}

/// Neighbouring edge: headroom equals the full `LOG1` body cost, so the log is emitted.
///
/// Table static is 375; the topic adds another 375. Headroom of 375 would enter the handler and
/// then overshoot on the topic, reverting the log. The executing edge is the full body cost.
#[test]
fn test_log1_at_full_body_cost_emits_the_log() {
    let intrinsic = unconstrained(stop_only());
    let before = unconstrained(log1_operands(BytecodeBuilder::default()).append(STOP).build());
    let full =
        unconstrained(log1_operands(BytecodeBuilder::default()).append(LOG1).append(STOP).build());
    assert_eq!(
        full.compute_gas,
        before.compute_gas + LOG1_BODY_COMPUTE,
        "empty-data LOG1 compute is table 375 plus one topic 375"
    );
    let limit = before.compute_gas + LOG1_BODY_COMPUTE;

    let outcome =
        run(log1_operands(BytecodeBuilder::default()).append(LOG1).append(STOP).build(), limit);

    assert!(
        outcome.is_success(),
        "headroom=full LOG1 body must emit the log; got {:?}",
        outcome.result
    );
    assert_eq!(outcome.compute_gas, full.compute_gas);
    assert_eq!(outcome.result.logs().len(), 1, "LOG1 body must emit exactly one log");
    assert_eq!(
        outcome.gas_used,
        outcome.compute_gas + storage_overhead(&intrinsic) + LOG_TOPIC_STORAGE_GAS,
        "receipt gas includes the LOG topic storage component"
    );
}

// ---------------------------------------------------------------------------------------------
// CREATE (table static = 0; body charges 32,000 after the true counter is restored)
// ---------------------------------------------------------------------------------------------

fn create_operands(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder.push_number(0u64).push_number(0u64).push_number(0u64)
}

fn decode_top_revert(label: &str, outcome: &Outcome) -> MegaLimitExceeded {
    match &outcome.result {
        ExecutionResult::Revert { output, .. } => MegaLimitExceeded::abi_decode(output)
            .unwrap_or_else(|e| panic!("{label}: revert is not MegaLimitExceeded: {e}")),
        ExecutionResult::Halt { reason, .. } => panic!(
            "{label}: CREATE table entry is 0, so headroom below 32_000 must not be a clamp \
             Halt (that would mean the 32_000 was prepaid); got {reason:?} compute={}",
            outcome.compute_gas
        ),
        other => panic!("{label}: expected Revert(MegaLimitExceeded), got {other:?}"),
    }
}

/// `CREATE; STOP` with compute headroom `32_000 − 1` at the opcode.
///
/// The table does not pre-charge CREATE, so the body runs on the restored counter, records
/// 32,000, and the top-frame per-opcode exceed reverts. The created account is discarded; the
/// fee stays in compute. A table entry of 32,000 would have made this a clamp Halt instead.
#[test]
fn test_create_one_below_body_fee_runs_then_reverts() {
    let intrinsic = unconstrained(stop_only());
    let before = unconstrained(create_operands(BytecodeBuilder::default()).append(STOP).build());
    let full = unconstrained(
        create_operands(BytecodeBuilder::default()).append(CREATE).append(STOP).build(),
    );
    assert_eq!(
        full.compute_gas,
        before.compute_gas + CREATE_BODY_GAS,
        "empty-initcode CREATE compute is the 32_000 body fee"
    );
    let limit = before.compute_gas + CREATE_BODY_GAS - 1;

    let outcome =
        run(create_operands(BytecodeBuilder::default()).append(CREATE).append(STOP).build(), limit);

    let decoded = decode_top_revert("CREATE headroom=32000-1", &outcome);
    assert_eq!(decoded.kind, LimitKind::ComputeGas.as_u8());
    assert_eq!(
        outcome.compute_gas, full.compute_gas,
        "the 32_000 body fee is recorded; a table-prepaid crossing would stop at {limit}"
    );
    assert!(
        created_addresses(&outcome).is_empty(),
        "the reverted CREATE must not leave a created account; created={:?}",
        created_addresses(&outcome)
    );
    assert_eq!(account_nonce(&outcome, CONTRACT), 0, "the creator nonce must not advance");
    assert_eq!(
        outcome.gas_used,
        outcome.compute_gas + storage_overhead(&intrinsic),
        "empty-initcode CREATE adds no storage gas at minimum bucket capacity"
    );
}

/// Neighbouring edge: headroom equals the 32,000 body fee, so CREATE finishes and the account
/// remains.
#[test]
fn test_create_at_body_fee_creates_the_account() {
    let intrinsic = unconstrained(stop_only());
    let before = unconstrained(create_operands(BytecodeBuilder::default()).append(STOP).build());
    let limit = before.compute_gas + CREATE_BODY_GAS;

    let outcome =
        run(create_operands(BytecodeBuilder::default()).append(CREATE).append(STOP).build(), limit);

    assert!(
        outcome.is_success(),
        "headroom=32000 must let CREATE finish; got {:?}",
        outcome.result
    );
    assert_eq!(outcome.compute_gas, before.compute_gas + CREATE_BODY_GAS);
    assert_eq!(account_nonce(&outcome, CONTRACT), 1, "CREATE must advance the creator nonce");
    assert_eq!(
        created_addresses(&outcome).len(),
        1,
        "CREATE must leave one created account; created={:?}",
        created_addresses(&outcome)
    );
    assert_eq!(outcome.gas_used, outcome.compute_gas + storage_overhead(&intrinsic));
}
