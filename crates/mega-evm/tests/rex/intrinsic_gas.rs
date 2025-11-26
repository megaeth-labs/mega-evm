//! Tests for Rex hardfork additional intrinsic gas costs.

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

/// Tests that Rex hardfork adds 39,000 gas to base transaction cost for simple transfer.
#[test]
fn test_rex_intrinsic_gas_simple_transfer() {
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
    // Rex: 21,000 (base) + 39,000 (Rex intrinsic storage gas) = 60,000
    assert_eq!(gas_used, BASE_INTRINSIC_GAS + constants::rex::TX_INTRINSIC_STORAGE_GAS);
}

/// Tests that `MiniRex` does NOT charge the additional 39,000 intrinsic gas.
#[test]
fn test_mini_rex_no_additional_intrinsic_gas() {
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
    // `MiniRex`: only 21,000 (base) - no additional intrinsic gas
    assert_eq!(gas_used, BASE_INTRINSIC_GAS);
}

/// Tests that `Equivalence` spec does NOT charge the additional 39,000 intrinsic gas.
#[test]
fn test_equivalence_no_additional_intrinsic_gas() {
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
    // `Equivalence`: only 21,000 (base) - no additional intrinsic gas
    assert_eq!(gas_used, BASE_INTRINSIC_GAS);
}

/// Tests `Rex` intrinsic gas with calldata (combines calldata costs + intrinsic storage gas).
#[test]
fn test_rex_intrinsic_gas_with_calldata() {
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

    // `Rex` inherits `MiniRex` calldata costs and adds 39,000 intrinsic storage gas
    // For 100 bytes of non-zero calldata:
    // - MiniRex base cost: 38,600 (from floor gas test calculations)
    // - Rex intrinsic storage gas: 39,000
    // Total: 77,600
    let expected_gas = 77_600;
    assert_eq!(gas_used, expected_gas);
}

/// Tests that Rex transaction fails if gas limit is below intrinsic gas requirement.
#[test]
fn test_rex_intrinsic_gas_insufficient_gas_limit() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(1_000_000));
    db.set_account_balance(CALLEE, U256::from(100));

    // Set gas limit to just below the Rex intrinsic gas (60,000)
    let insufficient_gas_limit = BASE_INTRINSIC_GAS + constants::rex::TX_INTRINSIC_STORAGE_GAS - 1;

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

/// Tests that Rex intrinsic gas applies to contract creation transactions.
#[test]
fn test_rex_intrinsic_gas_contract_creation() {
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

    // Should include the Rex intrinsic storage gas + new account storage gas + other costs
    // At minimum, it should be more than base + Rex intrinsic + new account storage gas
    assert!(
        gas_used >=
            BASE_INTRINSIC_GAS +
                constants::rex::TX_INTRINSIC_STORAGE_GAS +
                constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS
    );
}

/// Tests that `Rex` intrinsic gas is exactly 39,000 more than `MiniRex` for same transaction.
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
    // the Rex intrinsic storage gas (39,000)
    assert_eq!(gas_used_rex - gas_used_mini_rex, constants::rex::TX_INTRINSIC_STORAGE_GAS);
}

/// Tests that Rex intrinsic gas applies even for zero-value transfers.
#[test]
fn test_rex_intrinsic_gas_zero_value_transfer() {
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
    // Rex: 21,000 (base) + 39,000 (Rex intrinsic storage gas) = 60,000
    assert_eq!(gas_used, BASE_INTRINSIC_GAS + constants::rex::TX_INTRINSIC_STORAGE_GAS);
}

/// Tests Rex intrinsic gas with new account creation (combines new account gas + intrinsic gas).
#[test]
fn test_rex_intrinsic_gas_new_account_creation() {
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
    // - Rex intrinsic storage gas: 39,000
    // - New account storage gas: 2,000,000
    // Total: 2,060,000
    let expected_gas = BASE_INTRINSIC_GAS +
        constants::rex::TX_INTRINSIC_STORAGE_GAS +
        constants::mini_rex::NEW_ACCOUNT_STORAGE_GAS;
    assert_eq!(gas_used, expected_gas);
}
