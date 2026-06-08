//! REX5 keyless deploy fee-free invariants.
//!
//! Pin the post-REX5-fee-free contract:
//!
//! - sandbox tx runs as an OP deposit-like transaction (`gas_price = 0`, `source_hash` set), so the
//!   inner signer is never debited for sandbox gas;
//! - configured init code size limit is enforced by the sandbox itself, since the deposit path
//!   bypasses revm's `validate_env`;
//! - GASPRICE inside the sandbox is observable as `0`;
//! - deposit-caller materialization gas is charged BEFORE the sandbox runs (alongside
//!   `KEYLESS_DEPLOY_OVERHEAD_GAS`) based on parent journal-visible state, and is retained even
//!   when the sandbox subsequently validate-rejects.

use std::vec::Vec;

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::ExecutionResult,
    sandbox::{calculate_keyless_deploy_address, decode_error_result, KeylessDeployError},
    test_utils::MemoryDatabase,
    IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, SaltEnv,
    TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS, MIN_BUCKET_SIZE,
};
use revm::{
    context::{
        result::{HaltReason, OutOfGasError},
        TxEnv,
    },
    database::AccountState,
    inspector::NoOpInspector,
    state::Bytecode,
    Database as _,
};

const RELAYER: Address = address!("0000000000000000000000000000000000990000");
const SIGNED_GAS_PRICE: u128 = 100_000_000_000; // 100 gwei
const SIGNED_GAS_LIMIT: u64 = 100_000;
const OUTER_GAS_LIMIT: u64 = 30_000_000;
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;

fn run_keyless_outer(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: TestExternalEnvs<std::convert::Infallible>,
    keyless_tx_bytes: Bytes,
    gas_limit_override: u64,
) -> ExecutionResult<MegaHaltReason> {
    run_keyless_outer_with(spec, db, external_envs, keyless_tx_bytes, gas_limit_override, |_| {})
}

/// Variant that lets a test set a custom outer-tx `gas_limit`, used to force the
/// keylessDeploy interceptor's incoming `call_inputs.gas_limit` below thresholds like
/// `KEYLESS_DEPLOY_OVERHEAD_GAS`.
fn run_keyless_outer_with_gas_limit(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: TestExternalEnvs<std::convert::Infallible>,
    keyless_tx_bytes: Bytes,
    gas_limit_override: u64,
    outer_gas_limit: u64,
) -> ExecutionResult<MegaHaltReason> {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes,
        gasLimitOverride: U256::from(gas_limit_override),
    }
    .abi_encode();

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
        gas_limit: outer_gas_limit,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    alloy_evm::Evm::transact_commit(&mut evm, tx)
        .expect("outer keyless call should not fail at the EVM-error level")
}

/// Variant that lets a test tweak the outer `MegaContext` (e.g., `cfg`) before the
/// keylessDeploy call runs.
fn run_keyless_outer_with<F>(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: TestExternalEnvs<std::convert::Infallible>,
    keyless_tx_bytes: Bytes,
    gas_limit_override: u64,
    customize: F,
) -> ExecutionResult<MegaHaltReason>
where
    F: FnOnce(&mut MegaContext<&mut MemoryDatabase, TestExternalEnvs<std::convert::Infallible>>),
{
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: keyless_tx_bytes,
        gasLimitOverride: U256::from(gas_limit_override),
    }
    .abi_encode();

    let mut context = MegaContext::new(db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    customize(&mut context);

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

fn account_info(db: &mut MemoryDatabase, addr: Address) -> revm::state::AccountInfo {
    db.basic(addr).expect("db read should succeed").unwrap_or_default()
}

fn has_code(db: &mut MemoryDatabase, addr: Address) -> bool {
    let info = account_info(db, addr);
    if let Some(code) = info.code {
        !code.is_empty()
    } else {
        info.code_hash != revm::primitives::KECCAK_EMPTY
    }
}

/// Builds a deterministic pre-EIP-155 keyless tx with the given init code.
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

/// Returns the runtime code committed at `addr`.
fn deployed_code(db: &mut MemoryDatabase, addr: Address) -> Bytes {
    account_info(db, addr)
        .code
        .map(|code| Bytes::copy_from_slice(code.bytes_slice()))
        .unwrap_or_default()
}

/// Reads a storage slot from the committed parent state.
fn storage_slot(db: &mut MemoryDatabase, addr: Address, slot: U256) -> U256 {
    db.storage(addr, slot).expect("db storage read should succeed")
}

/// Constructor that returns 1-byte STOP runtime code (so it survives the
/// `EmptyCodeDeployed` check).
const STOP_RUNTIME_INIT_CODE: &[u8] = &[
    0x60, 0x00, // PUSH1 0x00
    0x60, 0x00, // PUSH1 0x00
    0x52, // MSTORE
    0x60, 0x01, // PUSH1 0x01
    0x60, 0x1f, // PUSH1 0x1f
    0xf3, // RETURN
];

// ============================================================================
// 1. value = 0 + signer balance = 0 deploys successfully under REX5
// ============================================================================

/// REX5 fee-free: the inner signer needs no balance to deploy when value = 0.
/// Pre-Rex5 required `gas_limit × gas_price + value` of balance; REX5 drops the
/// gas portion because the sandbox runs as a deposit-like fee-free tx.
#[test]
fn test_rex5_value_zero_signer_zero_balance_deploys_successfully() {
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // signer is intentionally NOT funded.

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    assert!(result.is_success(), "fee-free deploy with empty signer must succeed; got {result:?}");
    let signer_after = account_info(&mut db, signer);
    assert_eq!(signer_after.balance, U256::ZERO, "fee-free: signer balance must remain 0");
    assert_eq!(signer_after.nonce, 1, "replay barrier: signer nonce must advance to 1");
    assert!(has_code(&mut db, deploy_address), "deploy address must have non-empty code");
}

// ============================================================================
// 2. REX4 keeps the `gas_cost + value` balance precheck
// ============================================================================

/// REX4 (frozen spec): the sandbox's pre-execution balance check must reject an
/// unfunded signer, because pre-Rex5 runs the sandbox as a non-deposit tx that
/// debits `gas_limit × gas_price` from the signer. The fee-free simplification
/// is REX5-only.
#[test]
fn test_rex4_unfunded_signer_still_rejected_by_balance_precheck() {
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    let (keyless_tx_bytes, _signer) = build_keyless_tx_with_init_code(init_code);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // signer NOT funded; pre-Rex5 gas_cost > 0 so balance check trips.

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer(
        MegaSpecId::REX4,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    let revert_output = match &result {
        ExecutionResult::Revert { output, .. } => output.clone(),
        _ => panic!(
            "REX4 must revert when sandbox pre-check rejects unfunded signer; got {result:?}"
        ),
    };
    let err = decode_error_result(&revert_output).expect("revert output must decode");
    assert!(
        matches!(err, KeylessDeployError::InsufficientBalance),
        "REX4 must reject unfunded signer with InsufficientBalance; got {err:?}",
    );
}

// ============================================================================
// 3. value > 0 still requires the signer to fund the transfer (REX5)
// ============================================================================

/// REX5 fee-free still requires `signer.balance >= value`. Only the gas portion
/// is dropped — value transfer is real EVM semantics that needs funding.
#[test]
fn test_rex5_value_positive_with_insufficient_signer_balance_rejected() {
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    // build a value-bearing keyless tx (value=1 wei requires 1 wei in signer balance).
    let tx = TxLegacy {
        nonce: 0,
        gas_price: SIGNED_GAS_PRICE,
        gas_limit: SIGNED_GAS_LIMIT,
        to: TxKind::Create,
        value: U256::from(1u64),
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
    let keyless_tx_bytes = Bytes::from(buf);
    let signer = signed.recover_signer().expect("recover");

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // signer has 0 balance — cannot cover `value = 1`.

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    let revert_output = match &result {
        ExecutionResult::Revert { output, .. } => output.clone(),
        _ => panic!("REX5 value-precheck rejection must revert; got {result:?}"),
    };
    let err = decode_error_result(&revert_output).expect("revert output must decode");
    assert!(
        matches!(err, KeylessDeployError::InsufficientBalance),
        "value > 0 must be pre-checked under REX5; got {err:?}",
    );
    let signer_after = account_info(&mut db, signer);
    assert_eq!(signer_after.nonce, 0, "pre-check must not bump signer nonce");
    assert!(!has_code(&mut db, calculate_keyless_deploy_address(signer)));
}

/// REX5: triggering the sandbox-side initcode size check end-to-end with the production
/// `cfg.max_initcode_size = MAX_INITCODE_SIZE` (~536 KiB) would require a calldata payload
/// that trips `GasFloorMoreThanGasLimit` at the outer-tx validation layer first. To exercise
/// the sandbox's own pre-check, this test lowers `cfg.limit_contract_initcode_size` to a
/// tiny value so a 2-byte initcode is enough to overflow the configured cap.
///
/// Asserts:
/// - outer call reverts with `InitCodeTooLarge { size, max }`,
/// - `size` is the keyless tx's actual initcode length,
/// - `max` is the lowered configured cap,
/// - signer nonce stays at 0 (pre-check rejection),
/// - deploy address has no code.
#[test]
fn test_rex5_oversized_initcode_rejected_with_init_code_too_large() {
    // 2-byte init code: PUSH1 0x00 + STOP. Smaller than our lowered cap of 1 byte.
    let init_code = Bytes::from_static(&[0x60, 0x00]);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer_with(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        |ctx| {
            ctx.modify_cfg(|cfg| {
                cfg.limit_contract_initcode_size = Some(1);
            });
        },
    );

    let revert_output = match &result {
        ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("oversized init code must revert; got {other:?}"),
    };
    let err = decode_error_result(&revert_output).expect("revert output must decode");
    let (size, cap) = match err {
        KeylessDeployError::InitCodeTooLarge { size, max } => (size, max),
        other => panic!("expected InitCodeTooLarge, got {other:?}"),
    };
    assert_eq!(size, 2, "selector must report the actual init code length");
    assert_eq!(cap, 1, "selector must report the lowered cfg().max_initcode_size()");

    let signer_after = account_info(&mut db, signer);
    assert_eq!(signer_after.nonce, 0, "pre-check rejection must not bump signer nonce");
    assert!(!has_code(&mut db, deploy_address), "no deployment on InitCodeTooLarge");
}

// ============================================================================
// 5. Caller materialization is charged on first deploy, not on retry
// ============================================================================

/// REX5: when the deploy signer is unmaterialized in the parent state,
/// `charge_caller_materialization_pre_sandbox` charges `new_account_storage_gas(signer)`
/// against the outer Gas counter and records a deposit-caller state-growth
/// event. A second deploy (parent signer already nonce=1) must not re-charge,
/// even though `SandboxDb::with_nonce_override` makes the sandbox-internal view
/// see nonce=0.
///
/// First-deploy charge is observable as a difference in outer `gasUsed` between
/// two scenarios that differ only in whether the signer is materialized
/// pre-sandbox. To make the difference visible we put the signer's bucket in a
/// hot configuration so `new_account_storage_gas(signer)` is non-zero (REX
/// formula gives `25_000 × (multiplier - 1)`; the default multiplier of 1
/// would yield 0 and hide the difference).
#[test]
fn test_rex5_caller_materialization_first_deploy_charged_retry_not_recharged() {
    // Init code that REVERTS, so the deploy address never gets code and we can run
    // a second keylessDeploy call against the same signer.
    let revert_init_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(revert_init_code);

    let signer_bucket = <TestExternalEnvs as SaltEnv>::bucket_id_for_account(signer);
    let make_envs = || {
        TestExternalEnvs::<std::convert::Infallible>::new()
            .with_bucket_capacity(signer_bucket, MIN_BUCKET_SIZE as u64 * 2)
    };

    // First call: parent state has empty signer.
    let mut db1 = MemoryDatabase::default();
    db1.set_account_balance(RELAYER, U256::from(1_000_000_000_000_000_000u128));
    let first = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db1,
        make_envs(),
        keyless_tx_bytes.clone(),
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    let first_signer_after = account_info(&mut db1, signer);
    assert_eq!(first_signer_after.nonce, 1, "first call must bump signer nonce");

    let first_gas_used = match &first {
        ExecutionResult::Success { gas_used, .. } => *gas_used,
        other => panic!("first call must return success-style; got {other:?}"),
    };

    // Second call: same signer, but already materialized in the parent state.
    // We seed parent state directly to avoid running the first sandbox a second time.
    let mut db2 = MemoryDatabase::default();
    db2.set_account_balance(RELAYER, U256::from(1_000_000_000_000_000_000u128));
    db2.set_account_nonce(signer, 1);
    let second = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db2,
        make_envs(),
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    let second_signer_after = account_info(&mut db2, signer);
    assert_eq!(second_signer_after.nonce, 1, "retry: signer nonce must remain 1 (already used)");

    let second_gas_used = match &second {
        ExecutionResult::Success { gas_used, .. } => *gas_used,
        other => panic!("second call must return success-style; got {other:?}"),
    };

    // The first call paid for caller materialization; the second did not.
    // With hot bucket × 2, `new_account_storage_gas(signer) = 25_000 × 1 = 25_000`,
    // so first_gas_used must exceed second_gas_used by exactly that amount.
    let expected_diff: u64 = mega_evm::constants::rex::NEW_ACCOUNT_STORAGE_GAS_BASE;
    assert_eq!(
        first_gas_used.checked_sub(second_gas_used),
        Some(expected_diff),
        "first deploy must include exactly one caller materialization gas charge; \
         first={first_gas_used} second={second_gas_used}",
    );
}

// ============================================================================
// 6. GASPRICE inside the REX5 sandbox returns 0 (consensus-observable)
// ============================================================================

/// REX5 deposit-style sandbox sets `tx.gas_price = 0`. As a consequence, the
/// `GASPRICE` opcode executed inside the constructor returns 0, even though
/// the keyless transaction's signed gas price is non-zero (100 gwei here).
/// This test pins the spec choice so future "restore raw `gas_price`" attempts
/// fail loudly.
#[test]
fn test_rex5_sandbox_gasprice_opcode_returns_zero() {
    // Constructor:
    //   slot 0x42 ← 1                                  (marker; proves SSTORE ran)
    //   slot 0x43 ← GASPRICE                           (the assertion)
    //   RETURN single-byte STOP                        (avoid EmptyCodeDeployed)
    //
    // Bytecode:
    //   60 01  60 42  55       PUSH1 1, PUSH1 0x42, SSTORE
    //   3a     60 43  55       GASPRICE, PUSH1 0x43, SSTORE
    //   60 00  60 00  52       PUSH1 0, PUSH1 0, MSTORE
    //   60 01  60 1f  f3       PUSH1 1, PUSH1 0x1f, RETURN
    let init_code = Bytes::from_static(&[
        0x60, 0x01, 0x60, 0x42, 0x55, // SSTORE(0x42, 1)
        0x3a, 0x60, 0x43, 0x55, // SSTORE(0x43, GASPRICE)
        0x60, 0x00, 0x60, 0x00, 0x52, // mem[0:32] = 0
        0x60, 0x01, 0x60, 0x1f, 0xf3, // return mem[0x1f:0x20] = 0x00 (STOP)
    ]);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );
    assert!(result.is_success(), "deploy must succeed; got {result:?}");
    assert!(has_code(&mut db, deploy_address), "deploy address must have runtime code");

    let marker = storage_slot(&mut db, deploy_address, U256::from(0x42u64));
    assert_eq!(marker, U256::from(1u64), "marker proves the constructor's SSTORE actually ran");

    let observed_gas_price = storage_slot(&mut db, deploy_address, U256::from(0x43u64));
    assert_eq!(
        observed_gas_price,
        U256::ZERO,
        "REX5 sandbox: GASPRICE opcode must return 0 even though the signed gas_price is {SIGNED_GAS_PRICE} wei",
    );

    // Sanity: deployed code is the 1-byte STOP we returned.
    let code = deployed_code(&mut db, deploy_address);
    assert_eq!(code.as_ref(), &[0x00u8]);
}

// ============================================================================
// 7. Runtime halt under REX5 deposit-style sandbox returns success-style errorData
// ============================================================================

/// REX5: when the inner constructor halts (INVALID opcode here), the outer call MUST
/// still return success-style with `errorData = ExecutionHalted(...)` — NOT revert with
/// `InvalidTransaction`. The signer nonce MUST be bumped (replay barrier consumed). The
/// outer Gas counter MUST be debited for the sandbox's actual `gas_used` (not the
/// inflated `gas_limit` that op-revm's `FailedDeposit` path would have produced).
#[test]
fn test_rex5_runtime_halt_returns_execution_halted_with_replay_barrier_consumed() {
    // Constructor: `INVALID` opcode — halts immediately, no gas refund.
    let invalid_init_code = Bytes::from_static(&[0xfe]);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(invalid_init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000_000_000_000u128));

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    // Outer call MUST surface as success-style (the keyless interceptor returns
    // success-with-errorData on inner failure); a `Revert(InvalidTransaction)` here
    // would mean the runtime halt was misclassified as a validation rejection.
    let (gas_used, output) = match &result {
        ExecutionResult::Success { gas_used, output, .. } => (*gas_used, output.data().clone()),
        other => panic!("runtime halt must surface as outer Success; got {other:?}"),
    };
    let decoded = IKeylessDeploy::keylessDeployCall::abi_decode_returns(&output)
        .expect("decode keylessDeploy return data");
    let inner_err = decode_error_result(&decoded.errorData).expect("errorData must decode");
    assert!(
        matches!(inner_err, KeylessDeployError::ExecutionHalted { .. }),
        "runtime halt must surface as ExecutionHalted, NOT InvalidTransaction; got {inner_err:?}",
    );

    // The outer Gas debit MUST reflect the sandbox's actual gas usage. A `FailedDeposit`
    // misclassification would have inflated this to ~gas_limit_override.
    assert!(
        gas_used < LARGE_GAS_LIMIT_OVERRIDE,
        "outer gas_used must NOT equal the inflated FailedDeposit gas_limit; got {gas_used}",
    );

    // Replay barrier consumed: signer nonce = 1.
    let signer_after = account_info(&mut db, signer);
    assert_eq!(signer_after.nonce, 1, "runtime halt must bump signer nonce via make_create_frame");
    assert_eq!(signer_after.balance, U256::ZERO, "fee-free: signer balance must be unchanged");
    assert!(
        !has_code(&mut db, deploy_address),
        "halted constructor must leave deploy address empty"
    );
}

// ============================================================================
// 8. EIP-3607: signer with non-empty, non-EIP-7702 bytecode must be rejected
// ============================================================================

/// REX5: when the recovered signer already has non-empty bytecode in parent state that is
/// NOT an EIP-7702 delegation, the sandbox MUST reject with `SignerHasCode` before
/// constructing the deposit-style sandbox tx (which would otherwise bypass op-revm's
/// `validate_account_nonce_and_code` check).
#[test]
fn test_rex5_signer_with_contract_code_rejected_with_signer_has_code() {
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // Place real (non-EIP-7702) contract code at the signer address.
    db.set_account_code(signer, Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]));

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    let revert_output = match &result {
        ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("signer-with-code must revert with SignerHasCode; got {other:?}"),
    };
    let err = decode_error_result(&revert_output).expect("revert output must decode");
    assert!(
        matches!(err, KeylessDeployError::SignerHasCode),
        "REX5 must reject signer with contract code; got {err:?}",
    );
    // Pre-check rejection: signer untouched, no deployment.
    let signer_after = account_info(&mut db, signer);
    assert_eq!(signer_after.nonce, 0, "pre-check rejection must not bump signer nonce");
    assert!(!has_code(&mut db, deploy_address), "no deployment on pre-check rejection");
}

/// REX5: `cfg.disable_eip3607 = true` MUST bypass the `SignerHasCode` pre-check, matching
/// the canonical revm `validate_account_nonce_and_code` semantics. Pinned so the keyless
/// sandbox path stays consistent with the system-tx validation path and ordinary tx
/// validation under the same cfg flag.
#[test]
fn test_rex5_signer_has_code_check_respects_disable_eip3607() {
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // Same setup as the rejection test: signer has contract code.
    db.set_account_code(signer, Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]));

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer_with(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        |ctx| {
            ctx.modify_cfg(|cfg| {
                cfg.disable_eip3607 = true;
            });
        },
    );

    // With EIP-3607 disabled, the SignerHasCode pre-check is bypassed. The keyless deploy
    // then fails downstream because the deploy address would collide with already-existing
    // code at the signer (CREATE address vs signer are different, but the sandbox proceeds
    // past the SignerHasCode gate). The exact downstream outcome isn't the focus here —
    // what matters is that we do NOT revert with SignerHasCode.
    if let ExecutionResult::Revert { output, .. } = &result {
        if let Some(err) = decode_error_result(output) {
            assert!(
                !matches!(err, KeylessDeployError::SignerHasCode),
                "disable_eip3607=true must bypass SignerHasCode; got {err:?}",
            );
        }
    }
    // Direct positive: also exercise the non-code signer path with the same cfg flag to
    // confirm the override doesn't break the normal flow.
    let (clean_tx_bytes, clean_signer) =
        build_keyless_tx_with_init_code(Bytes::from_static(STOP_RUNTIME_INIT_CODE));
    let mut db2 = MemoryDatabase::default();
    db2.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    let clean_result = run_keyless_outer_with(
        MegaSpecId::REX5,
        &mut db2,
        TestExternalEnvs::<std::convert::Infallible>::new(),
        clean_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        |ctx| {
            ctx.modify_cfg(|cfg| {
                cfg.disable_eip3607 = true;
            });
        },
    );
    assert!(
        clean_result.is_success(),
        "disable_eip3607=true on a clean signer must still deploy successfully; got {clean_result:?}",
    );
    assert_eq!(account_info(&mut db2, clean_signer).nonce, 1, "clean signer nonce must bump");
}

/// Installs an EIP-7702 delegation designator (`0xef0100 || delegate_to`) at `address`,
/// matching what revm's `apply_eip7702_auth_list` writes during Type-4 tx processing.
/// `MemoryDatabase::set_account_code` only writes `Bytecode::new_legacy(..)`, which would
/// not satisfy `Bytecode::is_eip7702()` even if the bytes match the EIP-7702 magic, so
/// the test wires the proper variant directly.
fn set_eip7702_delegation_at(db: &mut MemoryDatabase, address: Address, delegate_to: Address) {
    let bytecode = Bytecode::new_eip7702(delegate_to);
    let code_hash = bytecode.hash_slow();
    let account = db.load_account(address).unwrap();
    account.info.code = Some(bytecode);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
}

// ============================================================================
// 10. EIP-7702 delegated signer is allowed through the EIP-3607 pre-check
// ============================================================================

/// REX5: a signer carrying only an EIP-7702 delegation designator (not real contract code)
/// MUST pass `validate_signer_code` and proceed to deploy. The `code.is_eip7702()` arm in
/// the EIP-3607 helper is the only thing keeping 7702-delegated EOAs eligible — without
/// it, every account that has accepted an authorization would lose access to keyless
/// deploy.
#[test]
fn test_rex5_signer_with_eip7702_delegation_passes_eip3607_check() {
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    let delegate_to = address!("dddddddddddddddddddddddddddddddddddd0001");
    set_eip7702_delegation_at(&mut db, signer, delegate_to);

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    assert!(
        result.is_success(),
        "EIP-7702 delegated signer must be allowed to deploy; got {result:?}",
    );
    if let ExecutionResult::Revert { output, .. } = &result {
        if let Some(err) = decode_error_result(output) {
            assert!(
                !matches!(err, KeylessDeployError::SignerHasCode),
                "EIP-7702 delegation must NOT trip SignerHasCode; got {err:?}",
            );
        }
    }
    let signer_after = account_info(&mut db, signer);
    assert_eq!(signer_after.nonce, 1, "deploy must bump signer nonce");
    assert!(has_code(&mut db, deploy_address), "deploy address must have runtime code");
}

// ============================================================================
// 11. Pre-sandbox materialization OOG halts before the sandbox ever runs
// ============================================================================

/// REX5: when the deploy signer is unmaterialized in parent state and the signer's
/// SALT bucket multiplier inflates `new_account_storage_gas` past the outer call's
/// remaining gas envelope, `charge_caller_materialization_pre_sandbox` MUST halt the
/// outer call with plain OOG (no rescue) BEFORE the sandbox is constructed — so the
/// signer nonce bump (the Nick's-Method replay barrier) never even appears, and the
/// deploy address stays empty. The halt is symmetric across what the constructor
/// *would* have done (success / runtime-revert) because no sandbox frame is ever
/// created.
#[test]
fn test_rex5_pre_sandbox_materialization_oog_halts_before_sandbox_runs() {
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let signer_bucket = <TestExternalEnvs as SaltEnv>::bucket_id_for_account(signer);
    // Multiplier ≈ 1_000_000 → `new_account_storage_gas(signer) ≈ 25_000 × 999_999`,
    // far above the outer call's remaining gas envelope.
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new()
        .with_bucket_capacity(signer_bucket, MIN_BUCKET_SIZE as u64 * 1_000_000);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000_000_000_000u128));

    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    let halt_reason = match &result {
        ExecutionResult::Halt { reason, .. } => reason.clone(),
        other => {
            panic!("pre-sandbox materialization OOG must surface as outer Halt; got {other:?}")
        }
    };
    assert!(
        matches!(
            halt_reason,
            MegaHaltReason::Base(op_revm::OpHaltReason::Base(HaltReason::OutOfGas(
                OutOfGasError::Basic
            )))
        ),
        "pre-sandbox materialization halt MUST be plain OOG (no rescue); got {halt_reason:?}",
    );

    let signer_after = account_info(&mut db, signer);
    assert_eq!(
        signer_after.nonce, 0,
        "pre-sandbox materialization OOG must halt before the sandbox can bump the signer nonce",
    );
    assert!(
        !has_code(&mut db, deploy_address),
        "no deployment when materialization fails before the sandbox runs",
    );
}

// ============================================================================
// 12. Sandbox-validate-reject still pays the upfront materialization charge
// ============================================================================

/// REX5: pinning the new failure-surface contract — materialization is charged
/// upfront (alongside `KEYLESS_DEPLOY_OVERHEAD_GAS`), so a sandbox-validate-reject
/// (`FailedDeposit → InvalidTransaction`) MUST still debit the outer gas counter for
/// the materialization amount, even though the sandbox state is dropped and no
/// replay barrier is installed. The relayer pays for the parent-state read +
/// SALT-multiplier work it asked for, just like the dispatch overhead.
///
/// Proof technique: two runs that differ only in the signer's bucket multiplier
/// (1 vs 2). With multiplier 1, `new_account_storage_gas = 0`; with multiplier 2,
/// it equals `NEW_ACCOUNT_STORAGE_GAS_BASE`. The delta in outer `gas_used` between
/// the two runs MUST equal that amount — proving materialization rode along on the
/// validate-reject path.
#[test]
fn test_rex5_pre_sandbox_materialization_charged_even_on_sandbox_validate_reject() {
    // Sandbox-validate-reject: the keyless tx's `gas_limit` is far below the sandbox
    // EVM's actual intrinsic + floor gas requirement, so `MegaHandler::validate`
    // inside the sandbox surfaces `CallGasCostMoreThanGasLimit`, which op-revm's
    // deposit `catch_error` wraps into `Ok(Halt(FailedDeposit, gas_limit))` →
    // `process_sandbox_transact_result` maps to `KeylessDeployError::InvalidTransaction`.
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    let low_gas_limit: u64 = 50_000;
    let tx = TxLegacy {
        nonce: 0,
        gas_price: SIGNED_GAS_PRICE,
        gas_limit: low_gas_limit,
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
    let keyless_tx_bytes = Bytes::from(buf);
    let signer = signed.recover_signer().expect("recover");
    let signer_bucket = <TestExternalEnvs as SaltEnv>::bucket_id_for_account(signer);

    // Run with the same low gas_limit_override (forces sandbox-validate to reject),
    // varying only the signer's bucket multiplier between runs.
    let run = |bucket_capacity: u64| -> (ExecutionResult<MegaHaltReason>, u64) {
        let mut db = MemoryDatabase::default();
        db.set_account_balance(RELAYER, U256::from(1_000_000_000_000_000_000u128));
        let envs = TestExternalEnvs::<std::convert::Infallible>::new()
            .with_bucket_capacity(signer_bucket, bucket_capacity);
        let result = run_keyless_outer(
            MegaSpecId::REX5,
            &mut db,
            envs,
            keyless_tx_bytes.clone(),
            low_gas_limit,
        );
        let gas_used = result.gas_used();
        (result, gas_used)
    };

    // Scenario A: bucket multiplier 1 → materialization gas = 0.
    let (result_a, gas_used_a) = run(MIN_BUCKET_SIZE as u64);
    let revert_a = match &result_a {
        ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("validate-reject must surface as outer Revert; got {other:?}"),
    };
    let err_a = decode_error_result(&revert_a).expect("revert output must decode");
    assert!(
        matches!(err_a, KeylessDeployError::InvalidTransaction),
        "low gas_limit_override must trigger sandbox-validate-reject → InvalidTransaction; got {err_a:?}",
    );

    // Scenario B: bucket multiplier 2 → materialization gas = NEW_ACCOUNT_STORAGE_GAS_BASE.
    let (result_b, gas_used_b) = run(MIN_BUCKET_SIZE as u64 * 2);
    let revert_b = match &result_b {
        ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("validate-reject must surface as outer Revert (scenario B); got {other:?}"),
    };
    let err_b = decode_error_result(&revert_b).expect("revert output must decode");
    assert!(matches!(err_b, KeylessDeployError::InvalidTransaction), "got {err_b:?}");

    // The materialization gas was charged upfront and retained on the validate-reject
    // path: outer gas_used differs by exactly `NEW_ACCOUNT_STORAGE_GAS_BASE`.
    let expected_diff: u64 = mega_evm::constants::rex::NEW_ACCOUNT_STORAGE_GAS_BASE;
    assert_eq!(
        gas_used_b.checked_sub(gas_used_a),
        Some(expected_diff),
        "validate-reject MUST retain the upfront materialization charge; \
         scenario A (mult=1, mat=0)={gas_used_a} B (mult=2, mat={expected_diff})={gas_used_b}",
    );
}

// ============================================================================
// 13. Step-1 dispatch-overhead OOG when the call's gas_limit is below the fixed cost
// ============================================================================

/// REX5: the very first step of `execute_keyless_deploy_call` charges
/// `KEYLESS_DEPLOY_OVERHEAD_GAS` (100K) against the inbound `Gas` object. When the outer
/// caller forwards less than that, the call must halt with plain OOG at line 1 of the
/// interceptor and never reach the decode / signer / sandbox steps — so neither the
/// signer nonce nor the parent state may be touched.
#[test]
fn test_rex5_outer_call_gas_below_dispatch_overhead_halts_at_step_one() {
    let init_code = Bytes::from_static(STOP_RUNTIME_INIT_CODE);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    // Outer tx gas_limit sized so initial+floor gas validation passes (REX5 base intrinsic
    // is ~60K) but the leftover forwarded to the keylessDeploy frame is well under
    // `KEYLESS_DEPLOY_OVERHEAD_GAS = 100_000`.
    let outer_gas_limit = 130_000;

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer_with_gas_limit(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
        outer_gas_limit,
    );

    let halt_reason = match &result {
        ExecutionResult::Halt { reason, .. } => reason.clone(),
        other => panic!("step-1 OOG must surface as outer Halt; got {other:?}"),
    };
    assert!(
        matches!(
            halt_reason,
            MegaHaltReason::Base(op_revm::OpHaltReason::Base(HaltReason::OutOfGas(
                OutOfGasError::Basic
            )))
        ),
        "step-1 dispatch-overhead halt MUST be plain OOG; got {halt_reason:?}",
    );

    // The interceptor halted before signer recovery, so the signer must remain
    // untouched and no deployment may have happened.
    let signer_after = account_info(&mut db, signer);
    assert_eq!(signer_after.nonce, 0, "step-1 OOG must not bump signer nonce");
    assert_eq!(signer_after.balance, U256::ZERO, "step-1 OOG must not debit signer");
    assert!(!has_code(&mut db, deploy_address), "no deployment when step-1 halts");
}
