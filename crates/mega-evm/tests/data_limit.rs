//! Tests for the data limit feature of the `MegaETH` EVM.
//!
//! Tests the data limit functionality that prevents spam attacks by limiting the amount
//! of data generated during transaction execution.

use std::convert::Infallible;

use alloy_eips::{
    eip2930::{AccessList, AccessListItem},
    eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization},
};
use alloy_primitives::{address, bytes, hex, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    test_utils::{
        opcode_gen::BytecodeBuilder, set_account_balance, set_account_code, MemoryDatabase,
        TraceInspector,
    },
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionBuilder,
    MegaTransactionError, NoOpOracle,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, GAS, INVALID, PUSH0, RETURN, SLOAD, SSTORE, STOP},
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
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
    data_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, MegaTransactionError>>
{
    let mut context = MegaContext::new(db, spec, NoOpOracle).with_data_limit(data_limit);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context).with_inspector(TraceInspector::default());
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

const FACTORY: Address = address!("0000000000000000000000000000000000200001");
const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const LIBRARY: Address = address!("0000000000000000000000000000000000100002");

/// The factory code of a contract that dumps a log.
///
/// The code:
/// ```yul
/// {
///     // Read first uint256 (number of topics) from calldata offset 0
///      let numTopics := calldataload(0)
///          
///      // Read second uint256 (length of log data) from calldata offset 32
///      let dataLength := calldataload(0x20)
///  
///      switch numTopics
///      case 0 {
///          // LOG0: log(offset, length)
///          log0(0x0, dataLength)
///      }
///      case 1 {
///          log1(0x0, dataLength, 0x0)
///      }
///      case 2 {
///          log2(0x0, dataLength, 0x0, 0x0)
///      }
///      case 3 {
///          log3(0x0, dataLength, 0x0, 0x0, 0x0)
///      }
///      case 4 {
///          log4(0x0, dataLength, 0x0, 0x0, 0x0, 0x0)
///      }
///      default {
///          invalid()
///      }
///  
///      stop()
///  }
/// ```
const LOG_FACTORY_CODE: Bytes = bytes!("5f3560203590805f146050578060011460475780600214603d5780600314603257600414602857fe5b5f8080809381a45b005b505f80809281a36030565b505f809181a26030565b505f9081a16030565b505fa0603056");

/// The factory code of a contract that creates a contract.
///
/// The code:
/// ```yul
/// {
///     // the last 32 bytes is uint256 argument
///     codecopy(0x0, sub(codesize(), 0x20), 0x20)
///     let codeLen := mload(0x0)
///     // the created contract code is returned
///     return(0x0, codeLen)
/// }
/// ```
///
/// There is one required argument, the contract size, which is a uint256 and should be appended by
/// the end of the creation code.
const CONTRACT_CONSTRUCTOR_CODE: Bytes = bytes!("60208038035f395f515ff3");

/// The factory code of a contract that creates a contract.
///
/// The code:
/// ```yul
/// {
///     // The contract constructor
///     let constructorLen := 11
///     let constructorCode := 0x60208038035f395f515ff3
///     mstore(0x0, constructorCode)
///     let constructorCodeStart := sub(0x20, constructorLen)
///
///     // The first 32 bytes of calldata is codeLen
///     let codeLen := calldataload(0x0)
///     // Append codeLen to the end of constructor
///     mstore(0x20, codeLen)
///
///     let created := create(0x0, constructorCodeStart, add(constructorLen, 0x20))
///     if iszero(created) {
///         invalid()
///     }
/// }
/// ```
///
/// There is one required argument, the contract size, which is a uint256 and should be appended by
/// the end of the creation code.
const CONTRACT_FACTORY_CODE: Bytes =
    bytes!("600b6a60208038035f395f515ff35f526020818103915f35825201905ff015602357005bfe");

/// Generates the input for the contract factory contract.
fn gen_contract_factory_input(contract_size: u64) -> Bytes {
    let mut input = vec![];
    input.extend_from_slice(&U256::from(contract_size).to_be_bytes_vec());
    input.into()
}

/// Generates the input for the contract create transaction. It uses the constructor code as the
/// input and append the contract size at the end.
fn gen_contract_create_tx_input(contract_size: u64) -> Bytes {
    let mut input = CONTRACT_CONSTRUCTOR_CODE.to_vec();
    input.extend_from_slice(&U256::from(contract_size).to_be_bytes_vec());
    input.into()
}

/// Generates the input for the log factory contract.
fn gen_log_factory_input(num_topics: u64, data_length: u64) -> Bytes {
    let mut input = vec![];
    input.extend_from_slice(&U256::from(num_topics).to_be_bytes_vec());
    input.extend_from_slice(&U256::from(data_length).to_be_bytes_vec());
    input.into()
}

/// Test the data size and kv update count for empty transaction execution.
#[test]
fn test_empty_tx() {
    let mut db = CacheDB::<EmptyDB>::default();
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
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
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1000))
        .account_balance(CALLEE, U256::from(100));
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).value(U256::from(1)).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
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
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).value(U256::from(1)).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
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
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 2 kv updates for the caller and callee account info updates
    assert_eq!(kv_update_count, 1);
    // 110 bytes for the intrinsic data of a transaction, 312 for the caller account info update
    assert_eq!(data_size, 110 + 312);
}

/// Test call with data, access list, and EIP-7702 authorization list.
///
/// This test verifies that a call transaction with additional data components
/// (call data, access list, and EIP-7702 authorization list) works correctly
/// and generates the expected amount of data and key-value updates. It tests
/// the data limit functionality with more complex transaction structures.
#[test]
fn test_call_with_data() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    let delegate = address!("0000000000000000000000000000000000100002");
    let data = bytes!("01020304"); // 4 bytes of call data
    let mut access_list = AccessList::default();
    access_list.0.push(AccessListItem { address: CALLER, storage_keys: vec![B256::ZERO] });
    let authorization_list = vec![RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::from(1), address: delegate, nonce: 0 },
        RecoveredAuthority::Valid(CALLER),
    )];
    let tx = TxEnvBuilder::new()
        .caller(CALLER)
        .call(CALLEE)
        .data(data)
        .access_list(access_list)
        .authorization_list_recovered(authorization_list)
        .build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 2 kv update for the caller account (tx nonce increase and 7702 code change)
    assert_eq!(kv_update_count, 2);
    // 110 bytes for the intrinsic data of a transaction, 2*312 for the caller account info update
    // and 7702 code change, 4 bytes for the call data, 52 bytes for the access list, 101 bytes
    // for the authorization list
    assert_eq!(data_size, 110 + 2 * 312 + 4 + 52 + 101);
}

/// Test contract creation and verify data size/KV update counts.
///
/// This test verifies that creating a new contract works correctly and generates
/// the expected amount of data and key-value updates. It creates a contract with
/// 10 bytes of code and verifies the data size includes the transaction data,
/// account updates, and the created contract code.
#[test]
fn test_create_contract() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    let input = gen_contract_create_tx_input(10);
    let input_len = input.len() as u64;
    let tx = TxEnvBuilder::new().caller(CALLER).create().data(input).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase), 1 kv update for the created account
    assert_eq!(kv_update_count, 2);
    // 110 bytes for the intrinsic data of a transaction, 2*312 bytes for the caller and created
    // account info update, 10 bytes for the created contract code, bytes for the
    // input data
    assert_eq!(data_size, 110 + 2 * 312 + 10 + input_len);
}

/// Test contract creation through a factory contract and verify data size/KV update counts.
///
/// This test verifies that creating a contract through a factory contract works correctly
/// and generates the expected amount of data and key-value updates. It uses a factory
/// contract that creates another contract with 10 bytes of code, testing the data limit
/// functionality in a more complex contract creation scenario.
#[test]
fn test_create_contract_with_factory() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    set_account_code(&mut db, FACTORY, CONTRACT_FACTORY_CODE);
    let input = gen_contract_factory_input(10);
    let input_len = input.len() as u64;
    let tx = TxEnvBuilder::new().caller(CALLER).call(FACTORY).data(input).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase), 1 kv update for the created account
    assert_eq!(kv_update_count, 2);
    // 110 bytes for the intrinsic data of a transaction, 2*312 bytes for the caller and created
    // account info update, 10 bytes for the created contract code, bytes for the
    // input data
    assert_eq!(data_size, 110 + 2 * 312 + 10 + input_len);
}

/// Test log data generation and verify data size/KV update counts.
///
/// This test verifies that generating log data works correctly and generates
/// the expected amount of data and key-value updates. It uses a log factory
/// contract that creates logs with 1 topic and 10 bytes of data, testing
/// the data limit functionality for log operations.
#[test]
fn test_log_data() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    set_account_code(&mut db, CALLEE, LOG_FACTORY_CODE);
    let input = gen_log_factory_input(1, 10);
    let input_len = input.len() as u64;
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).data(input).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_update_count, 1);
    // 110 bytes for the intrinsic data of a transaction, 312 bytes for the caller account info
    // update, 32 + 10 bytes for the log topics and data, bytes for the input data
    assert_eq!(data_size, 110 + 312 + 32 + 10 + input_len);
}

/// Test storage write operations and verify data size/KV update counts.
///
/// This test verifies that storage write operations (SSTORE) work correctly and generate
/// the expected amount of data and key-value updates. It uses a simple contract that
/// stores 0x0 to storage slot 0, testing the data limit functionality for storage
/// operations.
#[test]
fn test_sstore_data() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    // a simple contract that stores 0x0 to slot 0
    let code: Bytes = BytecodeBuilder::default().append_many([PUSH0, PUSH0, SSTORE, STOP]).build();
    set_account_code(&mut db, CALLEE, code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase), 1 kv update for the callee storage
    assert_eq!(kv_update_count, 2);
    // 110 bytes for the intrinsic data of a transaction, 312 bytes for the caller account info
    // update, 296 bytes for one storage slot write
    assert_eq!(data_size, 110 + 312 + 296);
}

/// Test storage read operations and verify data size/KV update counts.
///
/// This test verifies that storage read operations (SLOAD) work correctly and generate
/// the expected amount of data and key-value updates. It uses a simple contract that
/// loads from storage slot 0, testing the data limit functionality for storage
/// read operations.
#[test]
fn test_sload_data() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    // a simple contract that loads slot 0
    let code: Bytes = BytecodeBuilder::default().append_many([PUSH0, SLOAD, STOP]).build();
    set_account_code(&mut db, CALLEE, code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_update_count, 1);
    // 110 bytes for the intrinsic data of a transaction, 312 bytes for the caller account info
    // update, 212 bytes for one storage slot read
    assert_eq!(data_size, 110 + 312 + 212);
}

/// Test that data size tracking correctly handles reverted nested calls.
///
/// This test verifies that when a nested call fails (reverts), the data size
/// tracking still correctly accounts for storage reads that occurred before the
/// revert. It uses a contract that calls a library contract, where the library
/// performs storage operations and then reverts, ensuring that storage reads
/// are still included in the witness data even when the call fails.
#[test]
fn test_nested_call_data_size_are_reverted_on_failure() {
    let mut db = MemoryDatabase::default();
    // a simple contract that calls a library contract
    let contract_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]) // value, argOffset, argLen, returnOffset, returnLen
        .push_address(LIBRARY) // callee address
        .append(GAS) // gas to forward
        .append(CALL)
        .build();
    db.set_account_code(CALLEE, contract_code);
    // a library that sload and sstore and then revert
    let library_code =
        BytecodeBuilder::default().append_many([PUSH0, PUSH0, SLOAD, SSTORE, INVALID]).build();
    db.set_account_code(LIBRARY, library_code);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    // although the nested call is reverted, the outer call still succeeds
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_update_count, 1);
    // 110 bytes for the intrinsic data of a transaction, 312 bytes for the caller account info
    // update (nonce), 212 bytes for the library storage read (even if revert, the storage read
    // still needs to be included in witness)
    assert_eq!(data_size, 110 + 312);
}

#[test]
fn test_nested_creation_revert() {
    let mut db = MemoryDatabase::default();
    // a contract constructor code that always revert
    let constructor_code = BytecodeBuilder::default().revert().build();
    let constructor_code_len = constructor_code.len() as u64;
    // a simple contract that creates a contract
    let contract_code = BytecodeBuilder::default()
        .mstore(0x0, constructor_code)
        .push_number(constructor_code_len) // init code len
        .push_number(0x0_u64) // init code offset in memory
        .push_number(0x0_u64) // value to transfer
        .append(CREATE)
        .build();
    db.set_account_code(CALLEE, contract_code);

    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, tx).unwrap();
    // the outer call should succeed, even though the nested creation reverts
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_update_count, 1);
    // 110 bytes for the intrinsic data of a transaction, 312 bytes for the caller account info
    // update
    assert_eq!(data_size, 110 + 312);
}

// TODO: tests for data/kv update limit and revert handling in nested calls
// TODO: tests for data/kv update limit is disable in EQUIVALENCE Spec
// TODO: tests for doubled gas cost for sstore and call when bucket capacity doubles
