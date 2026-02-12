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
    IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
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
