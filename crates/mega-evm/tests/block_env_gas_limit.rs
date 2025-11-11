//! Tests for gas limiting after block environment access.
//!
//! These tests verify that accessing block environment data (TIMESTAMP, NUMBER, etc.)
//! immediately limits remaining gas to prevent `DoS` attacks.
//!
//! Key properties tested:
//! 1. Block env opcodes trigger gas limiting (`gas_used` should be small)
//! 2. Detained gas is restored before tx finishes (users only pay for real work)
//! 3. Gas limiting propagates through nested calls
//! 4. Without block env access, no limiting occurs (`gas_used` reflects full work)

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    constants::mini_rex::{BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS, TX_COMPUTE_GAS_LIMIT},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ExecutionResult, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    Inspector,
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");
const NESTED_CONTRACT: Address = address!("1000000000000000000000000000000000000003");

/// Helper to create and execute a transaction with given bytecode
fn execute_bytecode(
    db: &mut MemoryDatabase,
    gas_limit: u64,
) -> (
    ExecutionResult<MegaHaltReason>,
    MegaEvm<&mut MemoryDatabase, NoOpInspector, DefaultExternalEnvs>,
) {
    execute_bytecode_with_inspector::<NoOpInspector>(db, gas_limit, NoOpInspector)
}

/// Helper to create and execute a transaction with given bytecode and gas inspector
/// Helper to create and execute a transaction with given bytecode and gas price, committing state
fn execute_bytecode_with_inspector<
    'a,
    INSP: Inspector<MegaContext<&'a mut MemoryDatabase, DefaultExternalEnvs>>,
>(
    db: &'a mut MemoryDatabase,
    gas_limit: u64,
    inspector: INSP,
) -> (ExecutionResult<MegaHaltReason>, MegaEvm<&'a mut MemoryDatabase, INSP, DefaultExternalEnvs>) {
    let external_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, external_envs);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        data: Default::default(),
        value: U256::ZERO,
        gas_limit,
        gas_price: 0,
        ..Default::default()
    };

    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm =
        MegaEvm::new_with_accelerated_precompiles(context, None).with_inspector(inspector);
    let result = Evm::transact_commit(&mut evm, tx).unwrap();

    (result, evm)
}

/// Checks if the result is a volatile data access out of gas error.
fn is_volatile_data_access_oog(result: &ExecutionResult<MegaHaltReason>) -> bool {
    matches!(
        result,
        &ExecutionResult::Halt { reason: MegaHaltReason::VolatileDataAccessOutOfGas { .. }, .. }
    )
}

#[test]
fn test_timestamp_limits_gas() {
    // TIMESTAMP opcode should limit remaining gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(0u8)
        .append(TIMESTAMP)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    assert!(result.is_success(), "Transaction should succeed");
    // With detained gas restoration, gas_used should be much less than gas_limit
    // The contract does minimal work (TIMESTAMP, MSTORE, RETURN), so should use < 30K gas
    // If detained gas wasn't restored, gas_used would be ~29M
    let gas_used = result.gas_used();
    assert!(
        gas_used < 30_000,
        "gas_used should only reflect real work after TIMESTAMP limiting, but got {}. \
         If > 25M, detained gas was not restored.",
        gas_used
    );
}

#[test]
fn test_number_limits_gas() {
    // NUMBER opcode should limit remaining gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(0u8)
        .append(NUMBER)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    assert!(result.is_success(), "Transaction should succeed");
    let gas_used = result.gas_used();
    assert!(
        gas_used < 100_000,
        "gas_used should only reflect real work after NUMBER limiting, got {}",
        gas_used
    );
}

#[test]
fn test_coinbase_limits_gas() {
    // COINBASE opcode should limit remaining gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(0u8)
        .append(COINBASE)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    let gas_used = result.gas_used();
    assert!(
        gas_used < 100_000,
        "gas_used should only reflect real work after COINBASE limiting, got {}",
        gas_used
    );
}

#[test]
fn test_difficulty_limits_gas() {
    // DIFFICULTY/PREVRANDAO opcode should limit remaining gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(0u8)
        .append(DIFFICULTY)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    let gas_used = result.gas_used();
    assert!(
        gas_used < 100_000,
        "gas_used should only reflect real work after DIFFICULTY limiting, got {}",
        gas_used
    );
}

#[test]
fn test_gaslimit_limits_gas() {
    // GASLIMIT opcode should limit remaining gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(0u8)
        .append(GASLIMIT)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    let gas_used = result.gas_used();
    assert!(
        gas_used < 100_000,
        "gas_used should only reflect real work after GASLIMIT limiting, got {}",
        gas_used
    );
}

#[test]
fn test_basefee_limits_gas() {
    // BASEFEE opcode should limit remaining gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(0u8)
        .append(BASEFEE)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    let gas_used = result.gas_used();

    assert!(
        gas_used < 100_000,
        "gas_used should only reflect real work after BASEFEE limiting, got {}",
        gas_used
    );
}

#[test]
fn test_blockhash_limits_gas() {
    // BLOCKHASH opcode should limit remaining gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(0x01u8) // block number
        .append(BLOCKHASH)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );
    assert!(result.is_success(), "Transaction should succeed");

    let gas_used = result.gas_used();
    assert!(
        gas_used < 100_000,
        "gas_used should only reflect real work after BLOCKHASH limiting, got {}",
        gas_used
    );
}

#[test]
fn test_multiple_block_env_accesses() {
    // Multiple block env accesses should still limit gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP)
        .append(POP)
        .append(NUMBER)
        .append(POP)
        .append(COINBASE)
        .append(POP)
        .append(GASLIMIT)
        .append(POP)
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    assert!(result.is_success());
    let gas_used = result.gas_used();
    // After first block env access, gas should be limited (verified by small gas_used)
    assert!(
        gas_used < 100_000,
        "Multiple block env accesses should maintain gas limit. gas_used: {}",
        gas_used
    );
}

#[test]
fn test_block_env_access_with_nested_calls() {
    // Create a contract that accesses block env and then does more work
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP) // limits gas immediately
        .push_number(0u8)
        .append(MSTORE)
        // Try to do lots of work after gas is limited (simple loop)
        .push_number(100u8) // loop counter
        .append(JUMPDEST) // loop start at position ~6
        .push_number(1u8)
        .append(SWAP1)
        .append(SUB)
        .append(DUP1)
        .push_number(6u8) // jump back to JUMPDEST
        .append(JUMPI)
        .append(POP)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    let gas_used = result.gas_used();
    assert!(
        gas_used < 100_000,
        "gas_used should be small after block env access limiting, got {}",
        gas_used
    );
}

#[test]
fn test_no_gas_limit_without_block_env_access() {
    // Regular opcodes should NOT limit gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .push_number(1u8)
        .push_number(2u8)
        .append(ADD)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let gas_limit = 2_000_000;
    let (result, evm) = execute_bytecode(&mut db, gas_limit);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        TX_COMPUTE_GAS_LIMIT,
        "Compute gas limit should be the same as the transaction compute gas limit"
    );

    assert!(result.is_success());
    // Without block env access, gas should NOT be limited
    let gas_used = result.gas_used();
    assert!(
        gas_used < 50_000,
        "Regular opcodes should use minimal gas, expected < 50000, got {}",
        gas_used
    );
}

#[test]
fn test_out_of_gas_after_block_env_access() {
    // Try to do expensive work after block env access with limited gas
    let mut db = MemoryDatabase::default();
    let mut builder = BytecodeBuilder::default()
        .append(TIMESTAMP) // limits compute gas to 20M
        .append(POP);
    // Try to use more than 20M compute gas doing storage writes to unique slots
    // Each SSTORE (zero → non-zero, unique slot) costs ~22,100 gas in compute gas
    // To exceed 20M: need ~906 SSTOREs. Use 1000 to safely exceed the limit.
    // 1000 SSTOREs × 22,100 = 22.1M compute gas (exceeds 20M limit)
    for i in 1..=1000 {
        builder = builder.push_number(i as u32).push_number(i as u32).append(SSTORE);
    }
    let bytecode = builder.append(STOP).build();
    db.set_account_code(CONTRACT, bytecode);

    let total_gas = 1_000_000_000_000;
    let (result, _) = execute_bytecode(&mut db, total_gas);

    // Should run out of gas - 1000 SSTOREs cost 22.1M compute gas, but only 20M available after
    // limiting
    assert!(
        !result.is_success(),
        "Should run out of gas when attempting 1000 SSTOREs (22.1M compute gas) after block env access \
         (20M compute gas limit).",
    );
    assert!(
        is_volatile_data_access_oog(&result),
        "Should run out of gas due to volatile data access"
    );
    assert!(
        result.gas_used() < total_gas,
        "gas_used should be less than {total_gas}, got {}",
        result.gas_used()
    );
}

#[test]
fn test_nested_call_block_env_access_child_oog() {
    // Test that child contract runs out of gas when it accesses block env with limited gas
    let mut db = MemoryDatabase::default();

    // Nested contract that accesses TIMESTAMP then tries expensive work
    let mut nested_builder = BytecodeBuilder::default()
        .append(TIMESTAMP) // limits compute gas immediately to 20M
        .append(POP);
    // Try to do expensive compute work after block env access (should OOG)
    // Each SSTORE (zero → non-zero, unique slot) costs ~22,100 gas in compute gas
    // Use 1000 SSTOREs to exceed 20M compute gas limit: 1000 × 22,100 = 22.1M
    for i in 1..=1000 {
        nested_builder = nested_builder.push_number(i as u32).push_number(i as u32).append(SSTORE);
    }
    let nested_bytecode = nested_builder.push_number(0u8).push_number(0u8).append(RETURN).build();
    db.set_account_code(NESTED_CONTRACT, nested_bytecode);

    // Parent contract that calls nested contract
    let parent_bytecode = BytecodeBuilder::default()
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argSize
        .push_number(0u8) // argOffset
        .push_number(0u8) // value
        .push_address(NESTED_CONTRACT)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, parent_bytecode);

    let total_gas = 1_000_000_000_000;
    let (result, evm) = execute_bytecode(&mut db, total_gas);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    assert!(!result.is_success(), "Parent should succeed even if child runs out of gas");
    assert!(
        is_volatile_data_access_oog(&result),
        "Parent should run out of gas due to volatile data access"
    );
    assert!(
        result.gas_used() < total_gas,
        "gas_used should be less than {total_gas}, got {}",
        result.gas_used()
    );
}

#[test]
fn test_deeply_nested_call_block_env_access() {
    // Test multiple levels of nesting: CALLER -> CONTRACT -> NESTED_CONTRACT
    // When the deepest contract accesses block env through multiple call frames,
    // the gas limit propagates back through all parent frames
    let mut db = MemoryDatabase::default();

    // Deepest nested contract that accesses TIMESTAMP
    let nested_bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(NESTED_CONTRACT, nested_bytecode);

    // Middle contract that calls NESTED_CONTRACT
    let middle_bytecode = BytecodeBuilder::default()
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_address(NESTED_CONTRACT)
        .append(GAS)
        .append(CALL)
        .append(POP)
        // Try to do some work after nested call
        .push_number(1u8)
        .push_number(2u8)
        .append(ADD)
        .append(POP)
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, middle_bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    let success = result.is_success();
    let gas_used = result.gas_used();
    assert!(success, "Transaction should succeed");
    // With multiple call frames (2+ levels deep), the gas limit DOES propagate correctly
    assert!(
        gas_used < 100_000,
        "All parent calls should be limited when deeply nested call accesses block env. gas_used: {}",
        gas_used
    );

    // Note: We no longer check interpreter gas values via GasInspector.
    // The new compute gas limit system doesn't directly modify interpreter gas.
    // Instead, compute gas is tracked separately and enforced via AdditionalLimit.
}

#[test]
fn test_parent_block_env_access_oog_after_nested_call() {
    // Test that parent accessing block env runs out of gas when trying expensive work
    // even after making a nested call
    let mut db = MemoryDatabase::default();

    // Nested contract that does simple work (no block env access)
    let nested_bytecode = BytecodeBuilder::default()
        .push_number(1u8)
        .push_number(2u8)
        .append(ADD)
        .append(POP)
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(NESTED_CONTRACT, nested_bytecode);

    // Parent contract that accesses block env FIRST, then calls nested, then tries expensive work
    let mut parent_builder = BytecodeBuilder::default()
        .append(TIMESTAMP) // Parent accesses block env - limits parent's compute gas to 20M
        .append(POP)
        // Make a nested call (should succeed, child is not limited)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_address(NESTED_CONTRACT)
        .append(GAS)
        .append(CALL)
        .append(POP);
    // Try to do expensive compute work in parent (should OOG due to parent's own 20M compute limit)
    // Each SSTORE (zero → non-zero, unique slot) costs ~22,100 gas in compute gas
    // Use 1000 SSTOREs to exceed 20M: 1000 × 22,100 = 22.1M compute gas
    for i in 1..=1000 {
        parent_builder = parent_builder.push_number(i as u32).push_number(i as u32).append(SSTORE);
    }
    let parent_bytecode = parent_builder.push_number(0u8).push_number(0u8).append(RETURN).build();
    db.set_account_code(CONTRACT, parent_bytecode);

    let total_gas = 1_000_000_000_000;
    let (result, evm) = execute_bytecode(&mut db, total_gas);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    assert!(!result.is_success(), "Parent should run out of gas when attempting 1000 SSTOREs (22.1M compute gas) after accessing block env itself (20M compute limit).");
    assert!(
        is_volatile_data_access_oog(&result),
        "Parent should run out of gas due to volatile data access"
    );
    assert!(
        result.gas_used() < total_gas,
        "gas_used should be less than {total_gas}, got {}",
        result.gas_used()
    );
}

#[test]
fn test_nested_call_already_limited_no_further_restriction() {
    // Test that if parent already accessed block env, nested call doesn't make it worse
    let mut db = MemoryDatabase::default();

    // Nested contract that also accesses TIMESTAMP
    let nested_bytecode = BytecodeBuilder::default()
        .append(NUMBER)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(NESTED_CONTRACT, nested_bytecode);

    // Parent contract that accesses block env FIRST, then calls nested
    let parent_bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP) // Parent accesses block env first
        .append(POP)
        // Now make nested call
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_address(NESTED_CONTRACT)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, parent_bytecode);

    let (result, evm) = execute_bytecode(&mut db, 30_000_000);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    assert!(result.is_success(), "Transaction should succeed");
}

#[test]
fn test_volatile_data_access_oog_does_not_consume_all_gas() {
    // This test verifies that when a transaction runs out of gas due to volatile data access
    // (VolatileDataAccessOutOfGas), it does NOT consume all gas like a regular OutOfGas.
    // Instead, detained gas is refunded and gas_used reflects only actual work performed.
    let mut db = MemoryDatabase::default();

    // Contract that accesses TIMESTAMP then tries expensive work that exceeds the limit
    let mut builder = BytecodeBuilder::default()
        .append(TIMESTAMP) // Limits compute gas to 20M
        .append(POP);
    // Try to do 1000 SSTOREs (22.1M compute gas needed, but only 20M available after limiting)
    // Each SSTORE (zero → non-zero, unique slot) costs ~22,100 gas in compute gas
    // Note: Using push_number(i as u8) means we only get 256 unique slots (0-255),
    // so slots wrap around. But this is still sufficient for the test.
    // 1000 SSTOREs × 22,100 = 22.1M compute gas (exceeds 20M limit)
    for i in 1..=1000 {
        builder = builder.push_number(i as u32).push_number(i as u32).append(SSTORE);
    }
    let bytecode = builder.push_number(0u8).push_number(0u8).append(RETURN).build();
    db.set_account_code(CONTRACT, bytecode);

    let total_gas = 1_000_000_000_000;
    let (result, evm) = execute_bytecode(&mut db, total_gas);

    assert_eq!(
        evm.ctx_ref().additional_limit.borrow().compute_gas_limit,
        BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS
    );

    assert!(!result.is_success(), "Transaction should fail due to exceeding compute gas limit.");
    assert!(
        is_volatile_data_access_oog(&result),
        "Transaction should fail due to volatile data access"
    );

    let gas_used = result.gas_used();
    assert!(gas_used < total_gas, "gas_used should be less than {total_gas}, got {}", gas_used);
}
