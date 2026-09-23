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
    EvmTxRuntimeLimits, IMegaAccessControl, LimitUsage, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction, VolatileDataAccessType, ACCESS_CONTROL_ADDRESS,
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

// ============================================================================
// SELFDESTRUCT attempted inside a STATICCALL frame
// ============================================================================

/// A parent that STATICCALLs `target` once (forwarding `gas_each`), discards the
/// result, and STOPs successfully.
fn staticcall_once_parent(target: Address, gas_each: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_address(target)
        .push_number(gas_each) // forwarded gas (top of stack)
        .append(STATICCALL)
        .append(POP)
        .append(STOP)
        .build()
}

/// SELFDESTRUCT whose beneficiary creation exceeds the state-growth budget must fail at
/// the SELFDESTRUCT itself via the trailing all-dimension check: the usage is recorded
/// before the inner instruction runs, but only latched once the inner instruction has
/// succeeded.
#[test]
fn test_rex5_selfdestruct_beneficiary_creation_fails_on_state_growth_limit() {
    let sd_code =
        BytecodeBuilder::default().push_address(EMPTY_BENEFICIARY).append(SELFDESTRUCT).build();

    let run = |growth_limit: u64| {
        let mut db = MemoryDatabase::default()
            .account_code(CONTRACT, sd_code.clone())
            .account_balance(CONTRACT, U256::from(1_000u64))
            .account_balance(CALLER, U256::from(10).pow(U256::from(18)));
        let mut context = MegaContext::new(&mut db, MegaSpecId::REX5).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(growth_limit),
        );
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::from(0));
            chain.operator_fee_constant = Some(U256::from(0));
        });
        let mut evm = MegaEvm::new(context);
        let tx = TxEnvBuilder::default()
            .caller(CALLER)
            .call(CONTRACT)
            .gas_limit(10_000_000)
            .build_fill();
        let mut tx = MegaTransaction::new(tx);
        tx.enveloped_tx = Some(Bytes::new());
        let r = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
        let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
        (r, usage)
    };

    // Control: a budget of 1 admits the single new beneficiary account.
    let (ok_res, ok_usage) = run(1);
    assert!(ok_res.result.is_success(), "control should succeed: {:?}", ok_res.result);
    assert_eq!(ok_usage.state_growth, 1, "beneficiary creation should count as growth");

    // Budget 0: the beneficiary creation trips the limit at the SELFDESTRUCT.
    let (res, _) = run(0);
    assert!(
        !res.result.is_success(),
        "zero state-growth budget must fail the SELFDESTRUCT, got {:?}",
        res.result,
    );
}
