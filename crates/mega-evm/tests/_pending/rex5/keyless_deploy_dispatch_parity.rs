//! Cross-spec admission parity for `KeylessDeployInterceptor`.
//!
//! Every input shape below must produce the same outer-transaction outcome on
//! REX4 and REX5. A spec-gated divergence in the interceptor's admission path
//! would fail one of these tests.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    revm::context::result::ExecutionResult, test_utils::MemoryDatabase, MegaContext, MegaEvm,
    MegaHaltReason, MegaSpecId, MegaTransaction, TestExternalEnvs, KEYLESS_DEPLOY_ADDRESS,
    KEYLESS_DEPLOY_CODE,
};
use revm::context::{result::ResultAndState, tx::TxEnvBuilder};

const CALLER: Address = address!("0000000000000000000000000000000000500000");

const KEYLESS_DEPLOY_SELECTOR: [u8; 4] = mega_evm::IKeylessDeploy::keylessDeployCall::SELECTOR;

fn run_dispatch(spec: MegaSpecId, calldata: Bytes) -> ResultAndState<MegaHaltReason> {
    // KEYLESS_DEPLOY_CODE is installed so non-intercepted calls fall through to
    // canonical on-chain bytecode (which reverts NotIntercepted()).
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000_000u64))
        .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE);

    let external_envs = TestExternalEnvs::<Infallible>::new();
    let mut context = MegaContext::new(&mut db, spec).with_external_envs(external_envs.into());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let tx_env = TxEnvBuilder::default()
        .caller(CALLER)
        .kind(TxKind::Call(KEYLESS_DEPLOY_ADDRESS))
        .gas_limit(30_000_000)
        .gas_price(0)
        .data(calldata)
        .build_fill();
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());

    let mut evm = MegaEvm::new(context);
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("transact must not surface a fatal EVMError")
}

// Result tag + payload bytes only — gas counters are excluded so the parity
// check is admission-shaped, not gas-accounting-shaped.
fn outcome_fingerprint(res: &ResultAndState<MegaHaltReason>) -> (&'static str, Vec<u8>) {
    match &res.result {
        ExecutionResult::Success { output, .. } => ("Success", output.data().to_vec()),
        ExecutionResult::Revert { output, .. } => ("Revert", output.to_vec()),
        ExecutionResult::Halt { reason, .. } => ("Halt", format!("{reason:?}").into_bytes()),
    }
}

#[test]
fn test_keyless_deploy_unknown_selector_falls_through_across_specs() {
    let calldata = Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]);
    let rex4 = outcome_fingerprint(&run_dispatch(MegaSpecId::REX4, calldata.clone()));
    let rex5 = outcome_fingerprint(&run_dispatch(MegaSpecId::REX5, calldata));
    assert_eq!(rex4.0, "Revert");
    assert_eq!(rex4, rex5);
}

#[test]
fn test_keyless_deploy_short_input_falls_through_across_specs() {
    for tail in [&[][..], &[0xab][..], &[0xab, 0xcd][..], &[0xab, 0xcd, 0xef][..]] {
        let calldata = Bytes::from(tail.to_vec());
        let rex4 = outcome_fingerprint(&run_dispatch(MegaSpecId::REX4, calldata.clone()));
        let rex5 = outcome_fingerprint(&run_dispatch(MegaSpecId::REX5, calldata));
        assert_eq!(rex4.0, "Revert", "len={}", tail.len());
        assert_eq!(rex4, rex5, "len={}", tail.len());
    }
}

#[test]
fn test_keyless_deploy_truncated_args_falls_through_across_specs() {
    let mut calldata = Vec::with_capacity(5);
    calldata.extend_from_slice(&KEYLESS_DEPLOY_SELECTOR);
    calldata.push(0xff);
    let calldata = Bytes::from(calldata);

    let rex4 = outcome_fingerprint(&run_dispatch(MegaSpecId::REX4, calldata.clone()));
    let rex5 = outcome_fingerprint(&run_dispatch(MegaSpecId::REX5, calldata));
    assert_eq!(rex4.0, "Revert");
    assert_eq!(rex4, rex5);
}
