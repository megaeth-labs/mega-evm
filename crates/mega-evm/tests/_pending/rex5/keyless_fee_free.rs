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

// ============================================================================
// 2. REX4 keeps the `gas_cost + value` balance precheck
// ============================================================================

// ============================================================================
// 3. value > 0 still requires the signer to fund the transfer (REX5)
// ============================================================================

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

// ============================================================================
// 11. Pre-sandbox materialization OOG halts before the sandbox ever runs
// ============================================================================

// ============================================================================
// 12. Sandbox-validate-reject still pays the upfront materialization charge
// ============================================================================

// ============================================================================
// 13. Step-1 dispatch-overhead OOG when the call's gas_limit is below the fixed cost
// ============================================================================
