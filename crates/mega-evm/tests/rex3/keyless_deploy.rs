//! Tests for keyless deploy compute gas tracking in Rex3.
//!
//! Rex3 adds compute gas recording for the keyless deploy overhead (100K gas).
//! Before Rex3, keyless deploy bypassed compute gas tracking entirely.

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants,
    revm::context::result::ExecutionResult,
    sandbox::tests::{CREATE2_FACTORY_DEPLOYER, CREATE2_FACTORY_TX},
    test_utils::MemoryDatabase,
    EvmTxRuntimeLimits, IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{context::TxEnv, handler::EvmTr, inspector::NoOpInspector};

/// Standard test caller address.
const TEST_CALLER: Address = address!("0000000000000000000000000000000000100000");

/// Large gas limit override for tests (10 billion gas).
const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;

/// Helper function to execute a keyless deploy transaction and return the result
/// along with the compute gas used.
fn execute_keyless_deploy(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx_bytes: Bytes,
    gas_limit_override: u64,
) -> (ExecutionResult<MegaHaltReason>, u64) {
    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: tx_bytes,
        gasLimitOverride: U256::from(gas_limit_override),
    }
    .abi_encode();

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context = MegaContext::new(db, spec).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: 1_000_000_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let result = result_envelope.result;
    let compute_gas_used = evm.ctx_ref().additional_limit.borrow().get_usage().compute_gas;

    (result, compute_gas_used)
}

/// Test that keyless deploy overhead gas counts toward compute gas in Rex3.
#[test]
fn test_rex3_keyless_deploy_records_compute_gas() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let (result, compute_gas_used) = execute_keyless_deploy(
        MegaSpecId::REX3,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);

    // Compute gas should include the keyless deploy overhead (100K)
    assert!(
        compute_gas_used >= constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
        "Rex3 compute gas ({}) should include keyless deploy overhead ({})",
        compute_gas_used,
        constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
    );
}

/// Test that Rex2 keyless deploy does NOT count toward compute gas (backward compat).
#[test]
fn test_rex2_keyless_deploy_does_not_record_compute_gas() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let (result, compute_gas_used) = execute_keyless_deploy(
        MegaSpecId::REX2,
        &mut db,
        Bytes::from_static(CREATE2_FACTORY_TX),
        LARGE_GAS_LIMIT_OVERRIDE,
    );

    assert!(result.is_success(), "Transaction should succeed, got: {:?}", result);

    // In Rex2, keyless deploy returns early before compute gas tracking.
    // The only compute gas recorded should be intrinsic tx gas, NOT the keyless deploy overhead.
    assert!(
        compute_gas_used < constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
        "Rex2 compute gas ({}) should NOT include keyless deploy overhead ({})",
        compute_gas_used,
        constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
    );
}

/// Regression: a Rex3 keyless-deploy call whose intrinsic data size exceeds the TX
/// data-size limit must halt cleanly instead of aborting the node.
///
/// `before_tx_start` latches `DataLimitExceeded` from the outer calldata, but pre-Rex4
/// specs have no `frame_result_if_exceeding_limit` guard ahead of interceptor dispatch,
/// so the latch first reaches the keyless-deploy sandbox's overhead-gas charge. That
/// site used to assume the only reachable exceed was compute gas and hit an
/// `unreachable!()` (`compute_gas.check_limit()` returns `WithinLimit` on the
/// data-size path), panicking the node.
#[test]
fn test_rex3_keyless_deploy_data_size_overflow_halts_without_panic() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(CREATE2_FACTORY_TX),
        gasLimitOverride: U256::from(LARGE_GAS_LIMIT_OVERRIDE),
    }
    .abi_encode();

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context =
        MegaContext::new(&mut db, MegaSpecId::REX3).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: 1_000_000_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    // Force the outer calldata over the data-size limit so `before_tx_start` latches
    // `DataLimitExceeded` before the keyless-deploy interceptor runs.
    let runtime_limits =
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX3).with_tx_data_size_limit(64);
    let mut evm =
        MegaEvm::new(context).with_tx_runtime_limits(runtime_limits).with_inspector(NoOpInspector);

    // Before the fix this panics via `unreachable!()`; after the fix it returns a halt.
    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap().result;

    assert!(!result.is_success(), "expected a data-size limit halt, got: {result:?}");
    assert!(
        matches!(
            result,
            ExecutionResult::Halt { reason: MegaHaltReason::DataLimitExceeded { .. }, .. }
        ),
        "expected DataLimitExceeded halt, got: {result:?}",
    );

    // The remaining gas must be rescued for refund (as on the non-intercepted TX-level
    // exceed path): the sender is charged only intrinsic + keyless-deploy overhead, not
    // the full gas limit. Without the rescue the interceptor short-circuit burns the
    // entire 1e12 limit.
    assert!(
        result.gas_used() < 100_000_000,
        "expected remaining gas to be rescued (gas_used well below the 1e12 limit), got {}",
        result.gas_used(),
    );
}

/// Test that keyless deploy halts when compute gas limit is set below the 100K overhead.
/// In Rex3, the 100K overhead is recorded as compute gas, so if the compute gas limit
/// is lower than 100K, the transaction should fail.
#[test]
fn test_rex3_keyless_deploy_exceeds_compute_gas_limit() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(CREATE2_FACTORY_DEPLOYER, U256::from(1_000_000_000_000_000_000_000u128));

    let call_data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(CREATE2_FACTORY_TX),
        gasLimitOverride: U256::from(LARGE_GAS_LIMIT_OVERRIDE),
    }
    .abi_encode();

    let external_envs = TestExternalEnvs::<std::convert::Infallible>::new();
    let mut context =
        MegaContext::new(&mut db, MegaSpecId::REX3).with_external_envs((&external_envs).into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
        data: call_data.into(),
        value: U256::ZERO,
        gas_limit: 1_000_000_000_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    // Set compute gas limit to 50K, which is below the 100K keyless deploy overhead
    let runtime_limits =
        EvmTxRuntimeLimits::from_spec(MegaSpecId::REX3).with_tx_compute_gas_limit(50_000);
    let mut evm =
        MegaEvm::new(context).with_tx_runtime_limits(runtime_limits).with_inspector(NoOpInspector);
    let result_envelope = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    let result = result_envelope.result;

    // The transaction should fail because 100K overhead exceeds the 50K compute gas limit
    assert!(
        !result.is_success(),
        "Keyless deploy should fail when compute gas limit ({}) is below overhead ({})",
        50_000,
        constants::rex2::KEYLESS_DEPLOY_OVERHEAD_GAS,
    );
}
