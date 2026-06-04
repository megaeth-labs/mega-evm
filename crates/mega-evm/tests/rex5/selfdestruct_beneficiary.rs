//! Tests for SELFDESTRUCT beneficiary new-account metering.
//!
//! REX5 adds resource limit tracking for SELFDESTRUCT beneficiary account creation.
//! When a contract selfdestructs and sends its balance to a non-existent address,
//! the resulting new account creation should be metered for state growth, data size,
//! and KV updates.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IMegaAccessControl, LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, VolatileDataAccessType, ACCESS_CONTROL_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ResultAndState, tx::TxEnvBuilder, TxEnv},
    handler::EvmTr,
};

/// The 4-byte selector for `disableVolatileDataAccess()`.
const DISABLE_VOLATILE_DATA_ACCESS_SELECTOR: [u8; 4] =
    IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

/// The 4-byte selector for `VolatileDataAccessDisabled(uint8 accessType)` error.
const VOLATILE_DATA_ACCESS_DISABLED_SELECTOR: [u8; 4] =
    IMegaAccessControl::VolatileDataAccessDisabled::SELECTOR;

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000700000");
const CONTRACT: Address = address!("0000000000000000000000000000000000700001");
const PARENT: Address = address!("0000000000000000000000000000000000700002");
const EMPTY_BENEFICIARY: Address = address!("0000000000000000000000000000000000700099");

// ============================================================================
// HELPERS
// ============================================================================

fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (r, usage)
}

// ============================================================================
// TESTS
// ============================================================================

/// SELFDESTRUCT to an empty beneficiary should record state growth in REX5.
///
/// When a pre-existing contract with balance selfdestructs to a non-existent address,
/// the beneficiary account is created. REX5 meters this as state growth, data size,
/// and KV updates.
#[test]
fn test_rex5_selfdestruct_to_empty_beneficiary_records_state_growth() {
    let code =
        BytecodeBuilder::default().push_address(EMPTY_BENEFICIARY).append(SELFDESTRUCT).build();

    // Contract must have balance for the value transfer to create the beneficiary account.
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(1_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    // REX5 should record state growth for the new beneficiary account.
    assert!(
        usage.state_growth > 0,
        "state growth should include new beneficiary: {}",
        usage.state_growth
    );
    assert!(usage.data_size > 0, "data size should include account write: {}", usage.data_size);
    assert!(
        usage.kv_updates > 0,
        "KV updates should include account creation: {}",
        usage.kv_updates
    );
}

/// SELFDESTRUCT to an empty beneficiary should NOT record additional metering in REX4.
///
/// REX4 does not have the SELFDESTRUCT beneficiary metering hook. The baseline usage
/// from REX4 should be lower than REX5 for the same scenario.
#[test]
fn test_rex4_selfdestruct_to_empty_beneficiary_comparison() {
    let code =
        BytecodeBuilder::default().push_address(EMPTY_BENEFICIARY).append(SELFDESTRUCT).build();

    let build_db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000_000u64))
            .account_code(CONTRACT, code.clone())
            .account_balance(CONTRACT, U256::from(1_000_000u64))
    };

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result_rex5, usage_rex5) = transact(MegaSpecId::REX5, &mut build_db(), tx.clone());
    assert!(result_rex5.result.is_success(), "REX5 should succeed: {result_rex5:?}");

    let (result_rex4, usage_rex4) = transact(MegaSpecId::REX4, &mut build_db(), tx);
    assert!(result_rex4.result.is_success(), "REX4 should succeed: {result_rex4:?}");

    // REX5 should have higher state growth due to beneficiary metering.
    assert!(
        usage_rex5.state_growth > usage_rex4.state_growth,
        "REX5 state growth ({}) should be higher than REX4 ({}) due to beneficiary metering",
        usage_rex5.state_growth,
        usage_rex4.state_growth
    );
}

/// SELFDESTRUCT with zero balance should NOT charge new-account fees.
///
/// When a contract has zero balance and selfdestructs to an empty address,
/// no value transfer occurs, so no new account is created and no extra metering
/// should apply.
#[test]
fn test_rex5_selfdestruct_zero_balance_no_extra_charges() {
    let code =
        BytecodeBuilder::default().push_address(EMPTY_BENEFICIARY).append(SELFDESTRUCT).build();

    // Contract has ZERO balance — no value transfer to beneficiary.
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code);
    // Do NOT set CONTRACT balance — it will be 0.

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    // With zero balance, SELFDESTRUCT does not transfer value, so no new account is created.
    // The fix checks `has_value` — should not trigger for zero-balance selfdestructs.
    // state_growth should not include beneficiary (no value transfer means no account creation).
    assert_eq!(
        usage.state_growth, 0,
        "zero-balance SELFDESTRUCT should not create new account, state_growth: {}",
        usage.state_growth
    );
}

/// SELFDESTRUCT to self (caller == beneficiary) should NOT charge new-account fees.
///
/// The contract targets itself as the beneficiary. Since the contract has code, it is
/// non-empty (`state_clear_aware_is_empty()` returns false), so no new account is created
/// and no new-account storage-gas premium, data size, KV update, or state growth should
/// be recorded for the beneficiary.
#[test]
fn test_rex5_selfdestruct_to_self_no_new_account_charges() {
    // CONTRACT selfdestructs to itself: PUSH20 <CONTRACT> SELFDESTRUCT
    let code = BytecodeBuilder::default().push_address(CONTRACT).append(SELFDESTRUCT).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(1_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    // The beneficiary is the contract itself, which has code and is non-empty.
    // No new-account charges should apply.
    assert_eq!(
        usage.state_growth, 0,
        "SELFDESTRUCT to self should not record new-account state growth: {}",
        usage.state_growth
    );
}

// ============================================================================
// HOISTED-GUARD HELPERS
// ============================================================================

/// Decodes `VolatileDataAccessDisabled(uint8 accessType)` from revert data.
fn decode_volatile_data_access_disabled(
    data: &[u8],
) -> IMegaAccessControl::VolatileDataAccessDisabled {
    <IMegaAccessControl::VolatileDataAccessDisabled as SolError>::abi_decode(data)
        .expect("valid VolatileDataAccessDisabled revert data")
}

/// Builds bytecode that calls `disableVolatileDataAccess()` on the access-control contract.
fn call_disable_volatile_data_access(builder: BytecodeBuilder) -> BytecodeBuilder {
    let builder = builder.mstore(0x0, DISABLE_VOLATILE_DATA_ACCESS_SELECTOR);
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP)
}

/// Builds bytecode that CALLs `target` with `gas`, then copies the child's
/// return data into memory and returns it as the parent's frame output.
fn append_call_and_return_child_data(
    builder: BytecodeBuilder,
    target: Address,
    gas: u64,
) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(target)
        .push_number(gas)
        .append(CALL)
        .append(POP) // discard CALL success flag
        .append(RETURNDATASIZE) // size
        .push_number(0_u64) // dataOffset
        .push_number(0_u64) // destOffset
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE) // size
        .push_number(0_u64) // offset
        .append(RETURN)
}

/// Rex5: SELFDESTRUCT to the block beneficiary inside a disabled-volatile frame
/// must short-circuit ahead of the new-account storage-gas charge and the
/// `on_selfdestruct_new_account` resource-limit record.
///
/// Setup: parent disables volatile access, CALLs child; child's only opcodes are
/// `PUSH20 <beneficiary> ; SELFDESTRUCT`. CONTRACT (child) has balance, so the
/// `is_empty && has_value` branch in `storage_gas_ext::selfdestruct` would fire
/// `on_selfdestruct_new_account` (`state_growth` += 1, `kv_updates` += 1) if the
/// guard didn't short-circuit first. Asserts:
/// 1. Child reverts with `VolatileDataAccessDisabled(Beneficiary)`.
/// 2. `state_growth` stays at 0, proving the new-account hook never ran — the guard fired *before*
///    any of `inspect_account`, the storage-gas charge, or `on_selfdestruct_new_account`.
#[test]
fn test_rex5_selfdestruct_to_beneficiary_with_volatile_disabled_short_circuits() {
    // Block beneficiary defaults to `Address::ZERO` in `BlockEnv::default()`,
    // matching the `MegaContext::new(db, spec)` setup used by `transact()`.
    let beneficiary = Address::ZERO;

    let child_code =
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build();
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call_and_return_child_data(parent_code, CONTRACT, 50_000_000).build();

    // CONTRACT has balance so `has_value` is true under the guard's branch.
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(PARENT, parent_code)
        .account_code(CONTRACT, child_code)
        .account_balance(CONTRACT, U256::from(1_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(
        result.result.is_success(),
        "parent should succeed and return child revert data: {result:?}"
    );

    let output = result.result.output().expect("parent returns child's revert data");
    assert_eq!(
        &output[..4],
        &VOLATILE_DATA_ACCESS_DISABLED_SELECTOR,
        "child should revert with VolatileDataAccessDisabled selector",
    );
    let decoded = decode_volatile_data_access_disabled(output);
    assert_eq!(
        decoded.accessType,
        VolatileDataAccessType::Beneficiary,
        "SELFDESTRUCT to beneficiary with disabled volatile access must revert with Beneficiary",
    );

    // The hoisted guard short-circuits before any new-account resource record.
    // If the storage-gas charge / `on_selfdestruct_new_account` had run,
    // state_growth would be ≥ 1.
    assert_eq!(
        usage.state_growth, 0,
        "guard short-circuit must skip on_selfdestruct_new_account; state_growth: {}",
        usage.state_growth,
    );
}

/// Rex5: SELFDESTRUCT to the block beneficiary when volatile access is NOT disabled.
/// The outer `volatile_data_ext::selfdestruct_with_beneficiary_guard` guard's
/// `target == beneficiary` arm short-circuits the AND on the second operand
/// (`volatile_access_disabled()` returns false), so the guard does NOT fire and the call falls
/// through to the storage layer. With CONTRACT holding balance and the beneficiary empty, the
/// storage layer charges new-account gas and records `on_selfdestruct_new_account`.
///
/// Covers the `target == beneficiary && !volatile_disabled` branch of the macro-
/// generated outer wrapper, which is the partial branch the volatile-disabled
/// test alone cannot exercise.
#[test]
fn test_rex5_selfdestruct_to_beneficiary_without_volatile_disabled() {
    // Block beneficiary defaults to `Address::ZERO` in `BlockEnv::default()`,
    // matching the `MegaContext::new(db, spec)` setup used by `transact()`.
    let beneficiary = Address::ZERO;

    let code = BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build();

    // CONTRACT has balance so the value transfer to the empty beneficiary
    // triggers the new-account path inside `storage_gas_ext::selfdestruct`.
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000u64))
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(1_000_000u64));

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(CONTRACT).gas_limit(1_000_000).build_fill();

    let (result, usage) = transact(MegaSpecId::REX5, &mut db, tx);
    assert!(result.result.is_success(), "should succeed: {result:?}");

    // Guard did not fire — control reached the storage layer, which charged
    // new-account gas and recorded `on_selfdestruct_new_account`.
    assert!(
        usage.state_growth > 0,
        "fall-through to storage layer must record new-account growth: {}",
        usage.state_growth,
    );
}
