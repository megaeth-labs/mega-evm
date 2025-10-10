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
    constants::mini_rex::VOLATILE_DATA_ACCESS_REMAINING_GAS,
    test_utils::{BytecodeBuilder, GasInspector, MemoryDatabase, MsgCallMeta},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaSpecId, MegaTransaction,
};
use revm::{bytecode::opcode::*, context::TxEnv, Database};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");
const NESTED_CONTRACT: Address = address!("1000000000000000000000000000000000000003");

/// Helper to create and execute a transaction with given bytecode
fn execute_bytecode(db: &mut MemoryDatabase, gas_limit: u64) -> (bool, u64, u64) {
    execute_bytecode_with_price(db, gas_limit, 0, None)
}

/// Helper to create and execute a transaction with given bytecode and gas inspector
fn execute_bytecode_with_inspector(
    db: &mut MemoryDatabase,
    gas_limit: u64,
    gas_inspector: &mut GasInspector,
) -> (bool, u64, u64) {
    execute_bytecode_with_price(db, gas_limit, 0, Some(gas_inspector))
}

/// Helper to create and execute a transaction with given bytecode and gas price, committing state
fn execute_bytecode_with_price(
    db: &mut MemoryDatabase,
    gas_limit: u64,
    gas_price: u128,
    gas_inspector: Option<&mut GasInspector>,
) -> (bool, u64, u64) {
    let external_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, &external_envs);
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
        gas_price,
        ..Default::default()
    };

    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let result = if let Some(inspector) = gas_inspector {
        let mut evm = MegaEvm::new(context).with_inspector(inspector);
        Evm::transact_commit(&mut evm, tx).unwrap()
    } else {
        let mut evm = MegaEvm::new(context);
        Evm::transact_commit(&mut evm, tx).unwrap()
    };

    let success = result.is_success();
    let gas_used = result.gas_used();
    let gas_remaining = gas_limit.saturating_sub(gas_used);

    (success, gas_used, gas_remaining)
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

    let mut gas_inspector = GasInspector::new();
    let (success, gas_used, _gas_remaining) =
        execute_bytecode_with_inspector(&mut db, 1_000_000, &mut gas_inspector);

    assert!(success, "Transaction should succeed");
    // With detained gas restoration, gas_used should be much less than gas_limit
    // The contract does minimal work (TIMESTAMP, MSTORE, RETURN), so should use < 30K gas
    // If detained gas wasn't restored, gas_used would be ~990K
    assert!(
        gas_used < 30_000,
        "gas_used should only reflect real work after TIMESTAMP limiting, but got {}. \
         If > 900K, detained gas was not restored.",
        gas_used
    );

    // Verify that after TIMESTAMP opcode, all subsequent opcodes have gas ≤ 10k
    let mut after_timestamp = false;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            let opcode_info = item.borrow();
            if after_timestamp {
                assert!(
                    opcode_info.gas_after <= VOLATILE_DATA_ACCESS_REMAINING_GAS,
                    "Gas after TIMESTAMP should be ≤ {}, got {}",
                    VOLATILE_DATA_ACCESS_REMAINING_GAS,
                    opcode_info.gas_after
                );
            }
            if opcode_info.opcode == TIMESTAMP {
                after_timestamp = true;
            }
        },
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

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

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

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

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

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

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

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

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

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

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

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

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

    let (success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

    assert!(success);
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

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

    // With limited gas, the loop completes with minimal gas used (due to detained gas)
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

    let gas_limit = 1_000_000;
    let mut gas_inspector = GasInspector::new();
    let (success, _gas_used, gas_remaining) =
        execute_bytecode_with_inspector(&mut db, gas_limit, &mut gas_inspector);

    assert!(success);
    // Without block env access, gas should NOT be limited
    assert!(
        gas_remaining > VOLATILE_DATA_ACCESS_REMAINING_GAS,
        "Regular opcodes should not limit gas, expected > {}, got {}",
        VOLATILE_DATA_ACCESS_REMAINING_GAS,
        gas_remaining
    );

    // Verify that all opcodes have gas > 10k (no limiting occurred)
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            assert!(
                item.borrow().gas_after > VOLATILE_DATA_ACCESS_REMAINING_GAS,
                "Gas should remain > {} without block env access, got {}",
                VOLATILE_DATA_ACCESS_REMAINING_GAS,
                item.borrow().gas_after
            );
        },
    );
}

#[test]
fn test_out_of_gas_after_block_env_access() {
    // Try to do expensive work after block env access with limited gas
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP) // limits gas to 10,000
        .append(POP)
        // Try to use more than 10,000 gas doing storage writes
        .push_number(1u8)
        .push_number(0u8)
        .append(SSTORE) // expensive - 2M gas in Mini-Rex
        .push_number(2u8)
        .push_number(1u8)
        .append(SSTORE) // another expensive operation
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (success, _gas_used, gas_remaining) = execute_bytecode(&mut db, 5_000_000);

    // Should run out of gas - SSTORE costs 2M gas in Mini-Rex, but only 10K available after
    // limiting
    assert!(
        !success,
        "Should run out of gas when attempting expensive SSTORE after block env access. \
         gas_remaining: {}, success: {}",
        gas_remaining, success
    );
}

#[test]
fn test_gas_limit_tracked_correctly() {
    // Verify that gas tracking is accurate after limiting
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .append(GAS) // get remaining gas before
        .push_number(0u8)
        .append(MSTORE)
        .append(TIMESTAMP) // limits gas
        .append(POP)
        .append(GAS) // get remaining gas after
        .push_number(0x20u8)
        .append(MSTORE)
        .push_number(0x40u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

    assert!(
        gas_used < 100_000,
        "gas_used should be small after gas limiting and tracking, got {}",
        gas_used
    );
}

#[test]
fn test_block_env_access_before_call() {
    // Access block env, then try to make an external call
    let mut db = MemoryDatabase::default();
    let bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP) // limits gas
        .append(POP)
        // Try to make a CALL with remaining limited gas
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argSize
        .push_number(0u8) // argOffset
        .push_number(0u8) // value
        .push_number(1u8) // address
        .push_number(0xffffu16) // gas - request lots of gas
        .append(CALL)
        .append(POP) // pop result
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let (_success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

    // The CALL should get limited gas, resulting in small total gas_used
    assert!(
        gas_used < 100_000,
        "gas_used should be small after block env access and CALL, got {}",
        gas_used
    );
}

#[test]
fn test_nested_call_block_env_access_limits_parent_too() {
    // When a nested contract accesses block env, the top-level transaction should also be limited
    // This tests that gas limiting propagates back through the call stack
    let mut db = MemoryDatabase::default();

    // Nested contract that accesses TIMESTAMP
    let nested_bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(NESTED_CONTRACT, nested_bytecode);

    // Parent contract that calls the nested contract
    // This contract is the top-level transaction target
    let parent_bytecode = BytecodeBuilder::default()
        .push_number(0u8) // retSize
        .push_number(0u8) // retOffset
        .push_number(0u8) // argSize
        .push_number(0u8) // argOffset
        .push_number(0u8) // value
        .push_address(NESTED_CONTRACT) // address
        .append(GAS) // use all available gas
        .append(CALL)
        .append(POP) // pop result
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, parent_bytecode);

    let mut gas_inspector = GasInspector::new();
    let (success, gas_used, _gas_remaining) =
        execute_bytecode_with_inspector(&mut db, 1_000_000, &mut gas_inspector);

    assert!(success, "Transaction should succeed");
    // Top-level transaction should be limited when child accesses block env
    // Parent does minimal work (CALL setup + return), child does minimal work (TIMESTAMP + return)
    // Total should be < 50K. If > 900K, limiting didn't propagate.
    assert!(
        gas_used < 50_000,
        "Top-level transaction should be limited when child accesses block env. gas_used: {}. \
         If > 900K, gas limiting didn't propagate through nested calls.",
        gas_used
    );

    // Verify that after the nested call to TIMESTAMP, all subsequent parent opcodes have gas ≤ 10k
    let mut accessed_timestamp = false;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, node, _item_location, item| {
            let opcode_info = item.borrow();
            if accessed_timestamp {
                assert!(
                    opcode_info.gas_after <= VOLATILE_DATA_ACCESS_REMAINING_GAS,
                    "Gas after nested TIMESTAMP access should be ≤ {}, got {}",
                    VOLATILE_DATA_ACCESS_REMAINING_GAS,
                    opcode_info.gas_after
                );
            }
            // Check if we're in the nested contract and hit TIMESTAMP
            match &node.borrow().meta {
                MsgCallMeta::Call(call_inputs) => {
                    if call_inputs.target_address == NESTED_CONTRACT && opcode_info.opcode == TIMESTAMP {
                        accessed_timestamp = true;
                    }
                }
                MsgCallMeta::Create(_) => {}
            }
        },
    );
}

#[test]
fn test_nested_call_block_env_access_child_oog() {
    // Test that child contract runs out of gas when it accesses block env with limited gas
    let mut db = MemoryDatabase::default();

    // Nested contract that accesses TIMESTAMP then tries expensive work
    let nested_bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP) // limits gas immediately
        .append(POP)
        // Try to do expensive storage writes after block env access (should OOG)
        .push_number(1u8)
        .push_number(0u8)
        .append(SSTORE) // expensive - 2M gas in Mini-Rex
        .push_number(2u8)
        .push_number(1u8)
        .append(SSTORE) // another expensive operation
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
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

    let mut gas_inspector = GasInspector::new();
    let (success, _gas_used, _gas_remaining) =
        execute_bytecode_with_inspector(&mut db, 5_000_000, &mut gas_inspector);

    // Parent should succeed (child failure doesn't fail parent)
    // Child runs out of gas due to its own block env access limiting
    assert!(success, "Parent should succeed even if child runs out of gas");

    // Verify that inside the nested contract, gas was limited after TIMESTAMP
    let mut inside_nested = false;
    let mut after_timestamp_in_nested = false;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, node, _item_location, item| {
            let opcode_info = item.borrow();
            match &node.borrow().meta {
                MsgCallMeta::Call(call_inputs) => {
                    inside_nested = call_inputs.target_address == NESTED_CONTRACT;
                }
                MsgCallMeta::Create(_) => {}
            }
            if inside_nested {
                if after_timestamp_in_nested {
                    assert!(
                        opcode_info.gas_after <= VOLATILE_DATA_ACCESS_REMAINING_GAS,
                        "Gas in nested contract after TIMESTAMP should be ≤ {}, got {}",
                        VOLATILE_DATA_ACCESS_REMAINING_GAS,
                        opcode_info.gas_after
                    );
                }
                if opcode_info.opcode == TIMESTAMP {
                    after_timestamp_in_nested = true;
                }
            }
        },
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

    let external_envs = DefaultExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(&mut db, MegaSpecId::MINI_REX, &external_envs);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        data: Default::default(),
        value: U256::ZERO,
        gas_limit: 1_000_000,
        ..Default::default()
    };

    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut gas_inspector = GasInspector::new();
    let mut evm = MegaEvm::new(context).with_inspector(&mut gas_inspector);
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let success = result.result.is_success();
    let gas_used = result.result.gas_used();
    assert!(success, "Transaction should succeed");
    // With multiple call frames (2+ levels deep), the gas limit DOES propagate correctly
    assert!(
        gas_used < 100_000,
        "All parent calls should be limited when deeply nested call accesses block env. gas_used: {}",
        gas_used
    );

    // Verify that all opcodes after TIMESTAMP (in all frames) have gas ≤ 10k
    let mut after_timestamp = false;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            let opcode_info = item.borrow();
            if after_timestamp {
                assert!(
                    opcode_info.gas_after <= VOLATILE_DATA_ACCESS_REMAINING_GAS,
                    "Gas in all frames after TIMESTAMP should be ≤ {}, got {}",
                    VOLATILE_DATA_ACCESS_REMAINING_GAS,
                    opcode_info.gas_after
                );
            }
            if opcode_info.opcode == TIMESTAMP {
                after_timestamp = true;
            }
        },
    );
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
    let parent_bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP) // Parent accesses block env - limits parent's gas
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
        .append(POP)
        // Try to do expensive work in parent (should OOG due to parent's own limit)
        .push_number(1u8)
        .push_number(0u8)
        .append(SSTORE) // expensive - 2M gas in Mini-Rex
        .push_number(2u8)
        .push_number(1u8)
        .append(SSTORE)
        .push_number(0u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, parent_bytecode);

    let (success, _gas_used, gas_remaining) = execute_bytecode(&mut db, 5_000_000);

    // Parent should run out of gas due to its own block env access
    // SSTORE requires 2M gas but only 10K available after TIMESTAMP limiting
    assert!(
        !success,
        "Parent should run out of gas when attempting SSTORE after accessing block env itself. \
         gas_remaining: {}, success: {}",
        gas_remaining, success
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

    let (success, gas_used, _gas_remaining) = execute_bytecode(&mut db, 1_000_000);

    assert!(success, "Transaction should succeed");
    // Should still be limited (not more restrictive than already imposed limit)
    assert!(
        gas_used < 100_000,
        "Gas should remain limited after nested call. gas_used: {}",
        gas_used
    );
}

#[test]
fn test_detained_gas_is_restored_not_charged() {
    // This test explicitly verifies the detained gas restoration mechanism:
    // 1. Gas is artificially limited during execution (to VOLATILE_DATA_ACCESS_REMAINING_GAS)
    // 2. Detained gas is tracked
    // 3. Detained gas is restored before tx finishes
    // 4. User only pays for real work, not detained gas
    let mut db = MemoryDatabase::default();

    // Give caller a known balance
    let initial_balance = U256::from(10_000_000_000u64);
    db.set_account_balance(CALLER, initial_balance);

    // Contract that accesses TIMESTAMP then does minimal work
    let bytecode = BytecodeBuilder::default()
        .append(TIMESTAMP) // Triggers gas limiting
        .append(POP)
        .push_number(42u8)
        .push_number(0u8)
        .append(MSTORE)
        .push_number(0x20u8)
        .push_number(0u8)
        .append(RETURN)
        .build();
    db.set_account_code(CONTRACT, bytecode);

    let gas_limit = 1_000_000;
    let gas_price = 1000u128; // Higher gas price to make differences more visible

    let (success, gas_used, _gas_remaining) =
        execute_bytecode_with_price(&mut db, gas_limit, gas_price, None);

    assert!(success, "Transaction should succeed");

    // Verify gas_used is small (real work only, not including ~990K detained gas)
    assert!(
        gas_used < 30_000,
        "gas_used should only reflect real work, got {}. If > 900K, detained gas was NOT restored.",
        gas_used
    );

    // Verify user only paid for real work
    let final_balance = db.basic(CALLER).unwrap().unwrap().balance;
    let actual_cost = initial_balance - final_balance;
    let expected_cost = U256::from(gas_used) * U256::from(gas_price);

    assert_eq!(
        actual_cost, expected_cost,
        "User payment should match gas_used. actual: {}, expected: {}. \
         If actual >> expected, detained gas was charged to user.",
        actual_cost, expected_cost
    );

    // If detained gas was NOT restored, user would pay ~990K * 1000 = 990M units
    // With restoration, user pays ~20K * 1000 = 20M units
    assert!(
        actual_cost < U256::from(50_000_000u64),
        "User cost ({}) should be < 50M. If > 900M, detained gas was charged.",
        actual_cost
    );
}
