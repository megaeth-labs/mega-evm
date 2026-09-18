#![allow(clippy::doc_markdown)]
//! Regression tests for CREATE2 wrapper safety and compute-gas accounting.
//!
//! 1. CREATE2 with an out-of-bounds initcode slice must never trigger an interpreter panic. The
//!    wrapper expands memory before reading the initcode slice; if memory expansion OOGs, the
//!    canonical reject path runs without ever calling `slice_len` on unallocated memory.
//! 2. The expansion gas consumed by the wrapper's `resize_memory!` must be recorded into the
//!    compute gas tracker. Otherwise the per-tx compute gas limit is undercounted by the initcode
//!    memory-expansion cost, since the canonical CREATE2 path's own `resize_memory!` becomes a
//!    no-op once memory is already sized.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    TestExternalEnvs,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ExecutionResult, TxEnv},
    handler::EvmTr,
    state::{AccountInfo, Bytecode},
};

const CALLER: Address = address!("1111111111111111111111111111111111111111");
const CONTRACT_WITH_BAD_CREATE2: Address = address!("2222222222222222222222222222222222222222");

/// Runtime that performs CREATE2 with a length large enough to OOG the
/// canonical `resize_memory!` under a tight per-call gas budget, but
/// still under `MAX_INITCODE_SIZE` so the size check passes first.
///
/// Stack at CREATE2 (top → bottom): [value, offset, length, salt].
fn memory_oog_create2_runtime() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u8) // salt
        .push_number(500_000u32) // length — large enough to OOG memory expansion
        .push_number(0u8) // offset
        .push_number(0u8) // value
        .append(CREATE2)
        .append(STOP)
        .build()
}

#[test]
fn test_create2_with_oversize_initcode_len_does_not_panic() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(10_000_000_000_000_000_000u128));
    let runtime = memory_oog_create2_runtime();
    let bytecode = Bytecode::new_raw(runtime);
    db.insert_account_info(
        CONTRACT_WITH_BAD_CREATE2,
        AccountInfo { code_hash: bytecode.hash_slow(), code: Some(bytecode), ..Default::default() },
    );

    let envs = TestExternalEnvs::<Infallible>::new();
    let limits = EvmTxRuntimeLimits::no_limits();
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4)
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(limits);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx_env = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CONTRACT_WITH_BAD_CREATE2),
        gas_limit: 200_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());
    let res =
        alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact should not surface EVMError");

    assert!(
        matches!(
            res.result,
            ExecutionResult::Success { .. } |
                ExecutionResult::Halt { .. } |
                ExecutionResult::Revert { .. }
        ),
        "got: {:?}",
        res.result
    );
}

const COMPUTE_GAS_TEST_CONTRACT: Address = address!("3333333333333333333333333333333333333333");

/// Build runtime bytecode that issues a single CREATE2 over a memory window of
/// `initcode_len` bytes starting at offset 0, then STOPs. The initcode memory is left
/// zero-initialized; CREATE2 will deploy an empty contract, which is fine for measuring
/// gas accounting (we only care that the wrapper ran and metered the memory expansion).
///
/// Stack at CREATE2 (top → bottom): [value, offset, length, salt].
fn create2_bytecode(initcode_len: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64) // salt
        .push_number(initcode_len) // length
        .push_number(0u64) // offset
        .push_number(0u64) // value
        .append(CREATE2)
        .append(STOP)
        .build()
}

/// EVM memory expansion gas: `3*words + words*words/512`.
fn memory_expansion_gas(len: u64) -> u64 {
    let words = len.div_ceil(32);
    3 * words + words * words / 512
}

/// Run a single CREATE2 transaction at the REX4 spec and return the recorded compute gas.
fn run_create2_and_get_compute_gas(
    initcode_len: u64,
    gas_limit: u64,
) -> (ExecutionResult<MegaHaltReason>, u64) {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CALLER, U256::from(10_000_000_000_000_000_000u128));
    let bytecode = Bytecode::new_raw(create2_bytecode(initcode_len));
    db.insert_account_info(
        COMPUTE_GAS_TEST_CONTRACT,
        AccountInfo { code_hash: bytecode.hash_slow(), code: Some(bytecode), ..Default::default() },
    );

    let envs = TestExternalEnvs::<Infallible>::new();
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX4)
        .with_external_envs(envs.into())
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let tx_env = TxEnv {
        caller: CALLER,
        kind: TxKind::Call(COMPUTE_GAS_TEST_CONTRACT),
        gas_limit,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());
    let res = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact should not fail");
    let compute_gas = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;
    (res.result, compute_gas)
}
