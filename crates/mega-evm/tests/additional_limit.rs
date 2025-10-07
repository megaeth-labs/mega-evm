//! Tests for the data limit feature of the `MegaETH` EVM.
//!
//! Tests the data limit functionality that prevents spam attacks by limiting the amount
//! of data generated during transaction execution.

use std::convert::Infallible;

use alloy_eips::{
    eip2930::{AccessList, AccessListItem},
    eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization},
};
use alloy_primitives::{address, bytes, Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::{opcode_gen::BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError, ACCOUNT_INFO_WRITE_SIZE, BASE_TX_SIZE, STORAGE_SLOT_WRITE_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CREATE, DELEGATECALL, GAS, INVALID, PUSH0, PUSH1, SLOAD, SSTORE, STOP,
    },
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        ContextTr, TxEnv,
    },
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    DatabaseCommit,
};

/// Executes a transaction on the `MegaETH` EVM with configurable data limits.
///
/// Returns the execution result, generated data size, and number of key-value updates.
fn transact(
    spec: MegaSpecId,
    db: &mut CacheDB<EmptyDB>,
    data_limit: u64,
    kv_update_limit: u64,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, MegaTransactionError>>
{
    let mut context = MegaContext::new(db, spec, DefaultExternalEnvs::default())
        .with_data_limit(data_limit)
        .with_kv_update_limit(kv_update_limit);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx)?;

    let ctx = evm.ctx_ref();
    Ok((r, ctx.generated_data_size(), ctx.kv_update_count()))
}

/// Checks if the execution result indicates that the data limit was exceeded.
#[allow(unused)]
fn is_data_limit_exceeded(result: &ResultAndState<MegaHaltReason>) -> bool {
    match &result.result {
        ExecutionResult::Halt { reason, .. } => {
            matches!(reason, MegaHaltReason::DataLimitExceeded { .. })
        }
        _ => false,
    }
}

/// Checks if the execution result indicates that the KV update limit was exceeded.
#[allow(unused)]
fn is_kv_update_limit_exceeded(result: &ResultAndState<MegaHaltReason>) -> bool {
    match &result.result {
        ExecutionResult::Halt { reason, .. } => {
            matches!(reason, MegaHaltReason::KVUpdateLimitExceeded { .. })
        }
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

// ============================================================================
// BASIC TRANSACTION TESTS
// ============================================================================

/// Test data size and KV update count for empty transaction execution.
///
/// This test verifies the baseline data size and KV update counts for the simplest
/// possible transaction - a call to an existing account with no value or data.
/// It establishes the minimum data size (base transaction + sender account write)
/// and KV update count (sender nonce increment) for all transactions.
#[test]
fn test_empty_tx() {
    let mut db = CacheDB::<EmptyDB>::default();
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(!res.result.is_halt());
    // 1 kv update for the caller account (nonce increase)
    assert_eq!(kv_update_count, 1);
    // 110 bytes for the intrinsic data of a transaction + 312 bytes for the caller account info
    // update
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        +ACCOUNT_INFO_WRITE_SIZE // sender write
    );
}

/// Test calling a non-existing account with zero value and verify data size/KV update counts.
///
/// This test verifies that making a call to an account that doesn't exist yet
/// works correctly and generates the expected amount of data and key-value updates.
/// Unlike ether transfers, this call doesn't create the account since no value is transferred,
/// resulting in fewer KV updates and data size compared to value transfers.
#[test]
fn test_call_non_existing_account() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv updates for the caller info update
    assert_eq!(kv_update_count, 1);
    // base tx + sender account info read + sender account info write
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
    );
}

/// Test call with data, access list, and EIP-7702 authorization list.
///
/// This test verifies that a call transaction with additional data components
/// (call data, access list, and EIP-7702 authorization list) works correctly
/// and generates the expected amount of data and key-value updates. It tests
/// the data limit functionality with more complex transaction structures,
/// including EIP-7702 delegation which requires additional account updates.
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
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 2 kv update for the caller account (tx nonce increase and 7702 code change)
    assert_eq!(kv_update_count, 2);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        + 101 + ACCOUNT_INFO_WRITE_SIZE // 7702 authorization & account read & write
        + 4 + 52 // call data & access list
    );
}

// ============================================================================
// ETHER TRANSFER TESTS
// ============================================================================

/// Test ether transfer between existing accounts and verify data size/KV update counts.
///
/// This test verifies that a simple ether transfer from one existing account to another
/// works correctly and generates the expected amount of data and key-value updates.
/// It sets up two accounts with initial balances and transfers 1 wei from caller to callee.
/// The test verifies that both accounts are updated (sender nonce, receiver balance)
/// and that the data size includes both account writes.
#[test]
fn test_ether_transfer_to_existing_account() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1000))
        .account_balance(CALLEE, U256::from(100));
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).value(U256::from(1)).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (nonce increase), and one for the callee account (balance
    // increase)
    assert_eq!(kv_update_count, 2);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        + ACCOUNT_INFO_WRITE_SIZE // callee write
    );
}

/// Test ether transfer to a non-existing account and verify data size/KV update counts.
///
/// This test verifies that transferring ether to an account that doesn't exist yet
/// works correctly and generates the expected amount of data and key-value updates.
/// It creates a new account for the callee during the transfer operation, which
/// results in the same data size and KV update counts as transferring to an existing
/// account since both require updating the receiver's balance.
#[test]
fn test_ether_transfer_to_non_existing_account() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).value(U256::from(1)).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 2 kv updates for the caller and callee account info updates
    assert_eq!(kv_update_count, 2);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        + ACCOUNT_INFO_WRITE_SIZE // callee write
    );
}

// ============================================================================
// CONTRACT CREATION TESTS
// ============================================================================

/// Test contract creation and verify data size/KV update counts.
///
/// This test verifies that creating a new contract works correctly and generates
/// the expected amount of data and key-value updates. It creates a contract with
/// 10 bytes of code and verifies the data size includes the transaction data,
/// account updates, and the created contract code. The test ensures that both
/// the sender and the newly created contract account are properly tracked.
#[test]
fn test_create_contract() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    let input = gen_contract_create_tx_input(10);
    let input_len = input.len() as u64;
    let tx = TxEnvBuilder::new().caller(CALLER).create().data(input).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase), 1 kv update for the created account
    assert_eq!(kv_update_count, 2);
    // 110 bytes for the intrinsic data of a transaction, 2*312 bytes for the caller and created
    // account info update, 10 bytes for the created contract code, bytes for the
    // input data
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx 
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        + ACCOUNT_INFO_WRITE_SIZE // callee write
        + 10 // created contract code
        + input_len // input data
    );
}

/// Test contract creation through a factory contract and verify data size/KV update counts.
///
/// This test verifies that creating a contract through a factory contract works correctly
/// and generates the expected amount of data and key-value updates. It uses a factory
/// contract that creates another contract with 10 bytes of code, testing the data limit
/// functionality in a more complex contract creation scenario. The test ensures that
/// all three accounts (sender, factory, created contract) are properly tracked.
#[test]
fn test_create_contract_with_factory() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    db.set_account_code(FACTORY, CONTRACT_FACTORY_CODE);
    let input = gen_contract_factory_input(10);
    let input_len = input.len() as u64;
    let tx = TxEnvBuilder::new().caller(CALLER).call(FACTORY).data(input).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase), 1 kv update for the created account
    assert_eq!(kv_update_count, 3);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        + ACCOUNT_INFO_WRITE_SIZE // factory nonce change
        + ACCOUNT_INFO_WRITE_SIZE // created contract write
        + 10 // contract code
        + input_len // input data
    );
}

// ============================================================================
// STORAGE OPERATION TESTS
// ============================================================================

/// Test storage write operations and verify data size/KV update counts.
///
/// This test verifies that storage write operations (SSTORE) work correctly and generate
/// the expected amount of data and key-value updates. It uses a simple contract that
/// stores 0x1 to storage slot 0, testing the data limit functionality for storage
/// operations. Storage writes create both data size and KV update counts.
#[test]
fn test_sstore_data() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    // a simple contract that stores 0x0 to slot 0
    let code: Bytes =
        BytecodeBuilder::default().append_many([PUSH1, 0x1u8, PUSH0, SSTORE, STOP]).build();
    db.set_account_code(CALLEE, code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase), 1 kv update for the callee storage
    assert_eq!(kv_update_count, 2);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        + STORAGE_SLOT_WRITE_SIZE // storage write
    );
}

/// Test storage read operations and verify data size/KV update counts.
///
/// This test verifies that storage read operations (SLOAD) work correctly and generate
/// the expected amount of data and key-value updates. It uses a simple contract that
/// loads from storage slot 0, testing the data limit functionality for storage
/// read operations. Storage reads contribute to data size but don't create KV updates
/// since they don't modify state.
#[test]
fn test_sload_data() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    // a simple contract that loads slot 0
    let code: Bytes = BytecodeBuilder::default().append_many([PUSH0, SLOAD, STOP]).build();
    db.set_account_code(CALLEE, code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_update_count, 1);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
    );
}

// ============================================================================
// LOG OPERATION TESTS
// ============================================================================

/// Test log data generation and verify data size/KV update counts.
///
/// This test verifies that generating log data works correctly and generates
/// the expected amount of data and key-value updates. It uses a log factory
/// contract that creates logs with 1 topic and 10 bytes of data, testing
/// the data limit functionality for log operations. Log data contributes to
/// the total data size but doesn't create additional KV updates.
#[test]
fn test_log_data() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(1000));
    db.set_account_code(CALLEE, LOG_FACTORY_CODE);
    let input = gen_log_factory_input(1, 10);
    let input_len = input.len() as u64;
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).data(input).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_update_count, 1);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        + 32 + 10 // log topics and data
        + input_len // input data
    );
}

// ============================================================================
// CALL OPERATION TESTS
// ============================================================================

/// Test delegated call operations and verify data size/KV update counts.
///
/// This test verifies that delegated call operations work correctly and generate
/// the expected amount of data and key-value updates. It uses a contract that
/// performs a delegated call to a library contract, testing the data limit
/// functionality for delegated calls. Delegated calls don't create additional
/// account updates since they execute in the caller's context.
#[test]
fn test_delegated_call() {
    let mut db = MemoryDatabase::default();
    // a contract that calls a library contract
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // argOffset, argLen, returnOffset, returnLen
        .push_address(LIBRARY) // callee address
        .append(GAS) // gas to forward
        .append(DELEGATECALL)
        .build();
    db.set_account_code(CALLEE, code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_updates) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_updates, 1);
    assert_eq!(data_size, BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE); // no update on either contract
                                                                   // or library
}

/// Test that caller account updates are not double-counted in nested transfers.
///
/// This test verifies that when a contract performs a nested call with value transfer,
/// the caller account is not double-counted for data size and KV update purposes.
/// It uses a contract that transfers value to a library contract, ensuring that
/// account updates are properly deduplicated across call boundaries.
#[test]
fn test_updated_caller_should_not_be_double_counted_for_nested_transfer() {
    let mut db = MemoryDatabase::default();
    // a contract that transfers value to a library contract
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // argOffset, argLen, returnOffset, returnLen
        .push_number(1u8) // value to transfer
        .push_address(LIBRARY) // callee address
        .append(GAS) // gas to forward
        .append(CALL)
        .build();
    db.set_account_code(CALLEE, code);
    db.set_account_balance(CALLEE, U256::from(10000));
    db.set_account_balance(CALLER, U256::from(10000));
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).value(U256::from(100)).build_fill();
    let (res, data_size, kv_updates) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase), 1 kv update for the callee account
    // (balance increase and decrease), 1 kv update for the library account (balance increase)
    // (balance decrease)
    assert_eq!(kv_updates, 3);
    assert_eq!(
        data_size,
        BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE // base tx + sender write
        + ACCOUNT_INFO_WRITE_SIZE // callee write (count only once)
        + ACCOUNT_INFO_WRITE_SIZE // library write
    );
}

// ============================================================================
// NESTED CALL TESTS
// ============================================================================

/// Test that data size tracking correctly handles reverted nested calls.
///
/// This test verifies that when a nested call fails (reverts), the data size
/// tracking still correctly accounts for storage reads that occurred before the
/// revert. It uses a contract that calls a library contract, where the library
/// performs storage operations and then reverts, ensuring that storage reads
/// are still included in the witness data even when the call fails. The test
/// demonstrates that read operations are tracked even in failed calls.
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
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    // although the nested call is reverted, the outer call still succeeds
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_update_count, 1);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
    );
}

/// Test that data size tracking correctly handles reverted nested contract creation.
///
/// This test verifies that when a nested contract creation fails (reverts), the data size
/// tracking still works correctly. It uses a contract that attempts to create another
/// contract with a constructor that always reverts, ensuring that the outer call succeeds
/// but the nested creation fails without affecting the data size calculations.
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
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    // the outer call should succeed, even though the nested creation reverts
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_update_count, 1);
    assert_eq!(
        data_size,
        BASE_TX_SIZE // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
    );
}

// ============================================================================
// SPEC COMPARISON TESTS
// ============================================================================

/// Test that data size and KV update counts are not measured in EQUIVALENCE spec.
///
/// This test verifies that when using the EQUIVALENCE spec, the data size and
/// KV update counting functionality is disabled. This ensures that the EQUIVALENCE
/// spec behaves like standard Ethereum without the additional data limit tracking
/// that is present in the `MINI_REX` spec.
#[test]
fn test_data_size_and_kv_update_count_are_not_measured_in_equivalence_spec() {
    let mut db = MemoryDatabase::default();
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_update_count) =
        transact(MegaSpecId::EQUIVALENCE, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    assert_eq!(kv_update_count, 0);
    assert_eq!(data_size, 0);
}

// ============================================================================
// LIMIT ENFORCEMENT TESTS
// ============================================================================

/// Test that data limit enforcement works correctly when the limit is not exceeded.
///
/// This test verifies that transactions succeed when the generated data size is exactly
/// at the data limit threshold. It uses a simple call transaction that generates
/// the minimum data size (base transaction + sender account write) and sets the data
/// limit to that exact amount, ensuring the transaction completes successfully.
#[test]
fn test_data_limit_just_not_exceed() {
    let mut db = MemoryDatabase::default();
    // the data size is 110 bytes for the intrinsic data of a transaction, 312 bytes for the caller
    // account info update
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, _, _) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        BASE_TX_SIZE  // base tx
        + ACCOUNT_INFO_WRITE_SIZE, // sender write
        u64::MAX,
        tx,
    )
    .unwrap();
    assert!(res.result.is_success());
}

/// Test that data limit enforcement correctly halts transactions when the limit is exceeded.
///
/// This test verifies that transactions are halted when the generated data size exceeds
/// the data limit threshold. It uses a simple call transaction that generates
/// the minimum data size but sets the data limit to one byte less, ensuring
/// the transaction is halted with a `DataLimitExceeded` reason.
#[test]
fn test_data_limit_just_exceed() {
    let mut db = MemoryDatabase::default();
    // the data size is 110 bytes for the intrinsic data of a transaction, 312 bytes for the caller
    // account info update
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, _, _) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        BASE_TX_SIZE  // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        -1, // minus one byte data size
        u64::MAX,
        tx,
    )
    .unwrap();
    assert!(res.result.is_halt());
    assert!(is_data_limit_exceeded(&res));
}

/// Test that KV update limit enforcement works correctly when the limit is not exceeded.
///
/// This test verifies that transactions succeed when the number of key-value updates
/// is exactly at the KV update limit threshold. It uses an ether transfer transaction
/// that generates exactly 2 KV updates (caller and callee account updates) and sets
/// the KV update limit to 2, ensuring the transaction completes successfully.
#[test]
fn test_kv_update_limit_just_not_exceed() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(100));
    // 2 kv updates for the caller and callee account info updates
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).value(U256::from(1)).build_fill();
    let (res, _, _) = transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, 2, tx).unwrap();
    assert!(res.result.is_success());
}

/// Test that KV update limit enforcement correctly halts transactions when the limit is exceeded.
///
/// This test verifies that transactions are halted when the number of key-value updates
/// exceeds the KV update limit threshold. It uses an ether transfer transaction that
/// generates 2 KV updates (caller and callee account updates) but sets the KV update
/// limit to 1, ensuring the transaction is halted with a `KVUpdateLimitExceeded` reason.
#[test]
fn test_kv_update_limit_just_exceed() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(100));
    // 2 kv updates for the caller and callee account info updates
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).value(U256::from(1)).build_fill();
    let (res, _, _) = transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, 2 - 1, tx).unwrap();
    assert!(res.result.is_halt());
    assert!(is_kv_update_limit_exceeded(&res));
}

/// Test that data limit enforcement correctly halts transactions in nested calls.
///
/// This test verifies that when a nested call would exceed the data limit, the transaction
/// is properly halted with a `DataLimitExceeded` reason. It uses a contract that calls a
/// library contract, where the library performs storage operations that would exceed the
/// data limit, ensuring that the limit enforcement works correctly across call boundaries.
/// The test demonstrates that limits are enforced even in complex call scenarios.
#[test]
fn test_data_limit_exceed_in_nested_call() {
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
        BytecodeBuilder::default().append_many([PUSH1, 0x1u8, PUSH0, SLOAD, SSTORE, STOP]).build();
    db.set_account_code(LIBRARY, library_code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, _, _) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        BASE_TX_SIZE  // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        + 1, // one additional data size
        u64::MAX,
        tx,
    )
    .unwrap();
    assert!(res.result.is_halt());
    assert!(is_data_limit_exceeded(&res));
}

/// Test that KV update limit enforcement correctly halts transactions in nested calls.
///
/// This test verifies that when a nested call would exceed the KV update limit, the transaction
/// is properly halted with a `KVUpdateLimitExceeded` reason. It uses a contract that calls a
/// library contract with value transfer, where the combined KV updates from the root call,
/// nested call, and library operations exceed the limit, ensuring that the limit enforcement
/// works correctly across call boundaries. The test demonstrates that KV update limits
/// are enforced even in complex nested call scenarios.
#[test]
fn test_kv_update_limit_exceed_in_nested_call() {
    let mut db = MemoryDatabase::default();
    // a simple contract that sloads and then call a library
    let contract_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // argOffset, argLen, returnOffset, returnLen
        .push_number(1u8) // call value
        .push_address(LIBRARY) // callee address
        .append(GAS) // gas to forward
        .append(CALL)
        .build();
    db.set_account_code(CALLEE, contract_code);
    db.set_account_balance(CALLEE, U256::from(10000));
    // a library that sstore and then revert
    let library_code =
        BytecodeBuilder::default().append_many([PUSH1, 0x1u8, PUSH0, SSTORE, INVALID]).build();
    db.set_account_code(LIBRARY, library_code);
    // The tx makes 1 kv update in the root call for the caller account info update, 1 kv update in
    // the nested call for value transfer, and 1 kv update in the library for storage write. We set
    // the KV update limit to 2, so that the transaction is halted in the nested call with a
    // `KVUpdateLimitExceeded` reason.
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, _, _) = transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, 3 - 1, tx).unwrap();
    assert!(res.result.is_halt());
    assert!(is_kv_update_limit_exceeded(&res));
}

// ============================================================================
// STORAGE DEDUPLICATION TESTS
// ============================================================================

/// Test that writing zero to a storage slot should not be counted for data size/KV updates.
///
/// This test verifies that writing zero to a storage slot (which effectively deletes
/// the slot) is not counted towards data size or KV update limits. This is important
/// for gas optimization and ensures that storage cleanup operations don't consume
/// unnecessary resources. The test writes 0x0 to slot 0 and verifies that only
/// the base transaction and sender account update are counted.
#[test]
fn test_writing_zero_to_slot_should_not_be_counted() {
    let mut db = MemoryDatabase::default();
    // a contract writing 0x0 to slot 0
    let code: Bytes = BytecodeBuilder::default().append_many([PUSH0, PUSH0, SSTORE, STOP]).build();
    db.set_account_code(CALLEE, code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_updates) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    assert_eq!(kv_updates, 1);
    assert_eq!(data_size, BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE); // no storage slot write
}

/// Test that writing twice to the same storage slot should be counted only once.
///
/// This test verifies that multiple writes to the same storage slot within a single
/// transaction are deduplicated and counted only once for data size and KV update
/// purposes. This prevents abuse where contracts could write to the same slot
/// multiple times to artificially inflate their data usage. The test writes 0x1
/// then 0x2 to slot 0 and verifies that only one storage write is counted.
#[test]
fn test_writing_twice_to_same_slot_should_be_counted_once() {
    let mut db = MemoryDatabase::default();
    // a contract writing 0x1 to slot 0 and then 0x2 to slot 0
    let code: Bytes = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1))
        .sstore(U256::from(0), U256::from(2))
        .build();
    db.set_account_code(CALLEE, code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_updates) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase), 1 kv update for the storage slot
    // write
    assert_eq!(kv_updates, 2);
    assert_eq!(
        data_size,
        BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE // base tx + sender write
         + STORAGE_SLOT_WRITE_SIZE // only one storage slot write
    );
}

/// Test that eventually no change to a storage slot should not be counted.
///
/// This test verifies that when a storage slot is modified and then reset to its
/// original value within the same transaction, the net effect is no change and
/// therefore no data size or KV update should be counted. This prevents contracts
/// from artificially inflating their resource usage by performing operations that
/// ultimately cancel out. The test writes 0x1 then 0x0 to slot 0 and verifies
/// that no storage write is counted since the final state is unchanged.
#[test]
fn test_eventually_no_change_to_slot_should_not_be_counted() {
    let mut db = MemoryDatabase::default();
    // a contract writing 0x1 to slot 0 and then reset slot 0 to 0x0
    let code: Bytes = BytecodeBuilder::default()
        .sstore(U256::from(0), U256::from(1))
        .sstore(U256::from(0), U256::from(0))
        .build();
    db.set_account_code(CALLEE, code);
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, kv_updates) =
        transact(MegaSpecId::MINI_REX, &mut db, u64::MAX, u64::MAX, tx).unwrap();
    assert!(res.result.is_success());
    // 1 kv update for the caller account (tx nonce increase)
    assert_eq!(kv_updates, 1);
    assert_eq!(data_size, BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE); // no storage slot write
}

// ============================================================================
// STATE REVERT TESTS
// ============================================================================

/// Test that state is properly reverted when data limit is exceeded.
///
/// This test verifies that when a transaction exceeds the data limit, the entire
/// state is properly reverted to its original state before the transaction.
/// It uses a contract that performs storage operations and value transfers,
/// ensuring that all changes are rolled back when the limit is exceeded.
#[test]
fn test_state_revert_when_exceeding_limit() {
    let mut db = MemoryDatabase::default();
    // a contract that writes 0x1 to slot 0
    let code = BytecodeBuilder::default().sstore(U256::from(0), U256::from(1)).build();
    db.set_account_code(CALLEE, code);
    db.set_account_balance(CALLER, U256::from(10000));
    // the tx also transfers value to the callee
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).value(U256::from(100)).build_fill();
    let (res, data_size, kv_updates) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE // base tx + sender write
         + 1, // one additional data size
        u64::MAX,
        tx,
    )
    .unwrap();
    // the tx should be halted with a `DataLimitExceeded` reason, and the state should be reverted
    assert!(res.result.is_halt());
    assert!(is_data_limit_exceeded(&res));
    assert_eq!(kv_updates, 1); // only 1 kv update for the caller account (tx nonce increase)
                               // base tx + sender write, no update on the contract
    assert_eq!(data_size, BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE);
    // the contract should not be changed (touched)
    assert!(res.state.get(&CALLEE).is_none_or(|contract| !contract.is_touched()));
    // the caller balance should not be changed (tx reverts)
    assert!(res.state.get(&CALLER).is_some_and(|caller| caller.info.balance == U256::from(10000)));
}

// ============================================================================
// MULTIPLE TRANSACTION TESTS
// ============================================================================

/// Test that data limit and KV update limits reset for each transaction.
///
/// This test verifies that when executing multiple transactions sequentially with the same
/// context (like when building a block), the data size and KV update counters properly
/// reset for each new transaction instead of accumulating across transactions.
#[test]
fn test_limits_reset_across_multiple_transactions() {
    let mut db = MemoryDatabase::default().account_balance(CALLER, U256::from(10000));

    // Create context once and reuse it for multiple transactions
    let mut context =
        MegaContext::new(&mut db, MegaSpecId::MINI_REX, DefaultExternalEnvs::default())
            .with_data_limit(u64::MAX)
            .with_kv_update_limit(u64::MAX);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    context.modify_cfg(|cfg| {
        cfg.disable_nonce_check = true;
    });
    let mut evm = MegaEvm::new(context);

    // Execute first transaction
    let tx1 = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let mut mega_tx1 = MegaTransaction::new(tx1);
    mega_tx1.enveloped_tx = Some(Bytes::new());
    let res1 = alloy_evm::Evm::transact_raw(&mut evm, mega_tx1).unwrap();
    let data_size_after_tx1 = evm.ctx_ref().generated_data_size();
    let kv_updates_after_tx1 = evm.ctx_ref().kv_update_count();

    // Verify first transaction generated some data and KV updates
    assert_eq!(data_size_after_tx1, BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE);
    assert_eq!(kv_updates_after_tx1, 1);

    // Commit the first transaction's state changes
    evm.ctx_mut().db_mut().commit(res1.state);

    // Execute second transaction with same context
    let tx2 = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let mut mega_tx2 = MegaTransaction::new(tx2);
    mega_tx2.enveloped_tx = Some(Bytes::new());
    let _ = alloy_evm::Evm::transact_raw(&mut evm, mega_tx2).unwrap();
    let data_size_after_tx2 = evm.ctx_ref().generated_data_size();
    let kv_updates_after_tx2 = evm.ctx_ref().kv_update_count();

    // Verify counters reset and show only second transaction's data
    // Without reset, these would be 2x the single transaction values
    assert_eq!(data_size_after_tx2, BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE);
    assert_eq!(kv_updates_after_tx2, 1);
}
