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

#[test]
fn test_create2_memory_expansion_recorded_as_compute_gas() {
    // 32000-byte initcode -> 1000 words -> ~4953 gas of memory expansion.
    const INITCODE_LEN: u64 = 32_000;
    let words = INITCODE_LEN.div_ceil(32);
    let expected_expansion = memory_expansion_gas(INITCODE_LEN);
    // KECCAK256 over the initcode (charged inside CREATE2).
    let expected_hash = 6 * words;
    // CREATE2 per-initcode-word cost (part of CREATE2's static charge).
    let expected_init_word = 2 * words;
    // Sum of all three initcode_len-scaled costs. The expansion cost is what the wrapper must
    // record into the compute gas tracker.
    let expected_extra = expected_expansion + expected_hash + expected_init_word;

    let (baseline_result, baseline_compute_gas) = run_create2_and_get_compute_gas(0, 10_000_000);
    assert!(
        matches!(baseline_result, ExecutionResult::Success { .. }),
        "baseline CREATE2 with empty initcode should succeed: {:?}",
        baseline_result
    );

    let (big_result, big_compute_gas) = run_create2_and_get_compute_gas(INITCODE_LEN, 10_000_000);
    assert!(
        matches!(big_result, ExecutionResult::Success { .. }),
        "large-initcode CREATE2 should succeed: {:?}",
        big_result
    );

    // The delta should equal the sum of all three initcode-length-scaled costs (within a
    // small tolerance for the differing PUSH opcode used to push the length itself: PUSH0
    // for 0 vs PUSHn for 32000). If the wrapper failed to record the expansion gas, the
    // delta would be short by `expected_expansion`.
    let delta = big_compute_gas - baseline_compute_gas;
    let diff = delta.abs_diff(expected_extra);
    assert!(
        diff < 50,
        "compute gas delta ({}) should ~equal expansion ({}) + hash ({}) + init_word ({}) = {}; \
         baseline={}, big={}, diff={}. If diff is close to {}, the wrapper's resize_memory! \
         consumed interpreter gas but never recorded it into the compute gas tracker.",
        delta,
        expected_expansion,
        expected_hash,
        expected_init_word,
        expected_extra,
        baseline_compute_gas,
        big_compute_gas,
        diff,
        expected_expansion,
    );
}

#[test]
fn test_create2_resize_gas_skipped_when_canonical_create2_oogs() {
    // Verifies the deferred-recording semantics: when the inner CREATE2 OOGs (after the
    // wrapper's resize_memory! has already consumed interpreter gas), the compute gas
    // tracker must NOT receive the resize_gas. This mirrors `wrap_op_compute_gas`'s
    // "skip on inner error" pattern — historically the canonical CREATE2's expansion gas
    // was only recorded when the inner instruction completed without an EVM error.
    //
    // Strategy: pick a tx gas budget tight enough that, after intrinsic + bytecode + the
    // wrapper's resize, the remaining gas cannot cover the canonical CREATE2's static
    // base cost (32_000). The two runs (small vs big initcode) both halt with OutOfGas.
    // With deferred recording: compute gas delta ≈ 1 (just the differing PUSH opcode for
    // the length argument). With (hypothetical) immediate recording: delta would be
    // ≈ 1 + expected_expansion.
    const INITCODE_LEN: u64 = 32_000;
    let expected_expansion = memory_expansion_gas(INITCODE_LEN);

    // REX4 intrinsic = 21_000 (base) + 39_000 (REX TX_INTRINSIC_STORAGE_GAS) = 60_000.
    // After intrinsic, leave 31_000 for the frame: PUSHes (~9) + (for big run) resize
    // (~4953) + canonical CREATE2 base (32_000+). Small run has ~30_992 for canonical's
    // 32_000 base → OOG; big run has ~26_038 for canonical's 40_000 (32k base + 2k init
    // + 6k keccak) → also OOG. Both halt inside canonical CREATE2.
    let tight_gas: u64 = 91_000;

    let (small_result, small_compute_gas) = run_create2_and_get_compute_gas(0, tight_gas);
    let (big_result, big_compute_gas) = run_create2_and_get_compute_gas(INITCODE_LEN, tight_gas);

    // Both must OOG (otherwise the test premise is broken and the assertion below is
    // not actually exercising the deferred-recording path).
    assert!(
        matches!(small_result, ExecutionResult::Halt { .. }),
        "small-initcode run should OOG inside canonical CREATE2 at tight_gas={}: {:?}",
        tight_gas,
        small_result
    );
    assert!(
        matches!(big_result, ExecutionResult::Halt { .. }),
        "big-initcode run should OOG inside canonical CREATE2 at tight_gas={}: {:?}",
        tight_gas,
        big_result
    );

    // The only legitimate compute-gas difference between the two runs is the length-PUSH
    // opcode (PUSH0 vs PUSHn, ~1 gas). If the wrapper recorded resize_gas eagerly, the
    // big-initcode run would also be charged ~`expected_expansion` extra, putting the
    // delta near 4953 instead of near 0.
    let delta = big_compute_gas.abs_diff(small_compute_gas);
    assert!(
        delta < 50,
        "compute gas delta on OOG ({}) should be ~0; got small={}, big={}. If delta is \
         close to expected_expansion ({}), the resize gas leaked into the tracker even \
         though the inner CREATE2 errored — the deferred-recording skip path is broken.",
        delta,
        small_compute_gas,
        big_compute_gas,
        expected_expansion,
    );
}
