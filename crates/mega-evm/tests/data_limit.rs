//! Tests for the data limit feature of the `MegaETH` EVM.
//!
//! Tests the data limit functionality that prevents spam attacks by limiting the amount
//! of data generated during transaction execution.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::set_account_balance, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, NoOpOracle, TransactionError,
};
use revm::{
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        TxEnv,
    },
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    inspector::NoOpInspector,
};

/// Executes a transaction on the `MegaETH` EVM with configurable data limits.
///
/// Returns the execution result, generated data size, and number of key-value updates.
fn transact(
    spec: MegaSpecId,
    db: &mut CacheDB<EmptyDB>,
    caller: Address,
    callee: Option<Address>,
    data: Bytes,
    value: U256,
    data_limit: u64,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, TransactionError>> {
    let mut context = MegaContext::new(db, spec, NoOpOracle).with_data_limit(data_limit);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context, NoOpInspector);
    let tx = TxEnv {
        caller,
        kind: callee.map_or(TxKind::Create, TxKind::Call),
        data,
        value,
        gas_limit: 1000000000000000000,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx)?;

    let ctx = evm.ctx_ref();
    Ok((r, ctx.generated_data_size(), ctx.kv_update_count()))
}

/// Checks if the execution result indicates that the data limit was exceeded.
#[allow(unused)]
fn is_data_limit_exceeded(result: ResultAndState<MegaHaltReason>) -> bool {
    match result.result {
        ExecutionResult::Halt { reason, .. } => reason == MegaHaltReason::DataLimitExceeded,
        _ => false,
    }
}

/// Checks if the execution result indicates that the KV update limit was exceeded.
#[allow(unused)]
fn is_kv_update_limit_exceeded(result: ResultAndState<MegaHaltReason>) -> bool {
    match result.result {
        ExecutionResult::Halt { reason, .. } => reason == MegaHaltReason::KVUpdateLimitExceeded,
        _ => false,
    }
}

// #[test]
// fn test_data_limit_exceeded() {
//     let mut db = CacheDB::<EmptyDB>::default();
//     let contract_address = address!("0000000000000000000000000000000000100001");
//     let code: Bytes = hex!("620002005fa000").into();
//     set_account_code(&mut db, contract_address, code);

//     let caller = address!("0000000000000000000000000000000000100000");
//     let callee = Some(contract_address);
//     let (res, _data_size, _kv_update_count) =
//         transact(MegaSpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO,
// 600)             .unwrap();
//     assert!(res.result.is_halt());
// }

/// Test the data size and kv update count for empty transaction execution.
#[test]
fn test_empty_tx() {
    let mut db = CacheDB::<EmptyDB>::default();
    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(address!("0000000000000000000000000000000000100001"));
    let (res, data_size, kv_update_count) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        caller,
        callee,
        Bytes::default(),
        U256::ZERO,
        u64::MAX,
    )
    .unwrap();
    assert!(!res.result.is_halt());
    // 1 kv update for the caller account (nonce increase)
    assert_eq!(kv_update_count, 1);
    // 110 bytes for the intrinsic data of a transaction + 312 bytes for the caller account info
    // update
    assert_eq!(data_size, 110 + 312);
}

/// Test ether transfer between existing accounts and verify data size/KV update counts.
///
/// This test verifies that a simple ether transfer from one existing account to another
/// works correctly and generates the expected amount of data and key-value updates.
/// It sets up two accounts with initial balances and transfers 1 wei from caller to callee.
#[test]
fn test_ether_transfer_to_existing_account() {
    let mut db = CacheDB::<EmptyDB>::default();
    let caller = address!("0000000000000000000000000000000000100000");
    let callee = address!("0000000000000000000000000000000000100001");
    set_account_balance(&mut db, caller, U256::from(1000));
    set_account_balance(&mut db, callee, U256::from(100));
    let (res, data_size, kv_update_count) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        caller,
        Some(callee),
        Bytes::default(),
        U256::from(1),
        u64::MAX,
    )
    .unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (nonce increase), and one for the callee account (balance
    // increase)
    assert_eq!(kv_update_count, 2);
    // 110 bytes for the intrinsic data of a transaction, 2*312 for two account info updates (caller
    // and callee)
    assert_eq!(data_size, 110 + 2 * 312);
}

/// Test ether transfer to a non-existing account and verify data size/KV update counts.
///
/// This test verifies that transferring ether to an account that doesn't exist yet
/// works correctly and generates the expected amount of data and key-value updates.
/// It creates a new account for the callee during the transfer operation.
#[test]
fn test_ether_transfer_to_non_existing_account() {
    let mut db = CacheDB::<EmptyDB>::default();
    let caller = address!("0000000000000000000000000000000000100000");
    let callee = address!("0000000000000000000000000000000000100001");
    set_account_balance(&mut db, caller, U256::from(1000));
    let (res, data_size, kv_update_count) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        caller,
        Some(callee),
        Bytes::default(),
        U256::from(1),
        u64::MAX,
    )
    .unwrap();
    assert!(res.result.is_success());
    // 2 kv updates for the caller and callee account info updates
    assert_eq!(kv_update_count, 2);
    // 110 bytes for the intrinsic data of a transaction, 2*312 for two account info updates (caller
    assert_eq!(data_size, 110 + 2 * 312);
}

/// Test calling a non-existing account with zero value and verify data size/KV update counts.
///
/// This test verifies that making a call to an account that doesn't exist yet
/// works correctly and generates the expected amount of data and key-value updates.
/// Unlike ether transfers, this call doesn't create the account since no value is transferred.
#[test]
fn test_call_non_existing_account() {
    let mut db = CacheDB::<EmptyDB>::default();
    let caller = address!("0000000000000000000000000000000000100000");
    let callee = address!("0000000000000000000000000000000000100001");
    set_account_balance(&mut db, caller, U256::from(1000));
    let (res, data_size, kv_update_count) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        caller,
        Some(callee),
        Bytes::default(),
        U256::ZERO,
        u64::MAX,
    )
    .unwrap();
    assert!(res.result.is_success());
    // 2 kv updates for the caller and callee account info updates
    assert_eq!(kv_update_count, 1);
    // 110 bytes for the intrinsic data of a transaction, 312 for the caller account info update
    assert_eq!(data_size, 110 + 312);
}
