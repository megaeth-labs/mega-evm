//! Tests for oracle contract access detection.
#![allow(clippy::doc_markdown)]

use alloy_primitives::{address, Bytes, TxKind, U256};
use mega_evm::{
    constants::mini_rex::{ORACLE_ACCESS_REMAINING_COMPUTE_GAS, TX_COMPUTE_GAS_LIMIT},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, MegaContext, MegaEvm, MegaHaltReason, MegaHardforkConfig, MegaSpecId,
    MegaTransaction, TestExternalEnvs, ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    bytecode::opcode::{
        CALL, GAS, MSTORE, POP, PUSH0, RETURN, RETURNDATACOPY, RETURNDATASIZE, SLOAD, SSTORE,
        TIMESTAMP,
    },
    context::{result::ExecutionResult, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    Inspector,
};

const CALLER: alloy_primitives::Address = address!("2000000000000000000000000000000000000002");
const CALLEE: alloy_primitives::Address = address!("1000000000000000000000000000000000000001");

/// Helper function to execute a transaction with the given database.
/// Returns a tuple of `(ExecutionResult, MegaEvm, oracle_accessed: bool)`.
fn execute_transaction<
    'a,
    INSP: Inspector<MegaContext<&'a mut MemoryDatabase, &'a TestExternalEnvs<std::convert::Infallible>>>,
>(
    spec: MegaSpecId,
    db: &'a mut MemoryDatabase,
    external_envs: &'a TestExternalEnvs<std::convert::Infallible>,
    inspector: INSP,
    target: alloy_primitives::Address,
) -> (
    ExecutionResult<MegaHaltReason>,
    MegaEvm<&'a mut MemoryDatabase, INSP, &'a TestExternalEnvs<std::convert::Infallible>>,
    bool,
) {
    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(target),
        data: Default::default(),
        value: U256::ZERO,
        gas_limit: 1_000_000_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(inspector);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let result = result_envelope.result;
    // Get oracle_accessed before returning to avoid RefCell borrow conflicts
    let oracle_accessed = evm
        .ctx
        .volatile_data_tracker
        .try_borrow()
        .map(|tracker| tracker.has_accessed_oracle())
        .unwrap_or(false);

    (result, evm, oracle_accessed)
}

/// Checks if the result is a volatile data access out of gas error.
fn is_volatile_data_access_oog(result: &ExecutionResult<MegaHaltReason>) -> bool {
    matches!(
        result,
        &ExecutionResult::Halt { reason: MegaHaltReason::VolatileDataAccessOutOfGas { .. }, .. }
    )
}

/// Test that calling the oracle contract is detected.
#[test]
fn test_oracle_access_detected_on_call() {
    // Create bytecode that calls the oracle contract
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "Compute gas limit should be set to oracle access limit"
    );

    assert!(result.is_success(), "Transaction should succeed");
    assert!(oracle_accessed, "Oracle access should be detected");
}

/// Test that calling a non-oracle contract is not detected as oracle access.
#[test]
fn test_oracle_access_not_detected_on_regular_call() {
    const OTHER_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");

    // Create bytecode that calls a different contract (not oracle)
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(OTHER_CONTRACT) // callee: some other contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);
    assert!(result.is_success(), "Transaction should succeed");
    assert!(!oracle_accessed, "Oracle access should not be detected");
}

/// Test that oracle access is not detected when no CALL is made.
#[test]
fn test_oracle_access_not_detected_without_call() {
    // Simple bytecode that doesn't call anything
    let bytecode = BytecodeBuilder::default().push_number(42u8).stop().build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);
    assert!(result.is_success(), "Transaction should succeed");
    assert!(!oracle_accessed, "Oracle access should not be detected");
}

/// Test that oracle access is NOT detected in EQUIVALENCE spec (uses standard CALL).
/// Oracle detection only works in `MINI_REX` spec with custom CALL instruction.
#[test]
fn test_oracle_access_not_detected_in_equivalence_spec() {
    // Create bytecode that calls the oracle contract
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    // Oracle detection only works in MINI_REX spec, not EQUIVALENCE
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _evm, oracle_accessed) = execute_transaction(
        MegaSpecId::EQUIVALENCE,
        &mut db,
        &external_envs,
        NoOpInspector,
        CALLEE,
    );
    assert!(result.is_success(), "Transaction should succeed");
    assert!(!oracle_accessed, "Oracle access should not be detected in EQUIVALENCE spec");
}

/// Test that oracle access is detected with explicit 0 value parameter.
#[test]
fn test_oracle_access_detected_with_explicit_zero_value() {
    // Create bytecode that calls the oracle contract with explicit 0 wei value
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei (explicit)
        .push_address(ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);
    assert!(result.is_success(), "Transaction should succeed");
    assert!(oracle_accessed, "Oracle access should be detected");
}

/// Test that multiple calls to oracle are still tracked (should not fail).
#[test]
fn test_oracle_access_detected_on_multiple_calls() {
    // Create bytecode that calls the oracle contract twice
    let bytecode = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args again
        .push_address(ORACLE_CONTRACT_ADDRESS) // callee: oracle contract again
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, bytecode);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);
    assert!(result.is_success(), "Transaction should succeed");
    assert!(oracle_accessed, "Oracle access should be detected");
}

/// Test that contract runs out of gas when trying to execute expensive operations after oracle
/// access.
#[test]
fn test_parent_runs_out_of_gas_after_oracle_access() {
    const INTERMEDIATE_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");

    // Create intermediate contract that calls the oracle
    let mut builder = BytecodeBuilder::default();
    builder = builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL);
    // After the call returns, the left gas is limited to 10k
    // Try to execute 1000000 SSTORE operations (each costs 5000 gas minimum)
    // This should run out of gas partway through
    for i in 1..=1000 {
        builder = builder
            .push_number(i as u32) // offset: varying offset to avoid optimization
            .push_number(i as u32) // size: 32 bytes
            .append(SSTORE);
    }
    let intermediate_code = builder.stop().build();

    // Create main contract that:
    // 1. Calls intermediate contract (which accesses oracle)
    // 2. After return, tries to execute many expensive KECCAK256 operations
    // Expected: Parent gas is limited to 10k after oracle access, can't complete all operations
    let mut builder = BytecodeBuilder::default();
    // Call intermediate contract
    builder = builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(INTERMEDIATE_CONTRACT)
        .append(GAS)
        .append(CALL);
    let main_code = builder.append_many([SLOAD, SLOAD, SLOAD, SLOAD, SLOAD, SLOAD]).stop().build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    db.set_account_storage(INTERMEDIATE_CONTRACT, U256::ZERO, U256::from(0x2333u64));
    db.set_account_code(INTERMEDIATE_CONTRACT, intermediate_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");

    // The transaction runs out of gas - the parent frame couldn't complete all expensive operations
    // because its gas was limited to 10k after oracle access
    assert!(!result.is_success(), "Transaction should run out of gas");
    assert!(
        is_volatile_data_access_oog(&result),
        "Transaction should fail due to volatile data access out of gas"
    );
}

/// Test that gas is NOT limited when oracle is not accessed in nested calls.
#[test]
fn test_no_gas_limiting_without_oracle_access() {
    const INTERMEDIATE_CONTRACT: alloy_primitives::Address =
        address!("3000000000000000000000000000000000000003");
    const OTHER_CONTRACT: alloy_primitives::Address =
        address!("4000000000000000000000000000000000000004");

    // Create intermediate contract that calls a non-oracle contract
    let intermediate_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(OTHER_CONTRACT) // NOT the oracle
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    // Create main contract that calls intermediate contract
    let main_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(0u8)
        .push_address(INTERMEDIATE_CONTRACT)
        .append(GAS)
        .append(CALL)
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    db.set_account_code(INTERMEDIATE_CONTRACT, intermediate_code);
    db.set_account_code(OTHER_CONTRACT, Bytes::new()); // Empty contract

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);
    assert!(result.is_success(), "Transaction should succeed");

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        TX_COMPUTE_GAS_LIMIT,
        "Compute gas limit should be the same as the transaction compute gas limit"
    );

    // Verify oracle was NOT accessed
    assert!(!oracle_accessed, "Oracle should not have been accessed");
}

/// Test that when the oracle contract has code, that code is also subject to the gas limit.
#[test]
fn test_oracle_contract_code_subject_to_gas_limit() {
    // Create oracle contract code that tries to execute many expensive operations
    // Since calling the oracle immediately limits gas to 10k, the oracle's own code
    // should run out of gas when trying to execute too many operations
    let mut builder = BytecodeBuilder::default();
    // Try to execute 1000000 SSTORE operations (each costs 100 gas minimum)
    // With only 10k gas limit, this should run out of gas partway through
    for i in 1..=1000 {
        builder = builder
            .push_number(i as u32) // offset: varying offset to avoid optimization
            .push_number(i as u32) // size: 32 bytes
            .append(SSTORE);
    }
    let oracle_code = builder.stop().build();

    // Create main contract that calls the oracle
    let main_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // return memory args
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS)
        .append(CALL)
        .append_many([SLOAD, SLOAD, SLOAD, SLOAD, SLOAD, SLOAD])
        .stop()
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    db.set_account_storage(ORACLE_CONTRACT_ADDRESS, U256::ZERO, U256::from(0x2333u64));
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, _, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");

    // The transaction should run out of gas because the oracle contract's code
    // tries to execute too many expensive operations with only 10k gas available
    assert!(
        !result.is_success(),
        "Transaction should run out of gas due to oracle contract code exceeding gas limit"
    );
    assert!(
        is_volatile_data_access_oog(&result),
        "Transaction should fail due to volatile data access out of gas"
    );
}

/// Test that SLOAD operations on the oracle contract use the OracleEnv to provide storage values.
#[test]
fn test_oracle_storage_sload_uses_oracle_env() {
    // Storage slot and value to test with
    let test_slot = U256::from(42);
    let oracle_value = U256::from(0x1234567890abcdef_u64);

    // Create oracle contract code that performs SLOAD on the test slot and returns the value
    let oracle_code = BytecodeBuilder::default()
        .push_u256(test_slot) // push the slot number
        .append(SLOAD) // load from storage (value is now on stack)
        .push_number(0u8) // push memory offset to stack
        .append(MSTORE) // store value to memory at offset 0
        .push_number(32u8) // push return size to stack
        .push_number(0u8) // push return offset to stack
        .append(RETURN) // return 32 bytes from memory offset 0
        .build();

    // Create main contract that calls the oracle, captures return data, and returns it
    let main_code = BytecodeBuilder::default()
        .push_number(32u8) // retSize for CALL
        .push_number(0u8) // retOffset for CALL
        .push_number(0u8) // argsSize for CALL
        .push_number(0u8) // argsOffset for CALL
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS) // gas
        .append(CALL) // execute the call
        .append(PUSH0) // pop the call result (success/fail)
        // Now return data from oracle call is available via RETURNDATASIZE/RETURNDATACOPY
        .append(RETURNDATASIZE) // get size of return data
        .push_number(0u8) // destOffset in memory
        .push_number(0u8) // offset in returndata
        .append(RETURNDATASIZE) // size to copy
        .append(RETURNDATACOPY) // copy return data to memory
        .append(RETURNDATASIZE) // push size for RETURN
        .push_number(0u8) // push offset for RETURN
        .append(RETURN) // return the data we got from oracle
        .build();

    // Set up the oracle environment with the test storage value
    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new()
        .with_oracle_storage(test_slot, oracle_value);

    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    // Verify the transaction succeeded
    assert!(result.is_success(), "Transaction should succeed");

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");
}

/// Test that SLOAD on oracle contract falls back to database when OracleEnv returns None.
#[test]
fn test_oracle_storage_sload_fallback_to_database() {
    // Storage slot to test with
    let test_slot = U256::from(99);
    let db_value = U256::from(0xfedcba9876543210_u64);

    // Create oracle contract code that performs SLOAD on the test slot and returns the value
    let oracle_code = BytecodeBuilder::default()
        .push_u256(test_slot) // push the slot number
        .append(SLOAD) // load from storage (value is now on stack)
        .push_number(0u8) // push memory offset to stack
        .append(MSTORE) // store value to memory at offset 0
        .push_number(32u8) // push return size to stack
        .push_number(0u8) // push return offset to stack
        .append(RETURN) // return 32 bytes from memory offset 0
        .build();

    // Create main contract that calls the oracle, captures return data, and returns it
    let main_code = BytecodeBuilder::default()
        .push_number(32u8) // retSize for CALL
        .push_number(0u8) // retOffset for CALL
        .push_number(0u8) // argsSize for CALL
        .push_number(0u8) // argsOffset for CALL
        .push_number(0u8) // value: 0 wei
        .push_address(ORACLE_CONTRACT_ADDRESS) // callee: oracle contract
        .append(GAS) // gas
        .append(CALL) // execute the call
        .append(PUSH0) // pop the call result (success/fail)
        // Now return data from oracle call is available via RETURNDATASIZE/RETURNDATACOPY
        .append(RETURNDATASIZE) // get size of return data
        .push_number(0u8) // destOffset in memory
        .push_number(0u8) // offset in returndata
        .append(RETURNDATASIZE) // size to copy
        .append(RETURNDATACOPY) // copy return data to memory
        .append(RETURNDATASIZE) // push size for RETURN
        .push_number(0u8) // push offset for RETURN
        .append(RETURN) // return the data we got from oracle
        .build();

    // Set up database with a storage value for the oracle contract
    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);
    db.set_account_storage(ORACLE_CONTRACT_ADDRESS, test_slot, db_value);

    // Create external envs WITHOUT setting oracle storage (so it returns None)
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    // Verify the transaction succeeded
    assert!(result.is_success(), "Transaction should succeed");

    // Verify oracle was accessed
    assert!(oracle_accessed, "Oracle should have been accessed");
}

/// Test that SLOAD works correctly when transaction directly calls the oracle contract.
/// Oracle access tracking now occurs for both CALL instructions and direct transaction calls.
#[test]
fn test_oracle_storage_sload_direct_call() {
    // Storage slot and value to test with
    let test_slot = U256::from(123);
    let oracle_value = U256::from(0xabcdef1234567890_u64);

    // Create oracle contract code that performs SLOAD on the test slot and returns the value
    let oracle_code = BytecodeBuilder::default()
        .push_u256(test_slot) // push the slot number
        .append(SLOAD) // load from storage (value is now on stack)
        .push_number(0u8) // push memory offset to stack
        .append(MSTORE) // store value to memory at offset 0
        .push_number(32u8) // push return size to stack
        .push_number(0u8) // push return offset to stack
        .append(RETURN) // return 32 bytes from memory offset 0
        .build();

    // Set up the oracle environment with the test storage value
    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new()
        .with_oracle_storage(test_slot, oracle_value);

    // Call the oracle contract DIRECTLY as the transaction target
    let (result, _evm, oracle_accessed) = execute_transaction(
        MegaSpecId::MINI_REX,
        &mut db,
        &external_envs,
        NoOpInspector,
        ORACLE_CONTRACT_ADDRESS,
    );

    // Verify the transaction succeeded
    assert!(result.is_success(), "Transaction should succeed");

    // Verify oracle WAS accessed (oracle access tracking now happens at frame level)
    assert!(oracle_accessed, "Oracle access should be tracked for direct transaction calls");
}

/// Test that the oracle contract is deployed when mini-rex activates via block executor.
/// This exercises the deployment logic in block.rs:284-292.
#[test]
fn test_oracle_contract_deployed_on_mini_rex_activation() {
    use alloy_evm::{block::BlockExecutor, Evm, EvmEnv, EvmFactory};
    use alloy_primitives::B256;
    use mega_evm::{
        MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaSpecId,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, ORACLE_CONTRACT_CODE_HASH,
    };
    use revm::database::State;

    // Create a fresh in-memory database
    let mut db = MemoryDatabase::default();
    let mut state = State::builder().with_database(&mut db).build();

    // Create EVM factory and environment
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);

    // Create EVM environment with MiniRex spec
    use revm::context::BlockEnv;
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = MegaSpecId::MINI_REX;
    let block_env = BlockEnv {
        number: revm::primitives::U256::from(1000), // Set non-zero block number to avoid overflow
        timestamp: revm::primitives::U256::from(1_800_000_000), /* High timestamp to ensure
                                                     * Isthmus is active */
        gas_limit: 30_000_000, // Set reasonable gas limit
        ..Default::default()
    };
    let evm_env = EvmEnv::new(cfg_env, block_env);

    // Create the EVM instance
    let evm = evm_factory.create_evm(&mut state, evm_env);

    // Create block execution context with first_mini_rex_block = true
    // This triggers oracle deployment
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        Some(B256::ZERO), // Set a beacon block root
        Default::default(),
        BlockLimits::no_limits(),
    );

    // Configure hardforks with MiniRex activated at timestamp 0
    use alloy_hardforks::ForkCondition;
    use mega_evm::MegaHardfork;
    let chain_spec =
        MegaHardforkConfig::default().with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0));

    // Create receipt builder (use concrete OpAlloyReceiptBuilder type)
    use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
    let receipt_builder = OpAlloyReceiptBuilder::default();

    // Create block executor
    let mut executor = MegaBlockExecutor::new(evm, block_ctx, chain_spec, receipt_builder);

    // Call apply_pre_execution_changes which triggers oracle deployment in block.rs:284-292
    executor.apply_pre_execution_changes().expect("Pre-execution changes should succeed");

    // The oracle deployment changes should be committed to the database
    // Verify the oracle contract is actually deployed in the database
    let db_ref = executor.evm_mut().db_mut();

    // Load the oracle contract account from the cache
    let cache_acc =
        db_ref.load_cache_account(ORACLE_CONTRACT_ADDRESS).expect("Should be able to load account");
    let acc_info = cache_acc.account_info().expect("Oracle contract account should exist");

    // Verify code hash matches
    assert_eq!(
        acc_info.code_hash, ORACLE_CONTRACT_CODE_HASH,
        "Oracle contract code hash should match"
    );

    // Verify code is set and matches
    assert!(acc_info.code.is_some(), "Code should be set on the account");
    let deployed_code = acc_info.code.as_ref().unwrap();
    assert_eq!(
        deployed_code.original_bytes(),
        ORACLE_CONTRACT_CODE,
        "Deployed code should match original code"
    );

    // Verify that calling deploy_oracle_contract again returns state with account marked as read
    // (proving the contract is already deployed)
    use mega_evm::ensure_oracle_contract_deployed;
    let result = ensure_oracle_contract_deployed(db_ref).expect("Should not error");
    assert_eq!(
        result.len(),
        1,
        "Oracle should already be deployed, so deploy_oracle_contract should return state with account marked as read"
    );
}

/// Test progressive restriction: accessing block env (20M limit) then oracle (1M limit)
/// should further restrict gas to 1M.
#[test]
fn test_progressive_restriction_block_env_then_oracle() {
    // Create a simple oracle contract that returns success
    let oracle_code =
        BytecodeBuilder::default().push_number(0u8).push_number(0u8).append(RETURN).build();

    // Main contract that:
    // 1. Accesses TIMESTAMP (limits gas to 20M)
    // 2. Calls oracle (should further limit to 1M)
    // 3. Records gas before and after
    let main_code = BytecodeBuilder::default()
        // Record gas before TIMESTAMP
        .append(GAS)
        .push_number(0u8)
        .append(MSTORE)
        // Access block env - limits to 20M
        .append(TIMESTAMP)
        .append(POP)
        // Record gas after TIMESTAMP (should be ≤ 20M)
        .append(GAS)
        .push_number(0x20u8)
        .append(MSTORE)
        // Call oracle - should further limit to 1M
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argsSize
        .push_number(0u8) // argsOffset
        .push_number(0u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS) // pass all available gas
        .append(CALL)
        .append(POP)
        // Record gas after oracle call (should be ≤ 1M)
        .append(GAS)
        .push_number(0x40u8)
        .append(MSTORE)
        // Return all recorded gas values
        .push_number(0x60u8)
        .push_number(0u8)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    let (result, evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "Compute gas limit should be set to oracle access limit"
    );

    assert!(result.is_success(), "Transaction should succeed");
    assert!(oracle_accessed, "Oracle should have been accessed");
}

/// Test order independence: accessing oracle (1M limit) then block env (20M limit)
/// should result in the same 1M final limit.
#[test]
fn test_order_independent_oracle_then_block_env() {
    // Create a simple oracle contract that accesses TIMESTAMP before returning
    let oracle_code = BytecodeBuilder::default()
        .append(TIMESTAMP) // Oracle accesses block env (20M limit)
        .append(POP)
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();

    // Main contract that calls oracle first (which will access TIMESTAMP internally)
    let main_code = BytecodeBuilder::default()
        // Call oracle - this will establish 1M limit AND oracle will access TIMESTAMP (20M)
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argsSize
        .push_number(0u8) // argsOffset
        .push_number(0u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS) // pass all available gas
        .append(CALL)
        .append(POP)
        // Record gas after oracle call - should be ≤ 1M (not 20M)
        .append(GAS)
        .push_number(0u8)
        .append(MSTORE)
        // Return recorded gas value
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(CALLEE, main_code);
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let (result, evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        "Compute gas limit should be set to oracle access limit"
    );

    assert!(result.is_success(), "Transaction should succeed");
    assert!(oracle_accessed, "Oracle should have been accessed");
}

#[test]
fn test_oracle_volatile_data_access_oog_does_not_consume_all_gas() {
    // This test verifies that when a transaction runs out of gas due to oracle access
    // (VolatileDataAccessOutOfGas with Oracle type), it does NOT consume all gas.
    // Instead, detained gas is refunded and gas_used reflects only actual work performed.
    let mut db = MemoryDatabase::default();
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    // Contract that calls the oracle then tries expensive work that exceeds the 1M oracle limit
    let mut builder = BytecodeBuilder::default()
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argSize
        .push_number(0u8) // argOffset
        .push_number(0u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS) // oracle address
        .push_number(0xffffu16) // gas
        .append(CALL) // Call oracle - limits gas to 1M
        .append(POP); // pop result

    // Try to do 1000 SSTOREs (2M gas needed, but only 1M available after oracle limiting)
    for i in 1..=1000 {
        builder = builder.push_number(i as u32).push_number(i as u32).append(SSTORE);
    }
    let bytecode = builder.stop().build();
    db.set_account_code(CALLEE, bytecode);

    let (result, _, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    assert!(oracle_accessed, "Oracle should have been accessed");
    // Should fail with VolatileDataAccessOutOfGas
    assert!(!result.is_success(), "Transaction should fail due to oracle volatile data access OOG");
    assert!(
        is_volatile_data_access_oog(&result),
        "Transaction should fail due to volatile data access out of gas"
    );

    let gas_used = result.gas_used();

    // Key assertion: gas_used should be much less than gas_limit
    assert!(
        gas_used < 1_000_000_000,
        "gas_used should be much less than gas_limit, proving detained gas was refunded. Got: {}",
        gas_used
    );
}

#[test]
fn test_both_volatile_data_access_oog_does_not_consume_all_gas() {
    // This test verifies that when BOTH block env and oracle are accessed, and the transaction
    // runs out of gas, the halt reason correctly identifies "Both" type with the most restrictive
    // limit (1M from oracle), and detained gas is properly refunded.

    let mut db = MemoryDatabase::default();
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    // Contract that accesses TIMESTAMP (20M limit), then calls oracle (1M limit),
    // then tries expensive work that exceeds the 1M limit
    let mut builder = BytecodeBuilder::default()
        .append(TIMESTAMP) // Limits gas to 20M
        .append(POP)
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argSize
        .push_number(0u8) // argOffset
        .push_number(0u8) // value
        .push_address(ORACLE_CONTRACT_ADDRESS) // oracle address
        .push_number(0xffffu16) // gas
        .append(CALL) // Call oracle - further limits gas to 1M
        .append(POP); // pop result

    // Try to do 1000 SSTOREs (2M gas needed, but only 1M available)
    for i in 1..=1000 {
        builder = builder.push_number(i as u32).push_number(i as u32).append(SSTORE);
    }
    let bytecode = builder.stop().build();
    db.set_account_code(CALLEE, bytecode);

    let (result, _evm, oracle_accessed) =
        execute_transaction(MegaSpecId::MINI_REX, &mut db, &external_envs, NoOpInspector, CALLEE);

    // Should fail with VolatileDataAccessOutOfGas
    assert!(oracle_accessed, "Oracle should have been accessed");
    assert!(!result.is_success(), "Transaction should fail due to volatile data access OOG");
    assert!(
        is_volatile_data_access_oog(&result),
        "Transaction should fail due to volatile data access out of gas"
    );

    let gas_used = result.gas_used();
    // Key assertion: gas_used should be much less than gas_limit
    assert!(
        gas_used < 1_000_000_000,
        "gas_used should be much less than gas_limit, proving detained gas was refunded. Got: {}",
        gas_used
    );
}

/// Test that transactions from the mega system address are exempted from oracle access tracking.
/// This ensures system operations can call the oracle without gas detention.
#[test]
fn test_mega_system_address_exempted_from_oracle_tracking() {
    use mega_evm::MEGA_SYSTEM_ADDRESS;

    // Create oracle contract code that performs some operations
    let oracle_code =
        BytecodeBuilder::default().push_number(0u8).push_number(0u8).append(RETURN).build();

    let mut db = MemoryDatabase::default();
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, oracle_code);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    // Create a transaction from MEGA_SYSTEM_ADDRESS directly calling the oracle
    let mut context =
        MegaContext::new(&mut db, MegaSpecId::MINI_REX).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: MEGA_SYSTEM_ADDRESS,
        kind: TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        data: Default::default(),
        value: U256::ZERO,
        gas_limit: TX_COMPUTE_GAS_LIMIT + 100_000, // More than compute limit
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let result = result_envelope.result;

    // Get oracle_accessed flag
    let oracle_accessed = evm
        .ctx
        .volatile_data_tracker
        .try_borrow()
        .map(|tracker| tracker.has_accessed_oracle())
        .unwrap_or(false);

    // Verify transaction succeeded
    assert!(result.is_success(), "Transaction from mega system address should succeed");

    // Key assertion: Oracle access should NOT be tracked for mega system address
    assert!(
        !oracle_accessed,
        "Oracle access should NOT be tracked for transactions from MEGA_SYSTEM_ADDRESS"
    );

    // Verify compute gas limit was NOT applied (gas should not be limited to 1M)
    let compute_gas_limit = evm
        .ctx
        .volatile_data_tracker
        .try_borrow()
        .map(|tracker| tracker.get_compute_gas_limit())
        .unwrap_or(None);

    assert!(
        compute_gas_limit.is_none(),
        "Compute gas limit should NOT be set for mega system address transactions"
    );
}
