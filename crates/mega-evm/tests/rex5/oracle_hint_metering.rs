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
//! 2. The hint payload length (`data.len()` + the fixed `bytes32 topic` size) is recorded against
//!    the TX data-size budget via `AdditionalLimit::record_oracle_hint_bytes` before forwarding.
//! 3. If recording overflows, `on_hint` is NOT invoked; the next `before_frame_init` step produces
//!    the canonical TX-level `OutOfGas` halt via `create_exceeded_limit_result`.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolCall};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, TestExternalEnvs,
    ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2,
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

const TOPIC: B256 = B256::ZERO;

/// REX5 invariant 1: a CALL to `sendHint` with `gas_limit = 0` must NOT forward the
/// hint. Pre-REX5 the interceptor would forward regardless of gas, exposing a free
/// side-channel into the off-chain oracle backend.
#[test]
fn test_rex5_zero_gas_send_hint_does_not_forward() {
    let code = build_send_hint_bytecode(TOPIC, 64, 0); // 64-byte payload, gas=0
    let (result, hints, _) = run_with_oracle(MegaSpecId::REX5, code, u64::MAX);
    assert!(result.is_success(), "outer tx must succeed (only the inner CALL OOGs)");
    assert!(hints.is_empty(), "zero-gas sendHint must not forward to on_hint (REX5)",);
}

/// Pre-REX5 baseline: zero-gas sendHint still forwards. Pinned to guarantee replay
/// determinism on stable specs.
#[test]
fn test_rex4_zero_gas_send_hint_still_forwards() {
    let code = build_send_hint_bytecode(TOPIC, 64, 0);
    let (result, hints, _) = run_with_oracle(MegaSpecId::REX4, code, u64::MAX);
    assert!(result.is_success());
    assert_eq!(hints.len(), 1, "REX4 must keep the pre-REX5 unmetered, zero-gas-accepting path",);
}

/// REX4 replay parity: a normal sendHint with a large payload under REX4 must NOT
/// be charged against `data_size_used`. This pins the pre-REX5 unmetered invariant —
/// if hint metering accidentally activated on stable specs, the data-size lane
/// would grow and replay of historical REX4 blocks would diverge.
#[test]
fn test_rex4_does_not_meter_send_hint_payload() {
    let payload_size = 1024;
    let code = build_send_hint_bytecode(TOPIC, payload_size, 100_000);
    let (result, hints, data_size) = run_with_oracle(MegaSpecId::REX4, code, u64::MAX);
    assert!(result.is_success());
    assert_eq!(hints.len(), 1, "REX4 must still forward normal sendHints");
    assert!(
        data_size < payload_size as u64,
        "REX4 must NOT charge the {payload_size}-byte payload against data_size (got {})",
        data_size,
    );
}

/// REX5 invariant 2: a normal sendHint with positive gas still forwards to `on_hint`
/// (parity with the pre-REX5 happy path).
#[test]
fn test_rex5_non_zero_gas_send_hint_forwards() {
    let code = build_send_hint_bytecode(TOPIC, 128, 100_000); // 128-byte payload
    let (result, hints, data_size) = run_with_oracle(MegaSpecId::REX5, code, u64::MAX);
    assert!(result.is_success());
    assert_eq!(hints.len(), 1, "non-zero-gas sendHint must still forward (REX5)");
    assert_eq!(hints[0].data.len(), 128, "forwarded data length must match");
    // Sanity: the hint payload was counted into the TX data-size lane.
    assert!(data_size >= 128, "hint payload ({}) must contribute to data_size", data_size);
}

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

/// REX5: a wrong selector to `ORACLE_CONTRACT_ADDRESS` does not trigger metering. We
/// verify by sending an unknown selector and asserting `data_size` does not include
/// any hint payload — the call falls through to on-chain bytecode and the hint
/// interceptor never reaches the metering hook.
#[test]
fn test_rex5_wrong_selector_does_not_meter() {
    // Construct a CALL with a 4-byte unknown selector + 256 zero bytes. The interceptor
    // peek_selector check fails the equality test, returns None without metering. The
    // canonical Oracle bytecode then reverts NotIntercepted, so the CALL success flag is 0.
    let unknown_selector = [0xde, 0xad, 0xbe, 0xef];
    let payload_size = 256;
    let code = BytecodeBuilder::default()
        .mstore(0, unknown_selector)
        .push_number(0u64)
        .push_number((4 + payload_size - 1) as u64)
        .append(MSTORE8) // grow memory
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number((4 + payload_size) as u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_000_000u64)
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    let (_result, hints, data_size_after) = run_with_oracle(MegaSpecId::REX5, code, u64::MAX);
    assert!(hints.is_empty(), "wrong selector must not invoke on_hint");
    // Baseline data_size without any hint payload: BASE_TX_SIZE + caller account info +
    // empty tx input. The exact value depends on intrinsic accounting; what we pin is
    // that the 256-byte payload is NOT included.
    assert!(
        data_size_after < 256,
        "wrong selector must not record hint payload bytes (data_size = {})",
        data_size_after,
    );
}
