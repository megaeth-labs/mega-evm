//! Tests for Rex hardfork additional floor storage gas costs.

use std::convert::Infallible;

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    constants, test_utils::MemoryDatabase, DefaultExternalEnvs, EVMError, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionError,
};
use revm::{
    context::{result::ResultAndState, TxEnv},
    primitives::Address,
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CALLEE: Address = address!("1000000000000000000000000000000000000001");

/// Base intrinsic gas cost for all transactions
const BASE_INTRINSIC_GAS: u64 = 21_000;

/// Executes a transaction on the EVM.
#[allow(clippy::too_many_arguments)]
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: &DefaultExternalEnvs,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
    gas_limit: u64,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, spec, external_envs);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// Tests that Rex hardfork adds 160,000 floor storage gas to base transaction cost for simple
/// transfer.
#[test]
fn test_rex_floor_gas_simple_transfer() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000));
    db.set_account_balance(CALLEE, U256::from(100));

    let res = transact(
        MegaSpecId::REX,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::from(1),
        1_000_000,
    )
    .unwrap();

    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    // Rex: 21,000 (base) + 160,000 (Rex floor storage gas) = 181,000
    assert_eq!(gas_used, BASE_INTRINSIC_GAS + constants::rex::TX_FLOOR_STORAGE_GAS);
}

/// Tests that `MiniRex` does NOT charge the additional 160,000 floor storage gas.
#[test]
fn test_mini_rex_no_additional_floor_gas() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000));
    db.set_account_balance(CALLEE, U256::from(100));

    let res = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::from(1),
        1_000_000,
    )
    .unwrap();

    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    // `MiniRex`: only 21,000 (base) - no additional floor storage gas
    assert_eq!(gas_used, BASE_INTRINSIC_GAS);
}

/// Tests that `Equivalence` spec does NOT charge the additional 160,000 floor storage gas.
#[test]
fn test_equivalence_no_additional_floor_gas() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000));
    db.set_account_balance(CALLEE, U256::from(100));

    let res = transact(
        MegaSpecId::EQUIVALENCE,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::from(1),
        1_000_000,
    )
    .unwrap();

    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    // `Equivalence`: only 21,000 (base) - no additional floor storage gas
    assert_eq!(gas_used, BASE_INTRINSIC_GAS);
}

/// Tests `Rex` floor storage gas with calldata (combines calldata costs + floor storage gas).
#[test]
fn test_rex_floor_gas_with_calldata() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000));
    db.set_account_balance(CALLEE, U256::from(100));

    // Create calldata with 100 bytes (all non-zero for maximum gas cost)
    let calldata = Bytes::from(vec![1u8; 100]);

    let res = transact(
        MegaSpecId::REX,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        calldata,
        U256::from(1),
        1_000_000,
    )
    .unwrap();

    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();

    // `Rex` inherits `MiniRex` calldata costs and adds 160,000 floor storage gas
    // For 100 bytes of non-zero calldata:
    // - Base intrinsic: 21,000
    // - Calldata (100 non-zero bytes): 100 * 16 = 1,600 (compute gas)
    // - MiniRex calldata storage: 100 * 400 = 40,000
    // - Floor gas enforcement: max(initial_gas, floor_gas)
    //   - initial_gas = 21,000 + 1,600 + 40,000 = 62,600
    //   - floor_gas = base_floor + mini_rex_floor + rex_floor = 21,000 + (100 * 48) + (100 * 480) +
    //     160,000 = 224,800
    // Total: 225,000 (floor gas is higher, so it's used)
    let expected_gas = 225_000;
    assert_eq!(gas_used, expected_gas);
}

/// Tests that Rex transaction uses all gas when gas limit is below floor gas requirement.
#[test]
fn test_rex_floor_gas_insufficient_gas_limit() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000));
    db.set_account_balance(CALLEE, U256::from(100));

    // Set gas limit to just below the Rex floor gas (181,000)
    let insufficient_gas_limit = BASE_INTRINSIC_GAS + constants::rex::TX_FLOOR_STORAGE_GAS - 1;

    let res = transact(
        MegaSpecId::REX,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::from(1),
        insufficient_gas_limit,
    );

    // Transaction should fail due to insufficient gas
    assert!(res.is_err());
}

/// Tests that Rex floor storage gas applies to contract creation transactions.
#[test]
fn test_rex_floor_gas_contract_creation() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(10_000_000));

    // Simple contract bytecode that does nothing (PUSH0 PUSH0 RETURN)
    let init_code = Bytes::from(vec![0x5f, 0x5f, 0xf3]);

    let res = transact(
        MegaSpecId::REX,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        None, // None means CREATE
        init_code,
        U256::ZERO,
        10_000_000,
    )
    .unwrap();

    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();

    // Should include the Rex floor storage gas + new account storage gas + other costs
    // The floor gas doesn't add to creation cost when initial gas is higher
    // At minimum, it should include base + new account storage gas + code deposit costs
    assert!(gas_used >= BASE_INTRINSIC_GAS + constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS);
}

/// Tests that `Rex` floor storage gas is exactly 160,000 more than `MiniRex` for same transaction.
#[test]
fn test_rex_vs_mini_rex_gas_difference() {
    let mut db_rex = MemoryDatabase::default();
    db_rex.set_account_balance(CALLER, U256::from(1_000_000));
    db_rex.set_account_balance(CALLEE, U256::from(100));

    let mut db_mini_rex = MemoryDatabase::default();
    db_mini_rex.set_account_balance(CALLER, U256::from(1_000_000));
    db_mini_rex.set_account_balance(CALLEE, U256::from(100));

    // Use empty calldata for simplest comparison
    let calldata = Bytes::new();

    let res_rex = transact(
        MegaSpecId::REX,
        &mut db_rex,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        calldata.clone(),
        U256::from(1),
        1_000_000,
    )
    .unwrap();

    let res_mini_rex = transact(
        MegaSpecId::MINI_REX,
        &mut db_mini_rex,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        calldata,
        U256::from(1),
        1_000_000,
    )
    .unwrap();

    assert!(res_rex.result.is_success());
    assert!(res_mini_rex.result.is_success());

    let gas_used_rex = res_rex.result.gas_used();
    let gas_used_mini_rex = res_mini_rex.result.gas_used();

    // For transactions without calldata, the difference should be exactly
    // the Rex floor storage gas (160,000)
    assert_eq!(gas_used_rex - gas_used_mini_rex, constants::rex::TX_FLOOR_STORAGE_GAS);
}

/// Tests that Rex floor storage gas applies even for zero-value transfers.
#[test]
fn test_rex_floor_gas_zero_value_transfer() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000));
    db.set_account_balance(CALLEE, U256::from(100));

    let res = transact(
        MegaSpecId::REX,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::ZERO, // Zero value transfer
        1_000_000,
    )
    .unwrap();

    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();
    // Rex: 21,000 (base) + 160,000 (Rex floor storage gas) = 181,000
    assert_eq!(gas_used, BASE_INTRINSIC_GAS + constants::rex::TX_FLOOR_STORAGE_GAS);
}

/// Tests Rex floor storage gas with new account creation (combines new account gas + floor storage
/// gas).
#[test]
fn test_rex_floor_gas_new_account_creation() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(10_000_000));
    // CALLEE doesn't exist in db (new account)

    let res = transact(
        MegaSpecId::REX,
        &mut db,
        &DefaultExternalEnvs::new(),
        CALLER,
        Some(CALLEE),
        Bytes::new(),
        U256::from(1), // Non-zero value to trigger new account gas
        10_000_000,
    )
    .unwrap();

    assert!(res.result.is_success());
    let gas_used = res.result.gas_used();

    // Rex charges:
    // - Base: 21,000
    // - New account storage gas: 2,000,000
    // - Floor gas enforcement: max(initial_gas, floor_gas)
    //   - initial_gas = 21,000 + 2,000,000 = 2,021,000
    //   - floor_gas = 21,000 + 160,000 = 181,000
    // Total: 2,021,000 (initial gas is higher, so it's used)
    let expected_gas = BASE_INTRINSIC_GAS + constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS;
    assert_eq!(gas_used, expected_gas);
}
