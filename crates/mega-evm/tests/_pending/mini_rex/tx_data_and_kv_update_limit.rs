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
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
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
    interpreter::{CallInputs, CallOutcome},
    DatabaseCommit, Inspector,
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
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(data_limit)
            .with_tx_kv_updates_limit(kv_update_limit),
    );
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
// SPEC COMPARISON TESTS
// ============================================================================

// ============================================================================
// LIMIT ENFORCEMENT TESTS
// ============================================================================

/// Test that data limit enforcement correctly halts transactions when the limit is exceeded.
///
/// This test verifies that transactions are halted when the generated data size exceeds
/// the data limit threshold. It uses a simple call transaction that generates
/// the minimum data size but sets the data limit to one byte less, ensuring
/// the transaction is halted with a `DataLimitExceeded` reason.
#[test]
fn test_data_limit_just_exceed() {
    let mut db = MemoryDatabase::default();
    // the data size is 110 bytes for the intrinsic data of a transaction, 40 bytes for the caller
    // account info update
    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).build_fill();
    let (res, data_size, _) = transact(
        MegaSpecId::MINI_REX,
        &mut db,
        BASE_TX_SIZE  // base tx
        + ACCOUNT_INFO_WRITE_SIZE // sender write
        -1, // minus one byte data size
        u64::MAX,
        tx,
    )
    .expect("should succeed with halt");

    // Should halt (not error) with DataLimitExceeded
    assert!(matches!(
        res.result,
        ExecutionResult::Halt { reason: MegaHaltReason::DataLimitExceeded { .. }, .. }
    ));

    // Verify the data size tracked
    assert_eq!(data_size, BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE);
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

// ============================================================================
// STORAGE DEDUPLICATION TESTS
// ============================================================================

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
// GAS PRESERVATION TESTS
// ============================================================================

// ============================================================================
// TRACKER MIGRATION COVERAGE TESTS
// ============================================================================

/// Tests the `check_limit` priority order: `data_size` is checked before `kv_update`.
/// When both limits are exceeded simultaneously, the `data_size` error should be reported.
#[test]
fn test_check_limit_priority_data_size_before_kv_update() {
    // Simple code that STOPs immediately — the intrinsic TX data already exceeds limits
    let code = BytecodeBuilder::default().append(STOP).build();

    let mut db = CacheDB::new(EmptyDB::new());
    db.insert_account_info(
        CALLER,
        revm::state::AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
    );
    db.insert_account_info(
        CALLEE,
        revm::state::AccountInfo {
            code: Some(revm::bytecode::Bytecode::new_raw(code)),
            ..Default::default()
        },
    );

    let tx = TxEnvBuilder::new().caller(CALLER).call(CALLEE).gas_limit(100_000_000).build_fill();

    // Set both limits very low so both are exceeded by TX intrinsics:
    // data_size will be 150 (110 base + 40 caller), exceeds limit=1
    // kv_updates will be 1 (caller), exceeds limit=0
    let (result, _, _) = transact(MegaSpecId::MINI_REX, &mut db, 1, 0, tx).unwrap();

    assert!(result.result.is_halt(), "Expected halt, got {:?}", result.result);

    // data_size is checked before kv_update in the check_limit() order,
    // so DataLimitExceeded should be reported
    assert!(
        is_data_limit_exceeded(&result),
        "Expected DataLimitExceeded (checked first), got {:?}",
        result.result
    );
}

/// An inspector that mimics `TracerEip3155`'s `GasInspector` behavior:
/// calls `Gas::spend_all()` on error results in `call_end`.
///
/// This reproduces the scenario where an inspector zeros `gas.remaining()`
/// which could interfere with `rescue_gas` if called in the wrong order.
struct GasSpendingInspector;

impl<CTX: ContextTr> Inspector<CTX> for GasSpendingInspector {
    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        if !outcome.result.result.is_ok() {
            outcome.result.gas.spend_all();
        }
    }
}
