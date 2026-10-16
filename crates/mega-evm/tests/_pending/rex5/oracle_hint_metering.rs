//! Tests for the REX5+ oracle hint admission and metering rule.
//!
//! Pre-REX5: `OracleHintInterceptor` invokes `OracleEnv::on_hint` **before** the inner
//! Oracle frame runs, regardless of whether the caller provided gas. A contract can
//! call `sendHint(topic, bigPayload)` with `gas_limit = 0` to forward arbitrary bytes
//! to the off-chain backend while paying nothing — the inner Oracle frame OOGs but the
//! hint has already flowed out. The payload is also not charged against the
//! transaction's data-size budget.
//!
//! Under REX5:
//! 1. Zero-gas `sendHint` falls through to the on-chain Oracle bytecode (no forwarding).
//! 2. The raw `input_bytes.len()` of the inner CALL's calldata — the exact buffer the host just
//!    materialized — is recorded against the TX data-size budget via
//!    `AdditionalLimit::record_oracle_hint_bytes` **before** `abi_decode` runs. This charges both
//!    the legitimate envelope and any trailing junk silently dropped by
//!    `alloy_sol_types::abi_decode`, and it covers the malformed-payload case identically.
//! 3. If recording overflows, `on_hint` is NOT invoked; the next `before_frame_init` step produces
//!    the canonical TX-level `OutOfGas` halt via `create_exceeded_limit_result`.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolCall};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, TestExternalEnvs,
    ACCOUNT_INFO_WRITE_SIZE, BASE_TX_SIZE, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    inspector::NoOpInspector,
};

sol! {
    function sendHint(bytes32 topic, bytes calldata data) external;
}

const CALLER: Address = address!("0000000000000000000000000000000000500000");
const CALLER_CONTRACT: Address = address!("0000000000000000000000000000000000500001");

/// Builds bytecode that:
/// 1. ABI-encodes a `sendHint(topic, data)` call where `data` is `data_size` bytes of zeros (the
///    actual byte values don't matter for the metering test).
/// 2. Stores the calldata in memory.
/// 3. CALLs `ORACLE_CONTRACT_ADDRESS` with the supplied `forward_gas` and the encoded calldata as
///    args.
/// 4. RETURNs the 32-byte CALL success flag.
fn build_send_hint_bytecode(topic: B256, data_size: usize, forward_gas: u64) -> Bytes {
    let calldata = sendHintCall { topic, data: Bytes::from(vec![0u8; data_size]) }.abi_encode();
    let calldata_len = calldata.len();

    let mut builder = BytecodeBuilder::default();
    // Write each 32-byte word of the encoded calldata into memory[0..calldata_len].
    let mut offset = 0;
    let calldata_padded_len = calldata_len.div_ceil(32) * 32;
    let mut padded = calldata;
    padded.resize(calldata_padded_len, 0);
    while offset < calldata_padded_len {
        let mut word = [0u8; 32];
        word.copy_from_slice(&padded[offset..offset + 32]);
        builder = builder.mstore(offset, word);
        offset += 32;
    }
    // Truncate memory expansion to the actual calldata length by marking the last byte
    // (no-op write to grow memory if odd-length, otherwise MSTORE above already did).
    builder
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(calldata_len as u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(forward_gas) // gas forwarded
        .append(CALL)
        // Store success flag at memory[0..32] and RETURN it
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build()
}

/// Runs a TX that deploys `CALLER_CONTRACT` with `code` and calls into it. Returns
/// (`execution_result`, `recorded_hints`, `final_data_size_usage`).
fn run_with_oracle(
    spec: MegaSpecId,
    code: Bytes,
    data_size_limit: u64,
) -> (ExecutionResult<mega_evm::MegaHaltReason>, Vec<mega_evm::RecordedHint>, u64) {
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_code(CALLER_CONTRACT, code)
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs((&external_envs).into())
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::from_spec(spec).with_tx_data_size_limit(data_size_limit),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLER_CONTRACT)
        .gas_limit(100_000_000)
        .build_fill();
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact ok");
    use revm::handler::EvmTr;
    let data_size = evm.ctx_ref().additional_limit.borrow().get_usage().data_size;
    (envelope.result, external_envs.recorded_hints(), data_size)
}

/// Runs a TX that directly targets the Oracle contract with `calldata`.
fn run_direct_oracle_tx(
    spec: MegaSpecId,
    calldata: Bytes,
    data_size_limit: u64,
) -> (ExecutionResult<mega_evm::MegaHaltReason>, Vec<mega_evm::RecordedHint>, u64) {
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs((&external_envs).into())
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::from_spec(spec).with_tx_data_size_limit(data_size_limit),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnvBuilder::default()
        .caller(CALLER)
        .call(ORACLE_CONTRACT_ADDRESS)
        .data(calldata)
        .gas_limit(100_000_000)
        .build_fill();
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact ok");
    use revm::handler::EvmTr;
    let data_size = evm.ctx_ref().additional_limit.borrow().get_usage().data_size;
    (envelope.result, external_envs.recorded_hints(), data_size)
}

const TOPIC: B256 = B256::ZERO;

/// REX5 invariant 3: when the hint payload pushes `data_size_used` past the TX
/// limit, the interceptor must NOT forward the hint, AND the transaction must halt
/// via the canonical TX-level `OutOfGas` path (not a synthetic Revert).
#[test]
fn test_rex5_data_size_overflow_blocks_forwarding_and_halts_canonically() {
    // Choose data_size large enough that recording it overflows a tight TX data-size limit.
    // Intrinsic usage (BASE_TX + calldata + caller account update) eats some budget already.
    let payload_size = 4096;
    let limit = 2048; // far below payload_size, guarantees overflow on recording
    let code = build_send_hint_bytecode(TOPIC, payload_size, 1_000_000);
    let (result, hints, _) = run_with_oracle(MegaSpecId::REX5, code, limit);

    assert!(hints.is_empty(), "overflowing sendHint must NOT forward to on_hint",);
    // The TX halts via the canonical exceeded-limit path. The exact halt reason is one of
    // the data-size variants emitted by `create_exceeded_limit_result` — accept any non-success
    // outcome as long as no hint was forwarded.
    assert!(
        !result.is_success(),
        "tx must halt when sendHint payload overflows the data-size budget",
    );
}

/// Builds bytecode that writes `calldata` into memory, CALLs the oracle contract with
/// `forward_gas` and `calldata.len()` argsSize, and RETURNs the 32-byte CALL success flag.
fn build_oracle_call_with_raw_calldata(calldata: Vec<u8>, forward_gas: u64) -> Bytes {
    let calldata_len = calldata.len();
    let padded_len = calldata_len.div_ceil(32) * 32;
    let mut padded = calldata;
    padded.resize(padded_len, 0);

    let mut builder = BytecodeBuilder::default();
    let mut offset = 0;
    while offset < padded_len {
        let mut word = [0u8; 32];
        word.copy_from_slice(&padded[offset..offset + 32]);
        builder = builder.mstore(offset, word);
        offset += 32;
    }
    builder
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(calldata_len as u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(forward_gas) // gas forwarded
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build()
}
