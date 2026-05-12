//! REX5 keyless deploy replay-barrier invariant.
//!
//! Pin the property that `KeylessDeploy` never produces a parent-visible signer fee
//! deduction without also producing a parent-visible durable replay barrier (signer
//! nonce bump *or* deploy-address code). Combined with the REX5+ final Mega-side gas
//! validation, the failure paths inside the sandbox split into:
//!
//! - **`validate()` rejects** (e.g. final intrinsic / floor gas overrun): the sandbox returns
//!   `Err(...)` before `pre_execution()` runs, no signer mutation occurs, and the outer
//!   keyless-deploy call surfaces as `ExecutionResult::Revert`.
//! - **`pre_execution()` runs** and the inner CREATE either succeeds or reverts in the constructor:
//!   in either case `make_create_frame` reaches the post-pre-bump nonce bump, so the signer nonce
//!   is bumped. The parent journal merges the bump via `merge_evm_state_optional_status`.

use std::vec::Vec;

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::ExecutionResult,
    sandbox::{
        calculate_keyless_deploy_address, decode_error_result,
        tests::{CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER, CREATE2_FACTORY_TX},
        KeylessDeployError,
    },
    test_utils::MemoryDatabase,
    IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, SaltEnv,
    TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS, MIN_BUCKET_SIZE,
};
use revm::{context::TxEnv, inspector::NoOpInspector, Database as _};

const RELAYER: Address = address!("0000000000000000000000000000000000990000");
const SIGNED_GAS_PRICE: u128 = 100_000_000_000; // 100 gwei, matches CREATE2_FACTORY_TX
const SIGNED_GAS_LIMIT: u64 = 100_000; // matches CREATE2_FACTORY_TX

// Rex5 caps `gasLimitOverride` to the outer call's remaining gas, so the happy-path
// sandbox tx needs enough headroom for the CREATE2 factory deploy's storage gas, not
// just for its raw EVM gas. 30M matches the standard block budget used elsewhere.
const OUTER_GAS_LIMIT: u64 = 30_000_000;

// Big enough to cover all sandbox costs in the happy path / runtime-revert path.
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;

/// Drives a keyless-deploy outer call and returns the outer execution result.
/// Funds RELAYER for the outer-call gas. Funds `signer` separately so the sandbox
/// fee-debit can succeed (when it runs).
fn run_keyless_outer(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: TestExternalEnvs<std::convert::Infallible>,
    keyless_tx_bytes: Bytes,
    gas_limit_override: u64,
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
        gas_limit: OUTER_GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    // Use `transact_commit` (not `transact_raw`) so the result's state is committed back into
    // the database — that lets the test query parent-visible state via `db.basic(...)` after
    // the call. `transact_raw` returns state without committing.
    alloy_evm::Evm::transact_commit(&mut evm, tx)
        .expect("outer keyless call should not fail at the EVM-error level")
}

/// Reads the current account info for `addr`.
fn account_info(db: &mut MemoryDatabase, addr: Address) -> revm::state::AccountInfo {
    db.basic(addr).expect("db read should succeed").unwrap_or_default()
}

/// Returns whether the address has non-empty contract code committed.
fn has_code(db: &mut MemoryDatabase, addr: Address) -> bool {
    let info = account_info(db, addr);
    if let Some(code) = info.code {
        !code.is_empty()
    } else {
        info.code_hash != revm::primitives::KECCAK_EMPTY
    }
}

/// Builds a deterministic pre-EIP-155 keyless-deploy transaction with the given init code.
/// Returns `(tx_bytes, recovered_signer)`. Mirrors the helper in
/// `tests/rex4/keyless_deploy.rs::create_pre_eip155_deploy_tx`.
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

// ============================================================================
// 1. Validate-reject path: outer call MUST revert AND signer MUST be untouched
// ============================================================================

/// REX5: a keyless-deploy attempt whose sandbox `validate()` rejects (final Mega-side
/// gas check overrun) must not charge the signer, must not bump the signer nonce,
/// must not install code at the deploy address, AND must surface as outer
/// `ExecutionResult::Revert`. Flipping any one of the four assertions indicates the
/// invariant has slipped: either the signer is being charged without a barrier, or
/// the validation rejection has been silently translated into an outer success.
#[test]
fn test_keyless_signer_charge_implies_replay_barrier() {
    let mut db = MemoryDatabase::default();

    // Fund the signer enough to cover several full-override fees so the sandbox's pre-check
    // (signer.balance >= gas_cost + value) doesn't trip *before* validate(). Validate must be
    // the layer that rejects.
    let signer_starting_balance = U256::from(1_000_000_000_000_000_000_000u128); // 1000 ETH
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, signer_starting_balance);
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    // Hot bucket on the deploy address blows the CREATE storage gas past gas_limit_override.
    let contract_bucket =
        <TestExternalEnvs as SaltEnv>::bucket_id_for_account(CREATE2_FACTORY_CONTRACT);
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new()
        .with_bucket_capacity(contract_bucket, MIN_BUCKET_SIZE as u64 * 2);

    // gasLimitOverride covers signed-tx gas (100k) but not the inflated CREATE storage gas.
    let gas_limit_override = 120_000;

    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        Bytes::from_static(CREATE2_FACTORY_TX),
        gas_limit_override,
    );

    // (a) outer call result is Revert.
    let revert_output = match &result {
        ExecutionResult::Revert { output, .. } => output.clone(),
        _ => panic!(
            "outer keyless-deploy call MUST revert when sandbox validate() rejects under REX5; got {result:?}",
        ),
    };

    // (a') the revert payload distinguishes a sandbox tx-validation rejection from a
    // genuine internal failure: the dedicated `IKeylessDeploy::InvalidTransaction`
    // selector decodes to `KeylessDeployError::InvalidTransaction`. A regression that
    // re-collapses the err arm back into `InternalError` would fail this assertion.
    let decoded =
        decode_error_result(&revert_output).expect("revert output must decode to a known error");
    assert!(
        matches!(decoded, KeylessDeployError::InvalidTransaction),
        "sandbox tx-validation rejection must surface as InvalidTransaction, got {decoded:?}",
    );

    // (b) signer balance unchanged
    let signer_after = account_info(&mut db, CREATE2_FACTORY_DEPLOYER);
    assert_eq!(
        signer_after.balance, signer_starting_balance,
        "signer must not be fee-debited when sandbox validate() rejects",
    );
    // (c) signer nonce unchanged at 0 (no CREATE frame ran, no nonce bump)
    assert_eq!(signer_after.nonce, 0, "signer nonce must not advance");
    // (d) deploy-address has no code
    assert!(
        !has_code(&mut db, CREATE2_FACTORY_CONTRACT),
        "deploy address must not be created when sandbox validate() rejects",
    );
}

/// REX4 (stable spec) must preserve the historical bug-shape behavior on the same input:
/// outer call returns `Success` with errorData (not Revert), the signer IS fee-debited,
/// and no replay barrier is installed (signer nonce stays 0, deploy address has no code).
/// This test fails loudly if any future change leaks the REX5 sandbox-validate rejection
/// back into a stable spec.
#[test]
fn test_rex4_keyless_preserves_signer_charge_without_barrier() {
    let mut db = MemoryDatabase::default();

    let signer_starting_balance = U256::from(1_000_000_000_000_000_000_000u128); // 1000 ETH
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, signer_starting_balance);
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let contract_bucket =
        <TestExternalEnvs as SaltEnv>::bucket_id_for_account(CREATE2_FACTORY_CONTRACT);
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new()
        .with_bucket_capacity(contract_bucket, MIN_BUCKET_SIZE as u64 * 2);

    let gas_limit_override = 120_000;

    let result = run_keyless_outer(
        MegaSpecId::REX4,
        &mut db,
        external_envs,
        Bytes::from_static(CREATE2_FACTORY_TX),
        gas_limit_override,
    );

    // Outer call surfaces as Success — the keyless interceptor returns success-with-error-data
    // on inner failure under stable specs, so the relayer is not penalized.
    assert!(
        result.is_success(),
        "REX4 keyless inner-fail path MUST keep returning outer Success; got {result:?}",
    );
    // Signer IS charged for the full sandbox gas budget (the historical drain shape).
    let signer_after = account_info(&mut db, CREATE2_FACTORY_DEPLOYER);
    assert!(
        signer_after.balance < signer_starting_balance,
        "REX4: signer balance must be reduced (stable bug-shape preserved)",
    );
    // No barrier installed: nonce stays at 0, deploy address has no code → still replayable.
    assert_eq!(signer_after.nonce, 0, "REX4: signer nonce must stay replayable");
    assert!(!has_code(&mut db, CREATE2_FACTORY_CONTRACT), "REX4: deploy address must remain empty",);
}

// ============================================================================
// 2. Runtime-revert path: signer charge AND nonce bump (barrier consumed)
// ============================================================================

/// REX5: a keyless-deploy whose constructor reverts at runtime *does* charge the
/// signer, because `make_create_frame` reaches the post-pre-bump nonce bump before
/// the constructor runs. The signer nonce is bumped in the sandbox state and merged
/// into the parent. If a future change ever skips the nonce bump on this path, this
/// test catches it immediately.
#[test]
fn test_keyless_signer_charge_consumes_replay_barrier_on_runtime_failure() {
    // Init code = `PUSH1 0x00 PUSH1 0x00 REVERT` → constructor reverts unconditionally.
    let revert_init_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]);
    let (keyless_tx_bytes, signer) = build_keyless_tx_with_init_code(revert_init_code);
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();

    // Signer has enough to pay the full sandbox gas budget even if the constructor fails.
    // 1000 ETH — large enough to cover `gas_limit_override × gas_price` (≈100 ETH) plus the
    // sandbox's pre-check headroom. Same convention as `tests/rex4/keyless_deploy.rs`.
    let signer_starting_balance = U256::from(1_000_000_000_000_000_000_000u128); // 1000 ETH
    db.set_account_balance(signer, signer_starting_balance);
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    // No hot bucket — validate() must accept so make_create_frame is reached.
    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();

    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        keyless_tx_bytes,
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    // Outer call must SUCCEED (the keyless deploy interceptor returns success-with-error-data
    // so the relayer is not penalized when the inner constructor reverts; the signer is the
    // one who paid for the inner gas).
    assert!(
        result.is_success(),
        "outer call must succeed even when inner constructor reverts; got {result:?}"
    );

    // Decode the keylessDeploy return value to confirm the inner constructor actually reverted.
    let output = match &result {
        ExecutionResult::Success { output, .. } => output.data().clone(),
        _ => panic!("expected success result"),
    };
    let decoded = IKeylessDeploy::keylessDeployCall::abi_decode_returns(&output)
        .expect("decode keylessDeploy return data");
    assert_eq!(
        decoded.deployedAddress,
        Address::ZERO,
        "deployedAddress must be zero when the inner constructor reverts",
    );
    assert!(!decoded.errorData.is_empty(), "errorData must carry the constructor revert payload",);

    // Replay-barrier assertions: signer nonce advanced AND signer balance reduced.
    let signer_after = account_info(&mut db, signer);
    assert_eq!(
        signer_after.nonce, 1,
        "signer nonce must advance to 1 after make_create_frame's nonce bump",
    );
    assert!(
        signer_after.balance < signer_starting_balance,
        "signer balance must be reduced by the sandbox gas cost",
    );

    // Deploy address must not have code (constructor reverted).
    assert!(
        !has_code(&mut db, deploy_address),
        "constructor reverted, so deploy address must not have code",
    );
}

// ============================================================================
// 3. Happy path under REX5 (sanity)
// ============================================================================

/// REX5: the canonical CREATE2 factory deploy still succeeds end-to-end and persists the
/// expected replay barriers (signer nonce + deploy-address code).
#[test]
fn test_keyless_successful_deploy_still_charges_and_advances_nonce() {
    let mut db = MemoryDatabase::default();
    // 1000 ETH — large enough to cover `gas_limit_override × gas_price` (≈100 ETH) plus the
    // sandbox's pre-check headroom. Same convention as `tests/rex4/keyless_deploy.rs`.
    let signer_starting_balance = U256::from(1_000_000_000_000_000_000_000u128); // 1000 ETH
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, signer_starting_balance);
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        external_envs,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    assert!(result.is_success(), "happy-path keyless deploy must succeed; got {result:?}");

    let signer_after = account_info(&mut db, CREATE2_FACTORY_DEPLOYER);
    assert_eq!(signer_after.nonce, 1, "signer nonce must advance to 1 on successful deploy");
    assert!(
        signer_after.balance < signer_starting_balance,
        "signer balance must be reduced by the sandbox gas cost",
    );
    assert!(
        has_code(&mut db, CREATE2_FACTORY_CONTRACT),
        "deploy address must have contract code after a successful deploy",
    );
}
