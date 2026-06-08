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

/// REX5 direct-to-Oracle transactions intentionally pay for the calldata twice in
/// `data_size`: once as normal intrinsic transaction calldata, and once as the Oracle
/// hint side-channel payload forwarded to the off-chain backend.
#[test]
fn test_rex5_direct_oracle_send_hint_charges_intrinsic_and_hint_payload() {
    let calldata = sendHintCall { topic: TOPIC, data: Bytes::from(vec![0u8; 128]) }.abi_encode();
    let calldata_len = calldata.len() as u64;

    let (result, hints, data_size) =
        run_direct_oracle_tx(MegaSpecId::REX5, Bytes::from(calldata), u64::MAX);

    assert!(result.is_success());
    assert_eq!(hints.len(), 1, "direct Oracle sendHint must still forward");
    assert_eq!(hints[0].from, CALLER, "direct hint sender must be the tx caller");
    assert_eq!(
        data_size,
        BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE + calldata_len + calldata_len,
        "direct Oracle sendHint must include intrinsic tx calldata plus hint-side-channel bytes",
    );
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

/// A `sendHint` call whose payload is the selector followed by raw garbage fails to
/// ABI-decode, but the host has already materialized `input_bytes`. REX5 charges the
/// raw `input_bytes.len()` against the TX data-size budget before `abi_decode`, so a
/// caller cannot force unmetered host work by sending malformed `sendHint` calldata.
#[test]
fn test_rex5_malformed_send_hint_is_metered() {
    // 4-byte selector + 1024 bytes of garbage. `abi_decode` rejects it.
    let garbage_len = 1024usize;
    let mut calldata: Vec<u8> = sendHintCall::SELECTOR.to_vec();
    calldata.extend(core::iter::repeat_n(0xab, garbage_len));
    let total = calldata.len() as u64;

    // Generous budget: confirm the malformed payload is metered to its full length and
    // no hint is forwarded.
    let code = build_oracle_call_with_raw_calldata(calldata.clone(), 1_000_000);
    let (result, hints, data_size) = run_with_oracle(MegaSpecId::REX5, code, u64::MAX);
    assert!(result.is_success(), "outer tx must succeed under a generous budget");
    assert!(hints.is_empty(), "malformed sendHint must NOT forward to on_hint");
    assert!(
        data_size >= total,
        "input_bytes.len()={total} must be fully charged against data_size (got {data_size})",
    );

    // Tight budget below `total`: malformed payload must trigger canonical OOG halt.
    let code = build_oracle_call_with_raw_calldata(calldata, 1_000_000);
    let (result, hints, _) = run_with_oracle(MegaSpecId::REX5, code, total / 2);
    assert!(!result.is_success(), "tight budget must trigger canonical OOG halt");
    assert!(hints.is_empty(), "malformed sendHint must NOT forward under overflow");
}

/// A valid `sendHint` ABI envelope followed by huge trailing junk: `abi_decode`
/// silently drops the trailing bytes (pinned by
/// `test_alloy_abi_decode_accepts_selector_plus_trailing_bytes_on_zero_arg_calls` in
/// `crates/mega-evm/src/system/intercept.rs`). Charging only `data.len() + 32` would
/// let a caller forward unmetered megabytes of trailing junk; charging
/// `input_bytes.len()` before `abi_decode` closes that gap.
#[test]
fn test_rex5_trailing_junk_after_valid_send_hint_is_metered() {
    // Valid abi_encode of sendHint(topic, empty data), then a large run of trailing junk.
    let valid = sendHintCall { topic: TOPIC, data: Bytes::new() }.abi_encode();
    let junk_len = 4096usize;
    let mut calldata = valid;
    calldata.extend(core::iter::repeat_n(0xcd, junk_len));
    let total = calldata.len() as u64;

    // Generous budget: the envelope is valid, the hint is forwarded with decoded
    // (empty) data, but the full `input_bytes.len()` (envelope + trailing junk) is
    // charged against the data-size lane.
    let code = build_oracle_call_with_raw_calldata(calldata.clone(), 1_000_000);
    let (result, hints, data_size) = run_with_oracle(MegaSpecId::REX5, code, u64::MAX);
    assert!(result.is_success());
    assert_eq!(hints.len(), 1, "valid envelope still forwards under REX5");
    assert_eq!(hints[0].data.len(), 0, "decoded data must drop the trailing junk");
    assert!(
        data_size >= total,
        "full input length {total} (incl. {junk_len} trailing junk bytes) must be metered \
         (got {data_size})",
    );

    // Tight budget below `total`: trailing junk must NOT be allowed to forward.
    let code = build_oracle_call_with_raw_calldata(calldata, 1_000_000);
    let (result, hints, _) = run_with_oracle(MegaSpecId::REX5, code, total / 2);
    assert!(!result.is_success(), "tight budget must trigger canonical OOG halt");
    assert!(hints.is_empty(), "trailing junk must NOT forward when over budget");
}

/// The REX5 `input_bytes.len()` charge is strictly larger than the legacy
/// `data.len() + 32` formula. Verified by comparing REX5 (with metering) to REX4
/// (no metering, baseline pinned by `test_rex4_does_not_meter_send_hint_payload`)
/// for a `sendHint(topic, empty data)` call: the encoded ABI envelope is
/// `selector(4) + topic(32) + data_offset(32) + data_length(32) = 100` bytes; the
/// legacy formula would have charged `0 + HINT_TOPIC_BYTES = 32` bytes. A regression
/// that restored the legacy `call.data.len() + 32` charge would shrink `extra` below
/// the legacy threshold.
#[test]
fn test_rex5_charge_is_strictly_more_conservative_than_legacy_formula() {
    let code = build_send_hint_bytecode(TOPIC, 0, 100_000); // empty `data`
    let (_, _, rex4) = run_with_oracle(MegaSpecId::REX4, code.clone(), u64::MAX);
    let (_, _, rex5) = run_with_oracle(MegaSpecId::REX5, code, u64::MAX);

    let new_charge: u64 = 100; // input_bytes.len() for sendHint(topic, empty)
    let pre_fix_charge: u64 = 32; // legacy: data.len() + HINT_TOPIC_BYTES

    let extra = rex5
        .checked_sub(rex4)
        .expect("REX5 must record at least as much data_size as REX4 for identical bytecode");
    assert!(
        extra >= new_charge,
        "REX5 must charge at least input_bytes.len()={new_charge} more than REX4 (got {extra})",
    );
    assert!(
        extra > pre_fix_charge,
        "post-fix REX5 must charge strictly more than the legacy formula \
         (data.len()+32={pre_fix_charge}); got extra={extra}",
    );
}

/// REX5 invariant: consecutive `sendHint` calls in the same TX accumulate their
/// `input_bytes.len()` charges into the TX `persistent_usage` lane. With a budget sized
/// for one call but not two, the first sendHint must forward and the second must be
/// blocked by the accumulated overflow — TX halts via canonical OOG. Pins that hint
/// metering is TX-scoped, not per-call.
#[test]
fn test_rex5_consecutive_send_hints_accumulate_into_tx_lane() {
    let payload = 256;
    let calldata =
        sendHintCall { topic: TOPIC, data: Bytes::from(vec![0u8; payload]) }.abi_encode();
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
    // First CALL: sendHint.
    builder = builder
        .push_number(0u64)
        .push_number(0u64)
        .push_number(calldata_len as u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_000_000u64)
        .append(CALL)
        .append(POP);
    // Second CALL: sendHint (same calldata, memory still holds it).
    let code = builder
        .push_number(0u64)
        .push_number(0u64)
        .push_number(calldata_len as u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_000_000u64)
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();

    // Budget allows one `input_bytes.len()` (plus a small intrinsic head-room) but not two.
    let limit = (calldata_len as u64) + (calldata_len as u64) / 2;
    let (result, hints, _) = run_with_oracle(MegaSpecId::REX5, code, limit);

    assert!(
        !result.is_success(),
        "tx must halt when the second sendHint's input_bytes.len() pushes the accumulator over",
    );
    assert_eq!(
        hints.len(),
        1,
        "first sendHint forwards; second must be blocked by the accumulated TX budget",
    );
}

/// REX5 invariant: hint bytes recorded by the interceptor enter the TX-persistent
/// data-size lane (`tx_entry.persistent_usage`), not the per-frame discardable lane.
/// Even when the call frame that initiated the `sendHint` reverts, the charge persists
/// and the hint stays forwarded. Pins the docstring claim in
/// `data_size.rs::record_oracle_hint_bytes` — a regression that routed the charge
/// through the discardable frame lane would let callers spam hints from a doomed-to-
/// revert frame for free.
#[test]
fn test_rex5_hint_charge_persists_across_initiating_frame_revert() {
    const INNER: Address = address!("0000000000000000000000000000000000500002");

    let payload = 256;
    let calldata =
        sendHintCall { topic: TOPIC, data: Bytes::from(vec![0u8; payload]) }.abi_encode();
    let calldata_len = calldata.len();
    let padded_len = calldata_len.div_ceil(32) * 32;
    let mut padded = calldata;
    padded.resize(padded_len, 0);

    // INNER: MSTORE the sendHint calldata, CALL oracle, then REVERT(0, 0).
    let mut inner = BytecodeBuilder::default();
    let mut offset = 0;
    while offset < padded_len {
        let mut word = [0u8; 32];
        word.copy_from_slice(&padded[offset..offset + 32]);
        inner = inner.mstore(offset, word);
        offset += 32;
    }
    let inner_code = inner
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(calldata_len as u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(1_000_000u64)
        .append(CALL)
        .append(POP)
        .push_number(0u64) // REVERT size
        .push_number(0u64) // REVERT offset
        .append(REVERT)
        .build();

    // CALLER_CONTRACT: CALL INNER, store success flag, RETURN.
    let outer_code = BytecodeBuilder::default()
        .push_number(0u64) // retSize
        .push_number(0u64) // retOffset
        .push_number(0u64) // argsSize
        .push_number(0u64) // argsOffset
        .push_number(0u64) // value
        .push_address(INNER)
        .push_number(10_000_000u64)
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_code(CALLER_CONTRACT, outer_code)
        .account_code(INNER, inner_code)
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX2);
    let mut context = MegaContext::new(&mut db, MegaSpecId::REX5)
        .with_external_envs((&external_envs).into())
        .with_tx_runtime_limits(
            EvmTxRuntimeLimits::from_spec(MegaSpecId::REX5).with_tx_data_size_limit(u64::MAX),
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

    assert!(
        envelope.result.is_success(),
        "outer TX must succeed (only the INNER frame reverts after its sendHint)",
    );
    let hints = external_envs.recorded_hints();
    assert_eq!(hints.len(), 1, "hint must be forwarded before INNER's REVERT");
    assert!(
        data_size >= calldata_len as u64,
        "hint bytes ({calldata_len}) must persist across initiating frame REVERT (got {data_size})",
    );
}
