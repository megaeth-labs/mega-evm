//! Regression suite for the REX5+ `KeylessDeploy` empty-code log preservation.
//!
//! When a `KeylessDeploy` sandbox CREATE succeeds with empty runtime bytecode,
//! `process_sandbox_transact_result` used to bucket the outcome with revert /
//! halt failures and drop the constructor's emitted logs. Sandbox state (signer
//! nonce bump, journal writes) still persisted, so the parent receipt's logs
//! disagreed with the on-chain state.
//!
//! REX5+ now routes this through `SandboxCompletion::EmptyCode { logs, .. }`,
//! which the outer `execute_keyless_deploy_call` forwards via `ctx.log(log)`
//! before returning success-style with `EmptyCodeDeployed` errorData. The
//! Solidity ABI shape `EmptyCodeDeployed(uint64 gasUsed)` is intentionally
//! unchanged — only the in-band log forwarding is fixed.
//!
//! Pre-REX5 collapses empty-code into
//! `SandboxCompletion::ExecutionFailed { error: EmptyCodeDeployed, .. }` so
//! logs are dropped, preserving the frozen replay behavior.

use std::vec::Vec;

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::ExecutionResult,
    sandbox::{calculate_keyless_deploy_address, decode_error_result, KeylessDeployError},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    bytecode::opcode::{LOG1, MSTORE, RETURN, STOP},
    context::TxEnv,
    inspector::NoOpInspector,
};

const RELAYER: Address = address!("0000000000000000000000000000000000990000");
const SIGNED_GAS_PRICE: u128 = 100_000_000_000;
const SIGNED_GAS_LIMIT: u64 = 100_000;
const OUTER_GAS_LIMIT: u64 = 30_000_000;
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;

/// Constructor that emits a single `LOG1` with a fixed topic and `data = [0xde,
/// 0xad, 0xbe, 0xef]` then RETURNs empty runtime bytecode. The empty RETURN
/// triggers the `EmptyCodeDeployed` reclassification inside the sandbox.
///
/// Layout (annotated PUSH-immediates so the offsets are auditable):
///
/// ```text
///   PUSH4 0xdeadbeef                   // log data word
///   PUSH1 0x00                         // memory offset (store at slot 0)
///   MSTORE                             // mem[0..32] = 0x...deadbeef (right-aligned in word)
///
///   PUSH32 <topic>                     // topic for LOG1
///   PUSH1 0x04                         // log data size (4 bytes)
///   PUSH1 0x1c                         // log data offset (32 - 4 = 28)
///   LOG1
///
///   PUSH1 0x00                         // RETURN size = 0 (empty runtime code)
///   PUSH1 0x00                         // RETURN offset = 0
///   RETURN
/// ```
fn build_log1_then_empty_return_init_code(topic: B256) -> Bytes {
    let log_data_word: u32 = 0xdead_beef;
    BytecodeBuilder::default()
        // mem[0..32] = log data, right-aligned in the 32-byte word
        .push_bytes(log_data_word.to_be_bytes())
        .push_number(0_u8)
        .append(MSTORE)
        // LOG1(offset=28, size=4, topic)
        .push_bytes(topic.as_slice())
        .push_number(4_u8)
        .push_number(0x1c_u8)
        .append(LOG1)
        // RETURN(offset=0, size=0)
        .push_number(0_u8)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// Constructor that emits no logs and RETURNs empty runtime bytecode. Used to
/// confirm the `SandboxCompletion::EmptyCode` wiring does not synthesize spurious
/// logs.
fn build_no_logs_empty_return_init_code() -> Bytes {
    BytecodeBuilder::default()
        .push_number(0_u8) // RETURN size
        .push_number(0_u8) // RETURN offset
        .append(RETURN)
        .append(STOP) // unreachable; defensive
        .build()
}

/// Builds a deterministic pre-EIP-155 keyless tx with the given init code.
/// Copy of `keyless_fee_free.rs`'s helper — the keyless test files are
/// intentionally self-contained.
fn build_keyless_tx_with_init_code(init_code: Bytes) -> (Bytes, Address) {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: SIGNED_GAS_PRICE,
        gas_limit: SIGNED_GAS_LIMIT,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
        chain_id: None,
    };

    let r = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let s = U256::from_be_bytes(hex!(
        "2222222222222222222222222222222222222222222222222222222222222222"
    ));
    let sig = Signature::new(r, s, false);
    let signed = Signed::new_unchecked(tx, sig, B256::ZERO);

    let mut buf = Vec::new();
    signed.rlp_encode(&mut buf);
    let tx_bytes = Bytes::from(buf);
    let signer = signed.recover_signer().expect("should recover signer");
    (tx_bytes, signer)
}

/// Runs the outer `keylessDeployCall` and returns the parent `ExecutionResult`.
///
/// `gas_limit_override` matters under pre-REX5: the sandbox runs as a normal
/// fee-paying tx and debits `gas_limit_override × signed_gas_price` from the
/// signer at pre-execution. REX5 makes the sandbox fee-free, so the override
/// can be arbitrarily large there. Tests that exercise REX4 need a modest
/// override so the funded signer balance covers the upfront debit.
fn run_keyless_outer(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    keyless_tx_bytes: Bytes,
    gas_limit_override: u64,
) -> ExecutionResult<MegaHaltReason> {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes,
        gasLimitOverride: U256::from(gas_limit_override),
    }
    .abi_encode();

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let tx = TxEnv {
        caller: RELAYER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: OUTER_GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    alloy_evm::Evm::transact_commit(&mut evm, tx)
        .expect("outer keyless call should not fail at the EVM-error level")
}

/// Decode the `IKeylessDeploy::keylessDeployReturn` payload from the outer Success
/// output and return the contained `errorData` bytes.
fn extract_error_data(result: &ExecutionResult<MegaHaltReason>) -> Bytes {
    let output = match result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        other => panic!(
            "keyless empty-code path must return Success-style (errorData carries the failure); \
             got {other:?}",
        ),
    };
    let decoded = IKeylessDeploy::keylessDeployCall::abi_decode_returns(&output)
        .expect("outer Success output must decode as keylessDeployReturn");
    decoded.errorData
}

/// REX5: keyless init code that emits `LOG1` and then RETURNs empty runtime bytecode
/// MUST surface the log in the parent receipt, and the outer `errorData` MUST decode
/// as `EmptyCodeDeployed(gasUsed)`. Replay barrier (signer nonce bump) MUST still
/// persist.
#[test]
fn test_rex5_keyless_empty_code_forwards_log_to_parent_receipt() {
    let topic =
        B256::from_slice(&hex!("ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11"));
    let init_code = build_log1_then_empty_return_init_code(topic);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // signer is intentionally unfunded — REX5 fee-free sandbox needs no signer balance.

    let result =
        run_keyless_outer(MegaSpecId::REX5, &mut db, keyless_tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);

    // The wire shape is success-style with errorData = EmptyCodeDeployed.
    let error_data = extract_error_data(&result);
    let decoded_err = decode_error_result(&error_data);
    assert!(
        matches!(decoded_err, Some(KeylessDeployError::EmptyCodeDeployed { .. })),
        "REX5 empty-code path must surface EmptyCodeDeployed in errorData; got {decoded_err:?}",
    );

    // Replay barrier: the signer nonce must have advanced even on the empty-code path,
    // because state still merges into the parent.
    let signer_after = {
        use revm::Database as _;
        db.basic(signer).expect("db read should succeed").unwrap_or_default()
    };
    assert_eq!(
        signer_after.nonce, 1,
        "REX5 empty-code path must consume the replay barrier (signer nonce bump)",
    );

    // The parent receipt MUST contain the constructor's LOG1 with the expected topic and
    // emitter address. Pre-fix behavior dropped the log entirely.
    let logs = result.logs();
    assert_eq!(
        logs.len(),
        1,
        "REX5 empty-code path must forward the constructor's LOG1 into the parent receipt; \
         got logs = {logs:?}",
    );
    let log = &logs[0];
    assert_eq!(
        log.address, deploy_address,
        "forwarded log must carry the deploy address as emitter",
    );
    let topics = log.data.topics();
    assert_eq!(topics.len(), 1, "LOG1 must have exactly one topic; got {topics:?}");
    assert_eq!(topics[0], topic, "LOG1 topic must match the value pushed in init code");
    let expected_data = 0xdead_beef_u32.to_be_bytes();
    assert_eq!(
        log.data.data.as_ref(),
        &expected_data,
        "LOG1 data must match the 4-byte word the init code emitted",
    );
}

/// REX4 (pre-REX5 frozen): same input MUST return success-style with
/// `EmptyCodeDeployed` errorData, but the parent receipt MUST contain NO logs
/// (legacy "drop logs" behavior, preserved for replay parity).
#[test]
fn test_rex4_keyless_empty_code_drops_logs_for_replay_parity() {
    let topic =
        B256::from_slice(&hex!("ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11ee11"));
    let init_code = build_log1_then_empty_return_init_code(topic);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // REX4 sandbox is NOT fee-free — fund signer so the inner tx can pay
    // `gas_limit_override × signed_gas_price` upfront.
    db.set_account_balance(signer, U256::from(1_000_000_000_000_000_000u128));

    // REX4 sandbox debits `gas_limit_override × signed_gas_price` from signer at
    // pre-execution. With `signed_gas_price = 100 gwei`, an override of 1M gas
    // costs 0.0001 ETH — well within the 1 ETH funding above. Using
    // `LARGE_GAS_LIMIT_OVERRIDE` (10^10) here would demand 10^21 wei = 1000 ETH
    // and trip the `InsufficientBalance` precheck before the sandbox can run.
    let rex4_gas_limit_override: u64 = 1_000_000;
    let result =
        run_keyless_outer(MegaSpecId::REX4, &mut db, keyless_tx_bytes, rex4_gas_limit_override);

    let error_data = extract_error_data(&result);
    let decoded_err = decode_error_result(&error_data);
    assert!(
        matches!(decoded_err, Some(KeylessDeployError::EmptyCodeDeployed { .. })),
        "REX4 empty-code path must surface EmptyCodeDeployed in errorData; got {decoded_err:?}",
    );

    let logs = result.logs();
    assert!(
        logs.is_empty(),
        "REX4 empty-code path must NOT forward sandbox logs (frozen for replay parity); \
         got logs = {logs:?}",
    );
}

/// REX5 with an empty-code constructor that emits NO logs MUST report
/// `EmptyCodeDeployed` AND a zero-length receipt log list. Pins that the
/// `SandboxCompletion::EmptyCode` wiring forwards exactly the logs the sandbox emitted, not
/// anything more.
#[test]
fn test_rex5_keyless_empty_code_no_logs_emitted_no_logs_forwarded() {
    let init_code = build_no_logs_empty_return_init_code();
    let (keyless_tx_bytes, _signer) = build_keyless_tx_with_init_code(init_code);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let result =
        run_keyless_outer(MegaSpecId::REX5, &mut db, keyless_tx_bytes, LARGE_GAS_LIMIT_OVERRIDE);

    let error_data = extract_error_data(&result);
    let decoded_err = decode_error_result(&error_data);
    assert!(
        matches!(decoded_err, Some(KeylessDeployError::EmptyCodeDeployed { .. })),
        "REX5 empty-code path must surface EmptyCodeDeployed in errorData; got {decoded_err:?}",
    );

    let logs = result.logs();
    assert!(
        logs.is_empty(),
        "REX5 empty-code path with no LOG opcodes must forward zero logs; got logs = {logs:?}",
    );
}
