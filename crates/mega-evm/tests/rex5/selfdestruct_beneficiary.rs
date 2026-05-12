//! Tests for SELFDESTRUCT beneficiary new-account metering.
//!
//! REX5 adds resource limit tracking for SELFDESTRUCT beneficiary account creation.
//! When a contract selfdestructs and sends its balance to a non-existent address,
//! the resulting new account creation should be metered for state growth, data size,
//! and KV updates.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ResultAndState, tx::TxEnvBuilder, TxEnv},
    handler::EvmTr,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000700000");
const CONTRACT: Address = address!("0000000000000000000000000000000000700001");
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
