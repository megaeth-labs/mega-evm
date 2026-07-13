//! REX6 regression: Oracle hint interceptor consults the volatile-access tracker.
//!
//! `OracleHintInterceptor::intercept` forwards `sendHint` payloads to the off-chain oracle
//! backend as a side effect during `frame_init`, before any interpreter frame runs. Unlike the
//! SLOAD volatile guard (`instructions::sload`), it is not gated on the disabled-subtree
//! tracker (`disableVolatileDataAccess`) on pre-REX6 specs — so a caller could disable volatile
//! access for its own subtree (or a child's) and still leak a hint through a `sendHint` call made
//! inside that disabled subtree. REX6 closes this by consulting the tracker before forwarding —
//! skipping forwarding (not reverting, since the hint is an off-chain side-channel, not consensus
//! state). REX5 and earlier keep forwarding regardless of disabled state, preserving replay
//! determinism on sealed specs.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolCall};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IMegaAccessControl, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, RecordedHint,
    TestExternalEnvs, ACCESS_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX5,
};
use revm::{
    bytecode::opcode::*,
    context::{result::ExecutionResult, tx::TxEnvBuilder},
    inspector::NoOpInspector,
};

sol! {
    function sendHint(bytes32 topic, bytes calldata data) external;
}

const CALLER: Address = address!("0000000000000000000000000000000000600000");
const PARENT: Address = address!("0000000000000000000000000000000000600001");
const CHILD: Address = address!("0000000000000000000000000000000000600002");

const TOPIC: B256 = B256::ZERO;

const DISABLE_SELECTOR: [u8; 4] = IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR;

/// Appends bytecode that calls `disableVolatileDataAccess()` on the access control contract.
fn call_disable_volatile_data_access(builder: BytecodeBuilder) -> BytecodeBuilder {
    let builder = builder.mstore(0x0, DISABLE_SELECTOR);
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(4_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(ACCESS_CONTROL_ADDRESS)
        .push_number(100_000_u64) // gas
        .append(CALL)
        .append(POP) // discard success flag
}

/// Appends bytecode that writes a `sendHint(TOPIC, data_size zero bytes)` payload into memory
/// and CALLs `ORACLE_CONTRACT_ADDRESS` with it, leaving the CALL success flag on the stack.
fn call_send_hint(builder: BytecodeBuilder, data_size: usize, forward_gas: u64) -> BytecodeBuilder {
    let calldata =
        sendHintCall { topic: TOPIC, data: Bytes::from(vec![0u8; data_size]) }.abi_encode();
    let calldata_len = calldata.len();
    let padded_len = calldata_len.div_ceil(32) * 32;
    let mut padded = calldata;
    padded.resize(padded_len, 0);

    let mut builder = builder;
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
        .push_number(forward_gas)
        .append(CALL)
}

/// Appends bytecode that CALLs a target address with the given gas, discarding retdata.
fn append_call(builder: BytecodeBuilder, target: Address, gas: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(0_u64) // value
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

/// Runs a TX calling `PARENT` (with the given code, and optionally `CHILD` with its own code) and
/// returns (`execution_result`, `recorded_hints`).
fn run_with_oracle(
    spec: MegaSpecId,
    parent_code: Bytes,
    child_code: Option<Bytes>,
) -> (ExecutionResult<mega_evm::MegaHaltReason>, Vec<RecordedHint>) {
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_code(PARENT, parent_code)
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX5);
    if let Some(child_code) = child_code {
        db = db.account_code(CHILD, child_code);
    }
    let mut context = MegaContext::new(&mut db, spec).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact ok");
    (envelope.result, external_envs.recorded_hints())
}

/// Like [`run_with_oracle`] but with an explicit TX data-size limit, so a test can observe
/// whether a `sendHint` payload was charged against it.
fn run_with_oracle_and_data_size_limit(
    spec: MegaSpecId,
    parent_code: Bytes,
    data_size_limit: u64,
) -> (ExecutionResult<mega_evm::MegaHaltReason>, Vec<RecordedHint>) {
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_code(PARENT, parent_code)
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX5);
    let mut context = MegaContext::new(&mut db, spec)
        .with_external_envs((&external_envs).into())
        .with_tx_runtime_limits(
            mega_evm::EvmTxRuntimeLimits::from_spec(spec).with_tx_data_size_limit(data_size_limit),
        );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx =
        TxEnvBuilder::default().caller(CALLER).call(PARENT).gas_limit(100_000_000).build_fill();
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact ok");
    (envelope.result, external_envs.recorded_hints())
}

/// REX6: a frame that disabled its own volatile access and then calls `sendHint` in the same
/// frame must NOT have the hint forwarded.
#[test]
fn test_rex6_disabled_frame_blocks_own_send_hint() {
    let code =
        call_send_hint(call_disable_volatile_data_access(BytecodeBuilder::default()), 64, 100_000)
            .stop()
            .build();
    let (result, hints) = run_with_oracle(MegaSpecId::REX6, code, None);
    assert!(result.is_success(), "outer tx must succeed: {result:?}");
    assert!(hints.is_empty(), "sendHint from a volatile-disabled frame must not forward on REX6",);
}

/// REX6 baseline: when volatile access is NOT disabled, `sendHint` still forwards normally — the
/// new guard must not over-block.
#[test]
fn test_rex6_enabled_frame_still_forwards_send_hint() {
    let code = call_send_hint(BytecodeBuilder::default(), 64, 100_000).stop().build();
    let (result, hints) = run_with_oracle(MegaSpecId::REX6, code, None);
    assert!(result.is_success(), "outer tx must succeed: {result:?}");
    assert_eq!(hints.len(), 1, "REX6 sendHint must still forward when volatile access is enabled");
}

/// REX6: PARENT disables volatile access and calls CHILD, which calls `sendHint`. The hint must
/// not forward even though CHILD itself never called `disableVolatileDataAccess` — this is the
/// exact "disabled child subtree" scenario the finding reports (mirrors the existing SLOAD guard's
/// nested-call coverage in `rex4/access_control.rs::test_nested_call_volatile_access_reverts`).
#[test]
fn test_rex6_disabled_by_parent_blocks_child_send_hint() {
    let child_code = call_send_hint(BytecodeBuilder::default(), 64, 100_000).stop().build();
    let parent_code = call_disable_volatile_data_access(BytecodeBuilder::default());
    let parent_code = append_call(parent_code, CHILD, 90_000_000).stop().build();

    let (result, hints) = run_with_oracle(MegaSpecId::REX6, parent_code, Some(child_code));
    assert!(result.is_success(), "outer tx must succeed: {result:?}");
    assert!(
        hints.is_empty(),
        "a child call inside a parent-disabled subtree must not forward sendHint",
    );
}

/// REX6 spec/impl consistency: a disabled-frame `sendHint` call must NOT be charged against the
/// transaction's data-size budget. The volatile-access-disabled check runs before the
/// `record_oracle_hint_bytes` charge, so it groups with the zero-gas-limit and selector-mismatch
/// admission failures (uncharged) — not with a decode failure (charged, per
/// `oracle_hint_metering.rs::test_rex5_malformed_send_hint_is_metered`). Uses a payload large
/// enough that, if metered, would overflow a tight data-size limit; with the fix, the tx must
/// succeed (no halt) precisely because the charge never happens.
#[test]
fn test_rex6_disabled_send_hint_is_not_metered_against_data_size() {
    let payload_size = 4096;
    let limit = 2048; // far below payload_size: would overflow if the call were metered
    let code = call_send_hint(
        call_disable_volatile_data_access(BytecodeBuilder::default()),
        payload_size,
        1_000_000,
    )
    .stop()
    .build();

    let (result, hints) = run_with_oracle_and_data_size_limit(MegaSpecId::REX6, code, limit);

    assert!(
        result.is_success(),
        "a disabled sendHint call must not be charged against data-size, so a tight limit must \
         not halt the tx: {result:?}",
    );
    assert!(hints.is_empty(), "a disabled sendHint call must not forward");
}

/// REX5 replay parity: the guard is gated to REX6+. REX5 is sealed and must keep forwarding
/// `sendHint` regardless of the volatile-access-disabled state — no retroactive behavior change.
#[test]
fn test_rex5_disabled_frame_still_forwards_send_hint() {
    let code =
        call_send_hint(call_disable_volatile_data_access(BytecodeBuilder::default()), 64, 100_000)
            .stop()
            .build();
    let (result, hints) = run_with_oracle(MegaSpecId::REX5, code, None);
    assert!(result.is_success(), "outer tx must succeed: {result:?}");
    assert_eq!(
        hints.len(),
        1,
        "REX5 (sealed) must keep forwarding sendHint regardless of volatile-access-disabled state",
    );
}
