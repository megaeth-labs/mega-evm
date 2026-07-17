//! Coverage tests for handwritten compute-gas opcode wrappers in `evm/instructions.rs`.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EVMError, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{ExecutionResult, ResultAndState},
        tx::TxEnvBuilder,
    },
};

const CALLER: Address = address!("0000000000000000000000000000000000200000");
const CONTRACT: Address = address!("0000000000000000000000000000000000200001");
const TARGET: Address = address!("0000000000000000000000000000000000200002");

fn transact_code(
    db: &mut MemoryDatabase,
    target: Address,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>> {
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);
    let tx = TxEnvBuilder::new().caller(CALLER).call(target).gas_limit(5_000_000).build_fill();
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

fn assert_halt(result: &ExecutionResult<MegaHaltReason>) {
    assert!(result.is_halt(), "expected halt, got {result:?}");
}

fn build_log_contract(topic_count: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default()
        .push_bytes(0xdead_beef_u32.to_be_bytes())
        .push_number(0u8)
        .append(MSTORE);

    for i in 0..topic_count {
        builder = builder.push_number((0x10 + i) as u8);
    }

    builder
        .push_number(4u8)
        .push_number(0x1c_u8)
        .append(LOG0 + topic_count as u8)
        .append(STOP)
        .build()
}

fn build_zero_length_log_contract(topic_count: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for i in 0..topic_count {
        builder = builder.push_number((0x20 + i) as u8);
    }

    builder.push_number(0u8).push_number(0u8).append(LOG0 + topic_count as u8).append(STOP).build()
}

#[test]
fn test_push1_add_pop_success_paths_execute() {
    let bytecode = BytecodeBuilder::default()
        .append(PUSH1)
        .append(1)
        .append(PUSH1)
        .append(2)
        .append(ADD)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let result = transact_code(&mut db, CONTRACT).unwrap();
    assert!(result.result.is_success(), "expected success, got {:?}", result.result);
}

#[test]
fn test_add_stack_underflow_halts() {
    let bytecode =
        BytecodeBuilder::default().append(PUSH1).append(1).append(ADD).append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let result = transact_code(&mut db, CONTRACT).unwrap();
    assert_halt(&result.result);
}

#[test]
fn test_pop_stack_underflow_halts() {
    let bytecode = BytecodeBuilder::default().append(POP).append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let result = transact_code(&mut db, CONTRACT).unwrap();
    assert_halt(&result.result);
}

#[test]
fn test_push1_stack_overflow_halts() {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..1024 {
        builder = builder.append(PUSH0);
    }
    let bytecode = builder.append(PUSH1).append(1).append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let result = transact_code(&mut db, CONTRACT).unwrap();
    assert_halt(&result.result);
}

#[test]
fn test_log_variants_emit_expected_topic_counts() {
    for topic_count in 0..=4 {
        let mut db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(1_000_000))
            .account_code(CONTRACT, build_log_contract(topic_count));

        let result = transact_code(&mut db, CONTRACT).unwrap();
        assert!(
            result.result.is_success(),
            "expected success for LOG{topic_count}, got {:?}",
            result.result
        );

        let logs = result.result.logs();
        assert_eq!(logs.len(), 1, "LOG{topic_count} should emit exactly one log");
        let log = &logs[0];
        assert_eq!(log.address, CONTRACT, "LOG{topic_count} emitter mismatch");
        assert_eq!(log.data.topics().len(), topic_count, "LOG{topic_count} topic count mismatch",);
        assert_eq!(
            log.data.data.as_ref(),
            &0xdead_beef_u32.to_be_bytes(),
            "LOG{topic_count} data mismatch",
        );
    }
}

#[test]
fn test_log_zero_length_uses_empty_data_branch() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, build_zero_length_log_contract(1));

    let result = transact_code(&mut db, CONTRACT).unwrap();
    assert!(result.result.is_success(), "expected success, got {:?}", result.result);

    let logs = result.result.logs();
    assert_eq!(logs.len(), 1, "expected exactly one zero-length log");
    assert_eq!(logs[0].data.data.as_ref(), &[0u8; 0], "zero-length log should have empty data",);
    assert_eq!(logs[0].data.topics().len(), 1, "LOG1 should retain its topic");
}

#[test]
fn test_log_stack_underflow_halts() {
    let bytecode = BytecodeBuilder::default().append(PUSH0).append(LOG1).append(STOP).build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let result = transact_code(&mut db, CONTRACT).unwrap();
    assert_halt(&result.result);
}

#[test]
fn test_log_topic_underflow_halts_after_offset_and_len_are_present() {
    let bytecode = BytecodeBuilder::default()
        .push_number(0u8)
        .push_number(0u8)
        .append(LOG1)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, bytecode);

    let result = transact_code(&mut db, CONTRACT).unwrap();
    assert_halt(&result.result);
}

#[test]
fn test_log_staticcall_halts_without_emitting_logs() {
    let target_code = build_log_contract(0);
    let caller_code = BytecodeBuilder::default()
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_number(0u8)
        .push_address(TARGET)
        .push_number(100_000u32)
        .append(STATICCALL)
        .append(POP)
        .append(STOP)
        .build();

    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(CONTRACT, caller_code)
        .account_code(TARGET, target_code);

    let result = transact_code(&mut db, CONTRACT).unwrap();
    assert!(result.result.is_success(), "parent STATICCALL wrapper should succeed");
    assert!(result.result.logs().is_empty(), "static LOG must not emit logs");
}
