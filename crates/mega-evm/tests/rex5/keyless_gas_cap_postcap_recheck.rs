//! Regression suite for the REX5+ `KeylessDeploy` post-cap gas-limit recheck.
//!
//! In Step 4b of `execute_keyless_deploy_call`, REX5 caps `gas_limit_override`
//! to the outer's `gas.remaining()`. After the cap, the wrapper re-enforces
//! the signer's "execute with at least `tx_gas_limit`" guarantee: if the
//! relayer's outer envelope shrinks the override below the signed minimum,
//! the call rejects with `KeylessDeployError::GasLimitTooLow` reporting the
//! capped value. Pre-REX5 does not apply the cap, so this recheck never
//! fires under pre-REX5 specs.

use std::{convert::Infallible, vec::Vec};

use alloy_primitives::{address, hex, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_consensus::{Signed, TxLegacy},
    revm::context::result::ExecutionResult,
    sandbox::{calculate_keyless_deploy_address, decode_error_result, KeylessDeployError},
    test_utils::MemoryDatabase,
    BucketHasher, IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, SimpleBucketHasher, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS, MIN_BUCKET_SIZE,
};
use revm::{context::TxEnv, inspector::NoOpInspector, Database as _};

const RELAYER: Address = address!("0000000000000000000000000000000000990000");
const SIGNED_GAS_PRICE: u128 = 100_000_000_000; // 100 gwei
const SIGNED_GAS_LIMIT: u64 = 100_000;
/// Large enough that the Step 4 pre-cap check (`override >= tx_gas_limit`) passes.
const LARGE_OVERRIDE: u64 = 10_000_000_000;
/// Outer envelope sized so that, after the intrinsic charge and the
/// `KEYLESS_DEPLOY_OVERHEAD_GAS = 100_000` Step-1 debit, the remaining gas at
/// Step 4b lands below `SIGNED_GAS_LIMIT`. Pre-funding the signer keeps the
/// materialization charge at zero so the math stays predictable.
const TIGHT_OUTER_GAS: u64 = 200_000;
/// Outer envelope big enough that the Step 4b cap does not shrink the override
/// below `tx_gas_limit`. Used by the happy-path assertion.
const NORMAL_OUTER_GAS: u64 = 30_000_000;

/// Init code that returns a single-byte `STOP` runtime (so the deploy survives
/// the post-execution `EmptyCodeDeployed` check).
const STOP_RUNTIME_INIT_CODE: &[u8] = &[
    0x60, 0x00, // PUSH1 0x00
    0x60, 0x00, // PUSH1 0x00
    0x52, // MSTORE
    0x60, 0x01, // PUSH1 0x01
    0x60, 0x1f, // PUSH1 0x1f
    0xf3, // RETURN
];

fn build_keyless_tx() -> (Bytes, Address) {
    let tx = TxLegacy {
        nonce: 0,
        gas_price: SIGNED_GAS_PRICE,
        gas_limit: SIGNED_GAS_LIMIT,
        to: TxKind::Create,
        value: U256::ZERO,
        input: Bytes::from_static(STOP_RUNTIME_INIT_CODE),
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
    let signer = signed.recover_signer().expect("signer recovery");
    (Bytes::from(buf), signer)
}

fn run_keyless_outer(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    keyless_tx_bytes: Bytes,
    gas_limit_override: u64,
    outer_gas_limit: u64,
) -> ExecutionResult<MegaHaltReason> {
    run_keyless_outer_with_envs(
        spec,
        db,
        TestExternalEnvs::<Infallible>::new(),
        keyless_tx_bytes,
        gas_limit_override,
        outer_gas_limit,
    )
}

fn run_keyless_outer_with_envs(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    external_envs: TestExternalEnvs<Infallible>,
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
        .expect("outer keyless call should not surface EVMError")
}

/// REX5: a tight outer envelope drops `gas.remaining()` below the signed
/// `tx_gas_limit` at Step 4b. The Step-4 pre-cap check passes (the relayer
/// supplied a large override), the Step-4b cap shrinks the override to
/// `gas.remaining()`, and the post-cap recheck must reject with
/// `GasLimitTooLow` reporting the capped (smaller) value.
#[test]
fn test_rex5_envelope_shrinks_below_signed_min_returns_gas_limit_too_low() {
    let (keyless_tx_bytes, signer) = build_keyless_tx();
    let deploy_address = calculate_keyless_deploy_address(signer);

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // Pre-fund signer (non-empty) so `charge_caller_materialization_pre_sandbox`
    // is a no-op and the outer-gas math is dominated by intrinsic +
    // KEYLESS_DEPLOY_OVERHEAD_GAS.
    db.set_account_balance(signer, U256::from(1));

    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        keyless_tx_bytes,
        LARGE_OVERRIDE,
        TIGHT_OUTER_GAS,
    );

    let revert_output = match &result {
        ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("expected Revert(GasLimitTooLow), got {other:?}"),
    };
    let err = decode_error_result(&revert_output).expect("revert output must decode");
    match err {
        KeylessDeployError::GasLimitTooLow { tx_gas_limit, provided_gas_limit } => {
            assert_eq!(
                tx_gas_limit, SIGNED_GAS_LIMIT,
                "tx_gas_limit must echo the signed keyless tx's gas_limit",
            );
            assert!(
                provided_gas_limit < tx_gas_limit,
                "post-cap recheck must report the capped value (< tx_gas_limit); got \
                 provided_gas_limit = {provided_gas_limit}",
            );
            assert!(
                provided_gas_limit < LARGE_OVERRIDE,
                "post-cap recheck must report the capped value, not the relayer-supplied \
                 override; got provided_gas_limit = {provided_gas_limit}",
            );
        }
        other => panic!("expected GasLimitTooLow, got {other:?}"),
    }

    // Signer untouched, no deploy.
    let signer_info = db.basic(signer).unwrap().unwrap_or_default();
    assert_eq!(signer_info.nonce, 0, "signer nonce must remain 0");
    let deploy_info = db.basic(deploy_address).unwrap();
    assert!(
        deploy_info.is_none() ||
            deploy_info.as_ref().unwrap().code_hash == revm::primitives::KECCAK_EMPTY,
        "no deployment must happen",
    );
}

/// REX5: a normal-sized outer envelope leaves `gas.remaining()` above
/// `tx_gas_limit` after the Step-4b cap, so the post-cap recheck does not
/// fire and the deploy proceeds normally.
#[test]
fn test_rex5_envelope_above_signed_min_proceeds_normally() {
    let (keyless_tx_bytes, signer) = build_keyless_tx();

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    db.set_account_balance(signer, U256::from(1));

    let result = run_keyless_outer(
        MegaSpecId::REX5,
        &mut db,
        keyless_tx_bytes,
        LARGE_OVERRIDE,
        NORMAL_OUTER_GAS,
    );

    assert!(
        result.is_success(),
        "with a normal outer envelope the deploy must succeed; got {result:?}",
    );
}

/// REX4 parity: same envelope-too-small setup must NOT produce the post-cap
/// `GasLimitTooLow` shape under the pre-REX5 spec (no cap is applied, so the
/// recheck never sees a capped value smaller than `tx_gas_limit`). Any other
/// outcome — sandbox OOG, balance precheck, etc. — is acceptable as the
/// baseline; we just pin that the new REX5 path is genuinely REX5-gated.
#[test]
fn test_rex4_envelope_too_small_does_not_fire_post_cap_recheck() {
    let (keyless_tx_bytes, signer) = build_keyless_tx();

    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // Fund signer enough to cover the REX4 sandbox's `gas_limit × gas_price`
    // balance precheck when the sandbox runs uncapped at `LARGE_OVERRIDE`.
    db.set_account_balance(signer, U256::from(100_000_000_000_000_000_000u128));

    let result = run_keyless_outer(
        MegaSpecId::REX4,
        &mut db,
        keyless_tx_bytes,
        LARGE_OVERRIDE,
        TIGHT_OUTER_GAS,
    );

    if let ExecutionResult::Revert { output, .. } = &result {
        if let Some(KeylessDeployError::GasLimitTooLow { tx_gas_limit, provided_gas_limit }) =
            decode_error_result(output)
        {
            assert!(
                provided_gas_limit >= tx_gas_limit,
                "REX4 must not produce the post-cap-capped GasLimitTooLow shape; \
                 got provided_gas_limit ({provided_gas_limit}) < tx_gas_limit ({tx_gas_limit})",
            );
        }
    }
}

/// Post-cap recheck must fire on the materialization-drain shape: outer envelope is
/// fat enough to clear intrinsic + `KEYLESS_DEPLOY_OVERHEAD_GAS` without dipping below
/// `tx_gas_limit`, but `charge_caller_materialization_pre_sandbox` for an unfunded
/// signer drains the remaining gas under the signed minimum at Step 4b.
///
/// Setup:
/// 1. Outer envelope is fat — comfortably above intrinsic + overhead. With a pre-funded signer the
///    deploy would succeed (verified by the control case below).
/// 2. Signer is un-funded (`AccountInfo::is_empty()` → true), so
///    `charge_caller_materialization_pre_sandbox` debits `new_account_gas`.
/// 3. Signer's SALT bucket carries a heavy multiplier so `new_account_gas` is large enough to push
///    `gas.remaining()` below `SIGNED_GAS_LIMIT` at Step 4b.
///
/// The recheck must surface as `GasLimitTooLow` with the capped value. A control with
/// the same envelope but a pre-funded signer must succeed — isolating materialization
/// as the cause of the drain.
#[test]
fn test_rex5_materialization_drains_gas_below_signed_min_triggers_recheck() {
    /// `NEW_ACCOUNT_STORAGE_GAS_BASE * (multiplier - 1)` = `25_000 * 19 = 475_000`.
    /// Picked so that the gap between "post intrinsic + overhead remaining" and
    /// `tx_gas_limit` (100k) is bridged by materialization across a wide intrinsic
    /// window — the `MegaETH` outer intrinsic for this ~200-byte calldata lands near
    /// 90k (EIP-2028 + `CALLDATA_STANDARD_TOKEN_STORAGE_GAS` +
    /// `TX_INTRINSIC_STORAGE_GAS`), not the bare 21k EIP-2028 base, so the test
    /// uses a generous margin to stay robust against any minor calldata change.
    const HEAVY_MULTIPLIER: u64 = 20;
    const HEAVY_CAPACITY: u64 = (MIN_BUCKET_SIZE as u64) * HEAVY_MULTIPLIER;
    /// Fat enough that intrinsic (≈90k) + `KEYLESS_DEPLOY_OVERHEAD_GAS` (100k)
    /// alone leaves remaining gas well above `tx_gas_limit` (100k). The recheck
    /// only fires once materialization piles on the additional 475k.
    const FAT_OUTER_GAS: u64 = 700_000;

    let (keyless_tx_bytes, signer) = build_keyless_tx();
    let signer_bucket = SimpleBucketHasher::bucket_id(signer.as_slice());

    // Materialization-drain leg: signer unfunded → empty signer_info → materialization fires.
    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    // Do NOT fund signer here.
    let envs =
        TestExternalEnvs::<Infallible>::new().with_bucket_capacity(signer_bucket, HEAVY_CAPACITY);
    let result = run_keyless_outer_with_envs(
        MegaSpecId::REX5,
        &mut db,
        envs,
        keyless_tx_bytes.clone(),
        LARGE_OVERRIDE,
        FAT_OUTER_GAS,
    );
    let revert_output = match &result {
        ExecutionResult::Revert { output, .. } => output.clone(),
        other => panic!("expected Revert(GasLimitTooLow), got {other:?}"),
    };
    let err = decode_error_result(&revert_output).expect("revert output must decode");
    match err {
        KeylessDeployError::GasLimitTooLow { tx_gas_limit, provided_gas_limit } => {
            assert_eq!(tx_gas_limit, SIGNED_GAS_LIMIT);
            assert!(
                provided_gas_limit < tx_gas_limit,
                "post-cap recheck must report a capped value below tx_gas_limit; \
                 got provided_gas_limit = {provided_gas_limit}",
            );
            assert!(
                provided_gas_limit < LARGE_OVERRIDE,
                "must report the Step-4b-capped value, not the relayer-supplied override",
            );
        }
        other => panic!("expected GasLimitTooLow, got {other:?}"),
    }

    // Control leg: same envelope and same heavy bucket, but signer pre-funded so the
    // materialization charge is a no-op. The deploy must succeed, isolating
    // materialization as the cause of the drain in the case above.
    let mut db = MemoryDatabase::default();
    db.set_account_balance(RELAYER, U256::from(1_000_000_000u64));
    db.set_account_balance(signer, U256::from(1));
    let envs =
        TestExternalEnvs::<Infallible>::new().with_bucket_capacity(signer_bucket, HEAVY_CAPACITY);
    let control = run_keyless_outer_with_envs(
        MegaSpecId::REX5,
        &mut db,
        envs,
        keyless_tx_bytes,
        LARGE_OVERRIDE,
        FAT_OUTER_GAS,
    );
    assert!(
        control.is_success(),
        "control with pre-funded signer at the same fat envelope must succeed — \
         isolates materialization as the cause of the drain in the failing leg; got {control:?}",
    );
}
