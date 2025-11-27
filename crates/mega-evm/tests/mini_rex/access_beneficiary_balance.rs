//! Simplified tests for beneficiary balance access tracking functionality.
//!
//! When the block beneficiary is accessed, gas is immediately detained (limited to 10,000).
//! Any action which causes `ResultAndState` to contain the beneficiary should mark beneficiary
//! access and trigger gas detention.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::mini_rex::BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
    test_utils::{BytecodeBuilder, GasInspector, MsgCallMeta},
    EmptyExternalEnv, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    bytecode::opcode::{BALANCE, EXTCODECOPY, EXTCODEHASH, EXTCODESIZE, POP, PUSH0, STOP},
    context::{result::ResultAndState, BlockEnv, ContextSetters, ContextTr, TxEnv},
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
};

const BENEFICIARY: Address = address!("0000000000000000000000000000000000BEEF01");
const CALLER_ADDR: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT_ADDR: Address = address!("0000000000000000000000000000000000100001");
const NESTED_CONTRACT: Address = address!("0000000000000000000000000000000000100002");

fn create_evm() -> MegaEvm<CacheDB<EmptyDB>, GasInspector, EmptyExternalEnv> {
    let db = CacheDB::<EmptyDB>::default();
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX);

    let block_env =
        BlockEnv { beneficiary: BENEFICIARY, number: U256::from(10), ..Default::default() };
    context.set_block(block_env);

    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));

    MegaEvm::new(context).with_inspector(GasInspector::new())
}

fn set_account_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = AccountInfo { code: Some(bytecode), code_hash, ..Default::default() };
    db.insert_account_info(address, account_info);
}

fn execute_tx(
    evm: &mut MegaEvm<CacheDB<EmptyDB>, GasInspector, EmptyExternalEnv>,
    caller: Address,
    to: Option<Address>,
    value: U256,
    disable_beneficiary: bool,
) -> ResultAndState<MegaHaltReason> {
    if disable_beneficiary {
        evm.disable_beneficiary();
    }

    let tx = MegaTransaction {
        base: TxEnv {
            caller,
            kind: match to {
                Some(addr) => TxKind::Call(addr),
                None => TxKind::Create,
            },
            data: Bytes::default(),
            value,
            gas_limit: 10000000,
            ..Default::default()
        },
        ..Default::default()
    };

    alloy_evm::Evm::transact_raw(evm, tx).unwrap()
}

fn assert_beneficiary_detection(
    evm: &MegaEvm<CacheDB<EmptyDB>, GasInspector, EmptyExternalEnv>,
    result_and_state: &ResultAndState<MegaHaltReason>,
) {
    // Transaction should succeed
    assert!(result_and_state.result.is_success());

    // If state contains beneficiary, should have detection
    if result_and_state.state.contains_key(&BENEFICIARY) {
        assert!(evm.ctx_ref().volatile_data_tracker.borrow().has_accessed_beneficiary_balance());
    }
}

/// Test that verifies beneficiary balance access detection when the beneficiary is the transaction
/// caller. This test ensures that when the beneficiary address is used as the caller in a
/// transaction, the system correctly detects and tracks beneficiary balance access.
///
/// When beneficiary is the caller, access is detected but gas is not immediately limited.
/// This test primarily verifies that beneficiary access is correctly tracked.
#[test]
fn test_beneficiary_caller() {
    let mut evm = create_evm();
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, vec![STOP].into());

    let result_and_state =
        execute_tx(&mut evm, BENEFICIARY, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);

    // Beneficiary balance access should be detected
    assert!(evm.ctx_ref().volatile_data_tracker.borrow().has_accessed_beneficiary_balance());
}

/// Test that verifies beneficiary balance access detection when the beneficiary is the transaction
/// recipient. This test ensures that when a transaction sends value to the beneficiary address,
/// the system correctly detects and tracks beneficiary balance access.
///
/// When beneficiary is the recipient, access is detected but gas is not immediately limited.
/// This test primarily verifies that beneficiary access is correctly tracked.
#[test]
fn test_beneficiary_recipient() {
    let mut evm = create_evm();

    // Give caller some balance
    evm.ctx().db_mut().insert_account_info(
        CALLER_ADDR,
        AccountInfo { balance: U256::from(1_000_000_000_000_000_000u64), ..Default::default() },
    );

    let result_and_state = execute_tx(
        &mut evm,
        CALLER_ADDR,
        Some(BENEFICIARY),
        U256::from(500_000_000_000_000_000u64),
        false,
    );
    assert_beneficiary_detection(&evm, &result_and_state);

    // Beneficiary balance access should be detected
    assert!(evm.ctx_ref().volatile_data_tracker.borrow().has_accessed_beneficiary_balance());
}

/// Test that verifies beneficiary balance access detection when a contract uses the BALANCE opcode
/// on the beneficiary address. This test ensures that when a contract reads the balance of the
/// beneficiary address using the BALANCE opcode, the system correctly detects and tracks
/// beneficiary balance access.
///
/// Gas should be > 10k before BALANCE opcode, then ≤ 10k after.
#[test]
fn test_balance_opcode() {
    let mut evm = create_evm();

    // Contract that reads beneficiary balance
    let code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();

    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code);

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);

    // Verify that after BALANCE opcode, all subsequent opcodes have gas ≤ 10k
    let mut after_balance = false;
    let gas_inspector = &evm.inspector;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            let opcode_info = item.borrow();
            if after_balance {
                assert!(
                    opcode_info.gas_after <= BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    "Gas after BALANCE should be ≤ {}, got {}",
                    BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    opcode_info.gas_after
                );
            }
            if opcode_info.opcode.as_str() == "BALANCE" {
                after_balance = true;
            }
        },
    );
}

/// Test that verifies beneficiary balance access detection when a contract uses the EXTCODESIZE
/// opcode on the beneficiary address. This test ensures that when a contract checks the code size
/// of the beneficiary address using the EXTCODESIZE opcode, the system correctly detects and tracks
/// beneficiary balance access.
///
/// Gas should be > 10k before EXTCODESIZE opcode, then ≤ 10k after.
#[test]
fn test_extcodesize_opcode() {
    let mut evm = create_evm();

    // Give beneficiary some code
    set_account_code(evm.ctx().db_mut(), BENEFICIARY, vec![STOP].into());

    // Contract that checks beneficiary code size
    let code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(EXTCODESIZE)
        .append(POP)
        .stop()
        .build();

    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code);

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);

    // Verify that after EXTCODESIZE opcode, all subsequent opcodes have gas ≤ 10k
    let mut after_extcodesize = false;
    let gas_inspector = &evm.inspector;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            let opcode_info = item.borrow();
            if after_extcodesize {
                assert!(
                    opcode_info.gas_after <= BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    "Gas after EXTCODESIZE should be ≤ {}, got {}",
                    BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    opcode_info.gas_after
                );
            }
            if opcode_info.opcode.as_str() == "EXTCODESIZE" {
                after_extcodesize = true;
            }
        },
    );
}

/// Test that verifies beneficiary balance access detection when a contract uses the EXTCODECOPY
/// opcode on the beneficiary address. This test ensures that when a contract copies code from the
/// beneficiary address using the EXTCODECOPY opcode, the system correctly detects and tracks
/// beneficiary balance access.
///
/// Gas should be > 10k before EXTCODECOPY opcode, then ≤ 10k after.
#[test]
fn test_extcodecopy_opcode() {
    let mut evm = create_evm();

    // Give beneficiary some code
    set_account_code(evm.ctx().db_mut(), BENEFICIARY, vec![STOP, STOP, STOP, STOP].into());

    // Contract that copies beneficiary code: EXTCODECOPY(beneficiary, 0, 0, 4)
    let code = BytecodeBuilder::default()
        .push_number(4u8) // size = 4
        .push_number(0u8) // offset = 0
        .push_number(0u8) // destOffset = 0
        .push_address(BENEFICIARY) // beneficiary address
        .append(EXTCODECOPY)
        .stop()
        .build();

    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code);

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);

    // Verify that after EXTCODECOPY opcode, all subsequent opcodes have gas ≤ 10k
    let mut after_extcodecopy = false;
    let gas_inspector = &evm.inspector;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            let opcode_info = item.borrow();
            if after_extcodecopy {
                assert!(
                    opcode_info.gas_after <= BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    "Gas after EXTCODECOPY should be ≤ {}, got {}",
                    BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    opcode_info.gas_after
                );
            }
            if opcode_info.opcode.as_str() == "EXTCODECOPY" {
                after_extcodecopy = true;
            }
        },
    );
}

/// Test that verifies beneficiary balance access detection when a contract uses the EXTCODEHASH
/// opcode on the beneficiary address. This test ensures that when a contract reads the code hash
/// of the beneficiary address using the EXTCODEHASH opcode, the system correctly detects and tracks
/// beneficiary balance access.
///
/// Gas should be > 10k before EXTCODEHASH opcode, then ≤ 10k after.
#[test]
fn test_extcodehash_opcode() {
    let mut evm = create_evm();

    // Give beneficiary some code
    set_account_code(evm.ctx().db_mut(), BENEFICIARY, vec![STOP].into());

    // Contract that reads beneficiary code hash
    let code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(EXTCODEHASH)
        .append(POP)
        .stop()
        .build();

    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code);

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);

    // Verify that after EXTCODEHASH opcode, all subsequent opcodes have gas ≤ 10k
    let mut after_extcodehash = false;
    let gas_inspector = &evm.inspector;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            let opcode_info = item.borrow();
            if after_extcodehash {
                assert!(
                    opcode_info.gas_after <= BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    "Gas after EXTCODEHASH should be ≤ {}, got {}",
                    BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    opcode_info.gas_after
                );
            }
            if opcode_info.opcode.as_str() == "EXTCODEHASH" {
                after_extcodehash = true;
            }
        },
    );
}

/// Test that verifies beneficiary balance access detection when a contract performs a CALL
/// to the beneficiary address. This test ensures that making a call to the beneficiary triggers
/// gas detention.
///
/// Gas should be > 10k before the CALL, then ≤ 10k after.
#[test]
fn test_call_to_beneficiary() {
    let mut evm = create_evm();

    // Give beneficiary some code to execute
    set_account_code(evm.ctx().db_mut(), BENEFICIARY, vec![PUSH0, PUSH0, STOP].into());

    // Contract that calls beneficiary: CALL(gas, beneficiary, 0, 0, 0, 0, 0)
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]) // retSize, retOffset, argSize, argOffset, value
        .push_address(BENEFICIARY) // address
        .append(revm::bytecode::opcode::GAS) // gas
        .append(revm::bytecode::opcode::CALL)
        .stop()
        .build();

    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code);

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);

    // Verify that after the CALL, gas is limited
    let mut after_call_to_beneficiary = false;
    let gas_inspector = &evm.inspector;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, node, _item_location, item| {
            let opcode_info = item.borrow();

            // Check if we just completed a CALL to beneficiary
            if opcode_info.opcode.as_str() == "CALL" {
                if let MsgCallMeta::Call(call_inputs) = &node.borrow().meta {
                    if call_inputs.target_address == BENEFICIARY {
                        after_call_to_beneficiary = true;
                    }
                }
            }

            if after_call_to_beneficiary {
                assert!(
                    opcode_info.gas_after <= BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    "Gas after CALL to beneficiary should be ≤ {}, got {}",
                    BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    opcode_info.gas_after
                );
            }
        },
    );
}

/// Test that verifies beneficiary balance access detection when a contract performs a STATICCALL
/// to the beneficiary address. This test ensures that making a static call to the beneficiary
/// triggers gas detention.
///
/// Gas should be > 10k before the STATICCALL, then ≤ 10k after.
#[test]
fn test_staticcall_to_beneficiary() {
    let mut evm = create_evm();

    // Give beneficiary some code to execute
    set_account_code(evm.ctx().db_mut(), BENEFICIARY, vec![PUSH0, PUSH0, STOP].into());

    // Contract that static calls beneficiary: STATICCALL(gas, beneficiary, 0, 0, 0, 0)
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // retSize, retOffset, argSize, argOffset
        .push_address(BENEFICIARY) // address
        .append(revm::bytecode::opcode::GAS) // gas
        .append(revm::bytecode::opcode::STATICCALL)
        .stop()
        .build();

    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code);

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);

    // Verify that after the STATICCALL, gas is limited
    let mut after_staticcall_to_beneficiary = false;
    let gas_inspector = &evm.inspector;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, node, _item_location, item| {
            let opcode_info = item.borrow();

            // Check if we just completed a STATICCALL to beneficiary
            if opcode_info.opcode.as_str() == "STATICCALL" {
                if let MsgCallMeta::Call(call_inputs) = &node.borrow().meta {
                    if call_inputs.target_address == BENEFICIARY {
                        after_staticcall_to_beneficiary = true;
                    }
                }
            }

            if after_staticcall_to_beneficiary {
                assert!(
                    opcode_info.gas_after <= BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    "Gas after STATICCALL to beneficiary should be ≤ {}, got {}",
                    BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    opcode_info.gas_after
                );
            }
        },
    );
}

/// Test that verifies gas limiting propagates correctly through nested calls when a nested
/// contract accesses the beneficiary. This test ensures that when a child call accesses the
/// beneficiary, both the child and parent frames get gas limited.
///
/// Parent calls Child, Child accesses beneficiary → both should have gas ≤ 10k after.
#[test]
fn test_nested_call_beneficiary_access() {
    let mut evm = create_evm();

    // Nested contract that reads beneficiary balance
    let nested_code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();
    set_account_code(evm.ctx().db_mut(), NESTED_CONTRACT, nested_code);

    // Main contract that calls nested contract
    let main_code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]) // retSize, retOffset, argSize, argOffset, value
        .push_address(NESTED_CONTRACT) // address
        .append(revm::bytecode::opcode::GAS) // gas
        .append(revm::bytecode::opcode::CALL)
        .append(POP) // Pop the return value
        .stop()
        .build();
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, main_code);

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);

    // Verify that after the nested call accesses beneficiary, all subsequent opcodes have gas ≤ 10k
    let mut accessed_beneficiary_in_nested = false;
    let gas_inspector = &evm.inspector;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, node, _item_location, item| {
            let opcode_info = item.borrow();

            // Check if we're in the nested contract and hit BALANCE on beneficiary
            if opcode_info.opcode.as_str() == "BALANCE" {
                if let MsgCallMeta::Call(call_inputs) = &node.borrow().meta {
                    if call_inputs.target_address == NESTED_CONTRACT {
                        accessed_beneficiary_in_nested = true;
                    }
                }
            }

            if accessed_beneficiary_in_nested {
                assert!(
                    opcode_info.gas_after <= BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    "Gas after nested beneficiary access should be ≤ {}, got {}",
                    BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
                    opcode_info.gas_after
                );
            }
        },
    );
}

/// Test that verifies detained gas is restored (refunded) at the end of the transaction.
/// This ensures that users are not charged for the gas that was temporarily detained during
/// beneficiary access.
///
/// The transaction should start with high gas, detain most of it when beneficiary is accessed,
/// but the detained gas should be refunded so the final `gas_used` is reasonable.
#[test]
fn test_detained_gas_is_restored() {
    let mut evm = create_evm();

    // Simple contract that accesses beneficiary balance
    let code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code);

    // Execute with a large gas limit
    let gas_limit = 1_000_000u64;
    let tx = MegaTransaction {
        base: TxEnv {
            caller: CALLER_ADDR,
            kind: TxKind::Call(CONTRACT_ADDR),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success());
    assert!(evm.ctx_ref().volatile_data_tracker.borrow().has_accessed_beneficiary_balance());

    // The gas_used should be much less than the gas_limit because detained gas is refunded.
    // We expect gas_used to be only a few thousand (for the actual work done), not close to 1M.
    let gas_used = result.result.gas_used();
    assert!(
        gas_used < 50_000,
        "Gas used should be low after detained gas restoration, got {}",
        gas_used
    );

    // Verify that gas was actually limited during execution
    let gas_inspector = &evm.inspector;
    let mut saw_limited_gas = false;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            let opcode_info = item.borrow();
            if opcode_info.gas_after <= BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS {
                saw_limited_gas = true;
            }
        },
    );
    assert!(saw_limited_gas, "Should have seen gas limited to 10k during execution");
}
