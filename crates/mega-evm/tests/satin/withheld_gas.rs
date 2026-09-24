//! Gas detention against every other reader of a frame's gas, and against every out-of-gas.
//!
//! A detained frame's spendable gas is held at what the limit leaves the transaction, and the rest
//! is withheld: only a regular charge sees the difference. `GAS`, the 63/64 forward, the `SSTORE`
//! sentry, the skip-cold checks and a callee's inheritance see the whole regular gas. So a
//! transaction that reads volatile data runs as it does without the read — the same result and the
//! same bill on every ledger — unless its compute crosses the limit, which is a regular charge the
//! withheld part would have paid. That is the stop, and nothing else is.
//!
//! Each case runs twice: with a read of the block's timestamp, and with `PUSH0`, which costs the
//! same two gas and reads nothing, in its place.

use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    system::keyless::{IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_OVERHEAD_GAS},
    test_utils::{op_transaction, MemoryDatabase},
    EvmTxRuntimeLimits, MegaEvm,
};
use revm::context::TxEnv;

use crate::detention::{assert_stopped, context, run_on, Calls, BELOW, BENEFICIARY, CALLER};

/* ---------- answers ---------- */

/// An interceptor that charges its frame by taking gas off the frame's limit and then answers is
/// held to the allowance as a precompile is: a `keylessDeploy` call carrying value, from the
/// beneficiary under a cap below the keyless overhead, stops at the cap instead of answering. The
/// stop's gas is the frame's as its caller forwarded it, so a tracer reading the answer sees the
/// allowance spent and the rest left.
#[test]
fn test_an_interceptors_charge_is_held_to_what_the_limit_leaves() {
    let calldata: Bytes = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(b"a transaction"),
        gasLimitOverride: U256::ZERO,
    }
    .abi_encode()
    .into();
    let cap = KEYLESS_DEPLOY_OVERHEAD_GAS / 2;
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
    let run = |caller: Address| {
        let db = MemoryDatabase::default().account_balance(caller, U256::from(10));
        let tx = OpTx(op_transaction(TxEnv {
            caller,
            kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
            gas_limit: BELOW,
            value: U256::from(1),
            data: calldata.clone(),
            ..Default::default()
        }));
        let evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
        let mut evm = evm.with_inspector(Calls::default());
        let run = run_on(&mut evm, tx);
        (run, evm.inspector().calls[0].spent)
    };
    let ((detained, stop_spent), (plain, answer_spent)) = (run(BENEFICIARY), run(CALLER));
    assert_eq!(answer_spent, 0, "the answer spent nothing of the limit it was left");
    assert_eq!(stop_spent, cap, "the stop spent the allowance of the limit it was forwarded");
    assert_eq!(
        plain.outcome.result.output(),
        Some(&Bytes::from(IKeylessDeploy::NoEtherTransfer::SELECTOR.to_vec())),
        "the answer without detention"
    );
    assert_eq!(detained.limit, Some(cap), "the sender is the beneficiary");
    let intrinsic = plain.outcome.gas.regular - KEYLESS_DEPLOY_OVERHEAD_GAS;
    assert_stopped(&detained, intrinsic);
}
