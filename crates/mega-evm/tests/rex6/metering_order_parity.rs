//! REX6 unifies the per-opcode gas-metering order: compute gas is recorded exactly once, after the
//! opcode body has fully executed, with the storage gas charged in the opcode excluded.
//!
//! For every storage-affecting opcode **except CREATE2**, the pre-REX6 layering already followed
//! this order (nothing consumes EVM gas before the storage-gas charge), so the REX6 canonical path
//! must be byte-for-byte behavior-preserving relative to REX5: identical execution result,
//! compute-gas tracker reading, and `gas_used`.
//!
//! These tests pin that equivalence. CREATE2's intended divergence on the failure path is covered
//! in [`super::create2_metering_order`].

use crate::common::{transact, transact_default, CALLER, CONTRACT, EMPTY_TARGET};
use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaSpecId,
};
use revm::bytecode::opcode::{
    CALL, CALLCODE, CREATE, DELEGATECALL, LOG1, SELFDESTRUCT, SSTORE, STATICCALL, STOP,
};

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// Runs `build_db()` under REX5 and REX6 (identical runtime limits) and asserts the execution
/// result, compute-gas reading, and `gas_used` are identical — i.e. the REX6 canonical metering
/// order reproduces the frozen REX5 behavior exactly.
fn assert_rex5_rex6_parity(
    label: &str,
    expect_success: bool,
    build_db: impl Fn() -> MemoryDatabase,
) {
    let r5 = transact_default(MegaSpecId::REX5, build_db());
    let r6 = transact_default(MegaSpecId::REX6, build_db());

    assert_eq!(
        r5.is_success(),
        expect_success,
        "{label}: REX5 success expectation mismatch; got {:?}",
        r5.result
    );
    assert_eq!(
        r5.is_success(),
        r6.is_success(),
        "{label}: success mismatch REX5={:?} REX6={:?}",
        r5.result,
        r6.result
    );
    assert_eq!(
        r5.compute_gas, r6.compute_gas,
        "{label}: compute_gas must be identical (behavior-preserving); REX5={} REX6={}",
        r5.compute_gas, r6.compute_gas
    );
    assert_eq!(
        r5.gas_used, r6.gas_used,
        "{label}: gas_used must be identical (behavior-preserving); REX5={} REX6={}",
        r5.gas_used, r6.gas_used
    );
}

fn base_db(code: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10 * ONE_ETH))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(ONE_ETH))
}

#[test]
fn test_sstore_metering_parity_rex5_rex6() {
    // SSTORE zero -> non-zero triggers the dynamic storage-gas path in `storage_gas_ext::sstore`.
    let code = BytecodeBuilder::default().sstore(U256::from(7), U256::from(99)).stop().build();
    assert_rex5_rex6_parity("SSTORE", true, || base_db(code.clone()));
}

#[test]
fn test_log_metering_parity_rex5_rex6() {
    // LOG1 with 32 bytes of data exercises the topic + data storage-gas charge in
    // `storage_gas_ext::log`. Stack for LOG1 pops offset, len, topic (offset on top).
    let code = BytecodeBuilder::default()
        .mstore(0, [0x11u8; 32])
        .push_number(0xabcu64) // topic0
        .push_number(32u64) // len
        .push_number(0u64) // offset
        .append(LOG1)
        .stop()
        .build();
    assert_rex5_rex6_parity("LOG1", true, || base_db(code.clone()));
}

#[test]
fn test_call_value_transfer_metering_parity_rex5_rex6() {
    // CALL transferring value to an empty account exercises the new-account storage-gas path in
    // `storage_gas_ext::call`. Stack pops gas, to, value, aoff, asz, roff, rsz (gas on top).
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(1u64) // value
        .push_address(EMPTY_TARGET) // to
        .push_number(1_000_000u64) // gas
        .append(CALL)
        .stop()
        .build();
    assert_rex5_rex6_parity("CALL", true, || base_db(code.clone()));
}

#[test]
fn test_create_metering_parity_rex5_rex6() {
    // CREATE (non-CREATE2) with empty initcode. Stack pops value, offset, length (value on top).
    let code = BytecodeBuilder::default()
        .push_number(0u64) // length
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE)
        .stop()
        .build();
    assert_rex5_rex6_parity("CREATE", true, || base_db(code.clone()));
}

#[test]
fn test_selfdestruct_new_beneficiary_metering_parity_rex5_rex6() {
    // SELFDESTRUCT to an empty, non-beneficiary target while the contract holds balance exercises
    // the new-account storage-gas path in `storage_gas_ext::selfdestruct` (REX5+).
    let code = BytecodeBuilder::default().push_address(EMPTY_TARGET).append(SELFDESTRUCT).build();
    assert_rex5_rex6_parity("SELFDESTRUCT", true, || base_db(code.clone()));
}

#[test]
fn test_delegatecall_metering_parity_rex5_rex6() {
    // DELEGATECALL has no value operand, so `storage_charged` is always 0; this pins the
    // `storage_gas_ext::delegate_call` REX6 path. Stack pops gas, to, argsOffset, argsSize,
    // retOffset, retSize (gas on top).
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_address(EMPTY_TARGET) // to
        .push_number(1_000_000u64) // gas
        .append(DELEGATECALL)
        .stop()
        .build();
    assert_rex5_rex6_parity("DELEGATECALL", true, || base_db(code.clone()));
}

#[test]
fn test_staticcall_metering_parity_rex5_rex6() {
    // STATICCALL has no value operand either; same stack layout as DELEGATECALL.
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_address(EMPTY_TARGET) // to
        .push_number(1_000_000u64) // gas
        .append(STATICCALL)
        .stop()
        .build();
    assert_rex5_rex6_parity("STATICCALL", true, || base_db(code.clone()));
}

/// Pushes the same operands as a `SSTORE(slot=7, value=99)` but stops before the SSTORE byte, so
/// the run measures the compute-gas accumulated up to the opcode under test (TX intrinsic + the
/// two PUSH instructions). This is the same baseline-measurement trick used by
/// `super::create2_metering_order::build_pushes_only_contract`.
fn build_sstore_pushes_only_contract() -> Bytes {
    BytecodeBuilder::default()
        .push_u256(U256::from(99u64)) // value
        .push_u256(U256::from(7u64)) // slot
        .append(STOP)
        .build()
}

fn build_sstore_contract() -> Bytes {
    BytecodeBuilder::default()
        .push_u256(U256::from(99u64)) // value
        .push_u256(U256::from(7u64)) // slot
        .append(SSTORE)
        .append(STOP)
        .build()
}

#[test]
fn test_sstore_compute_limit_halt_parity_rex5_rex6() {
    // Place the compute-gas limit just past the pre-SSTORE usage so the limit trips after the
    // storage-gas charge (storage gas doesn't bill compute) and after the inner SSTORE body has
    // run — i.e. at the post-body `record_storage_compute_gas!` call. This is the consensus-
    // critical halt boundary the REX6 metering-order claim rests on: REX5 ↔ REX6 must be
    // byte-identical here, since nothing between `gas_before` and the storage charge consumes
    // EVM gas on the non-CREATE2 opcodes.
    let usage_before_sstore =
        transact_default(MegaSpecId::REX6, base_db(build_sstore_pushes_only_contract()))
            .compute_gas;
    let compute_limit = usage_before_sstore + 100;

    let limits =
        |spec| EvmTxRuntimeLimits::from_spec(spec).with_tx_compute_gas_limit(compute_limit);

    let r5 = transact(MegaSpecId::REX5, base_db(build_sstore_contract()), limits(MegaSpecId::REX5));
    let r6 = transact(MegaSpecId::REX6, base_db(build_sstore_contract()), limits(MegaSpecId::REX6));

    assert!(!r5.is_success(), "REX5 must halt under the tight compute limit; got {:?}", r5.result);
    assert!(!r6.is_success(), "REX6 must halt under the tight compute limit; got {:?}", r6.result);
    assert_eq!(
        format!("{:?}", r5.result),
        format!("{:?}", r6.result),
        "SSTORE halt: result must be identical; REX5={:?} REX6={:?}",
        r5.result,
        r6.result
    );
    assert_eq!(
        r5.compute_gas, r6.compute_gas,
        "SSTORE halt: compute_gas must be identical; REX5={} REX6={}",
        r5.compute_gas, r6.compute_gas
    );
    assert_eq!(
        r5.gas_used, r6.gas_used,
        "SSTORE halt: gas_used must be identical; REX5={} REX6={}",
        r5.gas_used, r6.gas_used
    );
}

#[test]
fn test_callcode_value_transfer_metering_parity_rex5_rex6() {
    // CALLCODE with value transfer goes through the same `new_account_storage_gas` charge path as
    // CALL (via `storage_gas_ext::call_code`). Stack pops gas, to, value, aoff, asz, roff, rsz
    // (gas on top).
    let code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(1u64) // value
        .push_address(EMPTY_TARGET) // to
        .push_number(1_000_000u64) // gas
        .append(CALLCODE)
        .stop()
        .build();
    assert_rex5_rex6_parity("CALLCODE", true, || base_db(code.clone()));
}
