//! Tests for the `STORAGE_CALL_STIPEND` feature introduced in REX4.
//!
//! `MegaETH`'s 10x storage gas multiplier on LOG opcodes causes `LOG1` to cost 4,500 gas
//! (750 compute + 3,750 storage), exceeding the EVM's `CALL_STIPEND` of 2,300.
//! This breaks `transfer()` / `send()` to contracts that emit events in `receive()`.
//!
//! REX4 introduces `STORAGE_CALL_STIPEND` (23,000 gas) that is added to the callee's gas
//! when CALL/CALLCODE transfers value.
//! The callee's compute gas limit remains at the original level, so the extra gas can only
//! be consumed by storage gas operations.
//! On return, unused `STORAGE_CALL_STIPEND` is burned.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, IMegaAccessControl, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId, MegaTransaction, MegaTransactionError, ACCESS_CONTROL_ADDRESS,
    KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

const CALLER: Address = address!("0000000000000000000000000000000000200000");
/// A contract that performs CALL with value to RECEIVER.
const SENDER_CONTRACT: Address = address!("0000000000000000000000000000000000200001");
/// A contract that emits events when receiving ETH (simulates `receive()`).
const RECEIVER: Address = address!("0000000000000000000000000000000000200002");
/// An empty contract (STOP immediately).
const EMPTY_RECEIVER: Address = address!("0000000000000000000000000000000000200003");
/// An intermediate contract for nested value-transfer tests.
const MIDDLE_CONTRACT: Address = address!("0000000000000000000000000000000000200005");
/// The 4-byte selector for `disableVolatileDataAccess()`.
const DISABLE_VOLATILE_DATA_ACCESS_SELECTOR: [u8; 4] =
    IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Executes a transaction and returns the result.
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context =
        MegaContext::new(db, spec).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

/// Builds bytecode for a contract that does CALL(gas=0, to, value=1 wei, ...) to simulate
/// `address.transfer(1)`.
/// The CALL forwards 0 gas explicitly — the callee only gets the `CALL_STIPEND` (+ any
/// `STORAGE_CALL_STIPEND` under REX4).
fn build_transfer_contract(to: Address) -> Bytes {
    BytecodeBuilder::default()
        // CALL(gas, addr, value, argsOffset, argsSize, retOffset, retSize)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value = 1 wei
        .push_address(to)
        .push_number(0_u64) // gas = 0 (rely on stipend)
        .append(CALL)
        // Return the success flag (1 = success, 0 = revert)
        .push_number(0_u64) // offset
        .append(MSTORE)
        .push_number(32_u64) // size
        .push_number(0_u64) // offset
        .append(RETURN)
        .build()
}

/// Builds bytecode for a contract that does CALLCODE(gas=0, to, value=1 wei, ...) to simulate
/// a stipend-limited CALLCODE with value transfer.
fn build_callcode_transfer_contract(to: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value = 1 wei
        .push_address(to)
        .push_number(0_u64) // gas = 0 (rely on stipend)
        .append(CALLCODE)
        .push_number(0_u64) // offset
        .append(MSTORE)
        .push_number(32_u64) // size
        .push_number(0_u64) // offset
        .append(RETURN)
        .build()
}

/// Builds bytecode for a contract that CALLs a system contract with value and 4 bytes calldata.
/// This exercises the `frame_init` interception path, which uses `push_empty_frame()`.
fn build_value_call_with_selector(to: Address, selector: [u8; 4]) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, selector)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value = 1 wei
        .push_address(to)
        .push_number(0_u64) // gas = 0 (rely on stipend)
        .append(CALL)
        .push_number(0_u64) // offset
        .append(MSTORE)
        .push_number(32_u64) // size
        .push_number(0_u64) // offset
        .append(RETURN)
        .build()
}

/// Builds bytecode for a contract that emits LOG1 with 0 bytes data.
/// Total cost: 750 compute + 3,750 storage = 4,500 gas.
fn build_log1_receiver() -> Bytes {
    BytecodeBuilder::default()
        // LOG1(offset, size, topic1)
        .push_number(0xdeadbeef_u64) // topic1
        .push_number(0_u64) // size = 0
        .push_number(0_u64) // offset = 0
        .append(LOG1)
        .append(STOP)
        .build()
}

/// Builds bytecode for a contract that emits LOG2 with 32 bytes data.
/// Compute: 375 + 375*2 + 8*32 = 1,381
/// Storage: 3,750*2 + 80*32 = 10,060
/// Total: 11,441 gas.
fn build_log2_receiver() -> Bytes {
    BytecodeBuilder::default()
        // Store 32 bytes of data in memory first
        .push_u256(U256::from(0x1234))
        .push_number(0_u64)
        .append(MSTORE)
        // LOG2(offset, size, topic1, topic2)
        .push_number(0xcafe_u64) // topic2
        .push_number(0xbeef_u64) // topic1
        .push_number(32_u64) // size = 32 bytes
        .push_number(0_u64) // offset = 0
        .append(LOG2)
        .append(STOP)
        .build()
}

/// Sets up a database with CALLER having enough ETH and the given contracts deployed.
fn setup_db(contracts: &[(Address, Bytes)]) -> MemoryDatabase {
    let mut db =
        MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000_000_000u128));
    for (addr, bytecode) in contracts {
        db = db.account_code(*addr, bytecode.clone());
    }
    // Ensure SENDER_CONTRACT has balance for value transfers (set after code).
    db.set_account_balance(SENDER_CONTRACT, U256::from(1_000_000_000u128));
    db
}

fn default_tx() -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(SENDER_CONTRACT).gas_limit(100_000_000).build_fill()
}

fn direct_value_transfer_tx(to: Address, gas_limit: u64) -> TxEnv {
    TxEnvBuilder::default()
        .caller(CALLER)
        .call(to)
        .gas_limit(gas_limit)
        .value(U256::from(1_u64))
        .build_fill()
}

// ============================================================================
// TESTS
// ============================================================================

/// Under REX4, CALL with value transfer to a contract that emits LOG1 should succeed.
/// The callee gets `CALL_STIPEND` (2,300) + `STORAGE_CALL_STIPEND` (23,000) = 25,300 total gas.
/// LOG1 costs 4,500 total (750 compute + 3,750 storage), which fits within 25,300.
#[test]
fn test_log1_in_receive_succeeds_under_rex4() {
    let sender_code = build_transfer_contract(RECEIVER);
    let receiver_code = build_log1_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            // The return data contains the CALL success flag.
            // 1 = inner CALL succeeded.
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            assert_eq!(success_flag, U256::from(1), "inner CALL should succeed under REX4");
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// Under REX3 (pre-REX4), the same CALL should fail because `STORAGE_CALL_STIPEND` is not
/// available and the callee only gets 2,300 gas which is insufficient for LOG1 (4,500).
#[test]
fn test_log1_in_receive_fails_under_rex3() {
    let sender_code = build_transfer_contract(RECEIVER);
    let receiver_code = build_log1_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX3, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            assert_eq!(
                success_flag,
                U256::ZERO,
                "inner CALL should fail under REX3 (no STORAGE_CALL_STIPEND)"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// The storage-call stipend only applies to internal `CALL`/`CALLCODE`, not to the
/// top-level transaction frame.
#[test]
fn test_top_level_value_transfer_does_not_get_storage_call_stipend() {
    let receiver_code = build_log1_receiver();
    let tx = direct_value_transfer_tx(RECEIVER, 63_000);

    let mut rex3_db = setup_db(&[(RECEIVER, receiver_code.clone())]);
    let rex3_result = transact(MegaSpecId::REX3, &mut rex3_db, tx.clone()).unwrap();
    assert!(
        matches!(rex3_result.result, ExecutionResult::Halt { .. }),
        "top-level value transfer should still halt under REX3"
    );

    let mut rex4_db = setup_db(&[(RECEIVER, receiver_code)]);
    let rex4_result = transact(MegaSpecId::REX4, &mut rex4_db, tx).unwrap();
    assert!(
        matches!(rex4_result.result, ExecutionResult::Halt { .. }),
        "top-level value transfer must NOT receive STORAGE_CALL_STIPEND under REX4"
    );
}

/// Under REX4, LOG2 with 32 bytes data (11,441 total gas) should also succeed in a
/// stipend-limited call.
#[test]
fn test_log2_in_receive_succeeds_under_rex4() {
    let sender_code = build_transfer_contract(RECEIVER);
    let receiver_code = build_log2_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            assert_eq!(success_flag, U256::from(1), "inner CALL with LOG2 should succeed");
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// Under REX4, CALLCODE with value transfer should also receive `STORAGE_CALL_STIPEND`.
/// The callee code executes in the caller's context, but the gas semantics still include
/// the extra storage-only stipend.
#[test]
fn test_callcode_gets_storage_call_stipend() {
    let sender_code = build_callcode_transfer_contract(RECEIVER);
    let receiver_code = build_log1_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            assert_eq!(
                success_flag,
                U256::from(1),
                "inner CALLCODE should succeed under REX4 with STORAGE_CALL_STIPEND"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// `STORAGE_CALL_STIPEND` should be burned on return.
/// When the callee does nothing (STOP), the parent should NOT recover the stipend as free gas.
/// We verify this by comparing `gas_used` with and without value transfer.
#[test]
fn test_storage_call_stipend_burned_on_return() {
    // Contract that CALLs EMPTY_RECEIVER with value (gets stipend, stipend should be burned)
    let sender_with_value = build_transfer_contract(EMPTY_RECEIVER);
    // Contract that CALLs EMPTY_RECEIVER without value (no stipend)
    let sender_no_value = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value = 0 (no transfer)
        .push_address(EMPTY_RECEIVER)
        .push_number(0_u64) // gas = 0
        .append(CALL)
        .append(STOP)
        .build();
    let empty_code = BytecodeBuilder::default().append(STOP).build();

    // Test with value transfer (gets STORAGE_CALL_STIPEND)
    let mut db_with_value =
        setup_db(&[(SENDER_CONTRACT, sender_with_value), (EMPTY_RECEIVER, empty_code.clone())]);
    let result_with_value = transact(MegaSpecId::REX4, &mut db_with_value, default_tx()).unwrap();
    let gas_used_with_value = match &result_with_value.result {
        ExecutionResult::Success { gas_used, .. } => *gas_used,
        other => panic!("expected Success, got {other:?}"),
    };

    // The gas_used should NOT include the burned STORAGE_CALL_STIPEND as "free" gas.
    // If the stipend leaked, the parent would have more remaining gas, resulting in lower gas_used.
    // We verify that gas_used is at least what we'd expect with the CALLVALUE cost.
    // CALLVALUE cost = 9000, so the transfer CALL should cost the parent at least 9000 more.
    let sender_no_value_addr = address!("0000000000000000000000000000000000200004");
    let mut db_no_value =
        setup_db(&[(sender_no_value_addr, sender_no_value), (EMPTY_RECEIVER, empty_code)]);
    let tx_no_value = TxEnvBuilder::default()
        .caller(CALLER)
        .call(sender_no_value_addr)
        .gas_limit(100_000_000)
        .build_fill();
    let result_no_value = transact(MegaSpecId::REX4, &mut db_no_value, tx_no_value).unwrap();
    let gas_used_no_value = match &result_no_value.result {
        ExecutionResult::Success { gas_used, .. } => *gas_used,
        other => panic!("expected Success, got {other:?}"),
    };

    // With value transfer: parent pays CALLVALUE (9000), but recovers the original
    // CALL_STIPEND (2300) as unused gas. Net cost ≈ 6700.
    // Without value: no CALLVALUE cost, no stipend.
    // The difference should be approximately CALLVALUE - CALL_STIPEND = 6700.
    // If STORAGE_CALL_STIPEND (23000) leaked, the difference would be negative
    // (parent would gain ~23000 free gas, far exceeding the CALLVALUE cost).
    let diff = gas_used_with_value.saturating_sub(gas_used_no_value);
    assert!(
        diff >= 6000,
        "STORAGE_CALL_STIPEND should be burned, not leaked. \
         gas_used_with_value={gas_used_with_value}, gas_used_no_value={gas_used_no_value}, diff={diff}"
    );
}

/// CALL without value transfer should NOT receive `STORAGE_CALL_STIPEND`.
#[test]
fn test_no_stipend_without_value_transfer() {
    // Contract that CALLs RECEIVER with value=0
    let sender_no_value = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value = 0
        .push_address(RECEIVER)
        .push_number(0_u64) // gas = 0
        .append(CALL)
        // Return success flag
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();
    let receiver_code = build_log1_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_no_value), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            // Without value transfer, no stipend at all (not even CALL_STIPEND).
            // The callee gets 0 gas and can't execute LOG1.
            assert_eq!(
                success_flag,
                U256::ZERO,
                "CALL without value should NOT get STORAGE_CALL_STIPEND"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// DELEGATECALL should NOT receive `STORAGE_CALL_STIPEND` (no value transfer possible).
#[test]
fn test_delegatecall_no_storage_call_stipend() {
    // Contract that does DELEGATECALL to RECEIVER
    let sender_code = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(RECEIVER)
        .push_number(0_u64) // gas = 0
        .append(DELEGATECALL)
        // Return success flag
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();
    let receiver_code = build_log1_receiver();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            assert_eq!(
                success_flag,
                U256::ZERO,
                "DELEGATECALL should NOT get STORAGE_CALL_STIPEND"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// STATICCALL should NOT receive `STORAGE_CALL_STIPEND` (no value transfer, no state changes).
#[test]
fn test_staticcall_no_storage_call_stipend() {
    // Contract that does STATICCALL to a contract that tries LOG1 (will fail due to static)
    // Use a simple contract that just does ADD to avoid the static violation
    let callee_code = BytecodeBuilder::default()
        .push_number(1_u64)
        .push_number(2_u64)
        .append(ADD)
        .append(POP)
        .append(STOP)
        .build();
    let sender_code = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_address(RECEIVER)
        .push_number(0_u64) // gas = 0
        .append(STATICCALL)
        .append(STOP)
        .build();
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, callee_code)]);

    // Just verify it runs without issues — STATICCALL has no value transfer so no stipend.
    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    assert!(matches!(result.result, ExecutionResult::Success { .. }), "STATICCALL should succeed");
}

/// When the callee reverts, the `STORAGE_CALL_STIPEND` should still be burned
/// (not returned to the parent).
#[test]
fn test_storage_call_stipend_burned_on_revert() {
    // Receiver that emits LOG1 then reverts
    let receiver_code = BytecodeBuilder::default()
        .push_number(0xdeadbeef_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(LOG1)
        .revert()
        .build();
    let sender_code = build_transfer_contract(RECEIVER);
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            // Inner CALL reverts, so success_flag = 0
            assert_eq!(success_flag, U256::ZERO, "inner CALL should revert");
        }
        other => panic!("expected outer Success (inner revert), got {other:?}"),
    }
    // The key check: the transaction's total gas_used should be reasonable.
    // The STORAGE_CALL_STIPEND should have been burned, not leaked back to the parent.
    let gas_used = match &result.result {
        ExecutionResult::Success { gas_used, .. } => *gas_used,
        _ => unreachable!(),
    };
    // Gas used should be at least the intrinsic + CALLVALUE cost.
    // If stipend leaked, gas_used would be much lower.
    assert!(gas_used > 21_000 + 9_000, "gas_used should reflect CALLVALUE cost, got {gas_used}");
}

/// `STORAGE_CALL_STIPEND` must NOT be usable for pure computation.
/// A callee that only does compute (PUSH/POP loops, no LOG or storage ops) should still
/// be limited to `CALL_STIPEND` (2,300) worth of compute gas.
/// This is the core reentrancy safety property.
#[test]
fn test_storage_call_stipend_cannot_be_used_for_compute() {
    // Receiver that does ~1000 iterations of PUSH1/POP (pure compute, ~6000 compute gas).
    // This exceeds CALL_STIPEND (2,300) but fits within CALL_STIPEND + STORAGE_CALL_STIPEND
    // (25,300). It MUST fail because the compute gas cap is enforced at CALL_STIPEND (2,300).
    let mut builder = BytecodeBuilder::default();
    for _ in 0..1000 {
        builder = builder.push_number(1_u64).append(POP);
    }
    let receiver_code = builder.append(STOP).build();

    let sender_code = build_transfer_contract(RECEIVER);
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code), (RECEIVER, receiver_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            assert_eq!(
                success_flag,
                U256::ZERO,
                "callee doing pure compute (>2300 gas) should fail even with STORAGE_CALL_STIPEND"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// Value-transferring calls that are intercepted in `frame_init` should still go through
/// the storage-call-stipend bookkeeping path.
/// No EVM frame executes here, but the synthetic result must still be wrapped in an empty
/// tracking frame so return-time stipend burning stays stack-aligned.
#[test]
fn test_storage_call_stipend_applied_on_intercepted_call() {
    let sender_code = build_value_call_with_selector(
        ACCESS_CONTROL_ADDRESS,
        DISABLE_VOLATILE_DATA_ACCESS_SELECTOR,
    );
    let mut db = setup_db(&[(SENDER_CONTRACT, sender_code)]);

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            assert_eq!(
                success_flag,
                U256::ZERO,
                "intercepted CALL with non-zero value should revert via system contract interceptor"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}

/// Top-level keyless deploy interception must preserve the gas already consumed by the
/// interceptor itself.
#[test]
fn test_top_level_keyless_deploy_value_call_preserves_gas_accounting() {
    let call_data = Bytes::from(
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: Bytes::new(),
            gasLimitOverride: U256::ZERO,
        }
        .abi_encode(),
    );
    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(KEYLESS_DEPLOY_ADDRESS)
        .gas_limit(200_000)
        .data(call_data)
        .value(U256::from(1_u64))
        .build_fill();

    let mut rex3_db = setup_db(&[]);
    let rex3_result = transact(MegaSpecId::REX3, &mut rex3_db, tx.clone()).unwrap();
    let rex3_gas_used = match rex3_result.result {
        ExecutionResult::Revert { gas_used, .. } => gas_used,
        other => panic!("expected Revert, got {other:?}"),
    };

    let mut rex4_db = setup_db(&[]);
    let rex4_result = transact(MegaSpecId::REX4, &mut rex4_db, tx).unwrap();
    let rex4_gas_used = match rex4_result.result {
        ExecutionResult::Revert { gas_used, .. } => gas_used,
        other => panic!("expected Revert, got {other:?}"),
    };

    assert_eq!(
        rex4_gas_used, rex3_gas_used,
        "REX4 should preserve keyless-deploy interceptor gas accounting on top-level value calls"
    );
}

/// Nested value-transferring calls: A calls B with value, B calls C with value.
/// Both B and C should independently receive and burn their own `STORAGE_CALL_STIPEND`,
/// exercising the stipend stack push/pop alignment across multiple depth levels.
#[test]
fn test_nested_value_transfer_stipend_stack() {
    // MIDDLE_CONTRACT: emits LOG1, then CALLs RECEIVER with value=1 wei
    let middle_code = BytecodeBuilder::default()
        // Emit LOG1 first (uses storage gas from B's stipend)
        .push_number(0xaaaa_u64) // topic1
        .push_number(0_u64) // size
        .push_number(0_u64) // offset
        .append(LOG1)
        // Then CALL RECEIVER with value=1 (C gets its own stipend)
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value = 1 wei
        .push_address(RECEIVER)
        .push_number(100_000_u64) // gas (enough for the nested call)
        .append(CALL)
        .append(POP) // discard success flag
        .append(STOP)
        .build();

    // RECEIVER: emits LOG1 (uses storage gas from C's stipend)
    let receiver_code = build_log1_receiver();

    // SENDER_CONTRACT: CALLs MIDDLE_CONTRACT with value=1
    let sender_code = BytecodeBuilder::default()
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(1_u64) // value = 1 wei
        .push_address(MIDDLE_CONTRACT)
        .push_number(1_000_000_u64) // gas (enough for nested calls)
        .append(CALL)
        // Return success flag
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();

    let mut db = setup_db(&[
        (SENDER_CONTRACT, sender_code),
        (MIDDLE_CONTRACT, middle_code),
        (RECEIVER, receiver_code),
    ]);
    // MIDDLE_CONTRACT needs balance to forward value to RECEIVER
    db.set_account_balance(MIDDLE_CONTRACT, U256::from(1_000_000_000u128));

    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).unwrap();
    match &result.result {
        ExecutionResult::Success { output, .. } => {
            let success_flag = U256::from_be_slice(&output.data()[..32]);
            assert_eq!(
                success_flag,
                U256::from(1),
                "nested value-transfer calls should both succeed with independent stipends"
            );
        }
        other => panic!("expected Success, got {other:?}"),
    }
}
