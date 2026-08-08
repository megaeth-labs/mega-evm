//! The inspected execution loop must meter exactly like the plain one.
//!
//! `frame_run` and `inspect_frame_run` are two hand-maintained copies of the same body. Both
//! snapshot `interpreter_result.gas.remaining()` before `process_next_action` so the gas the
//! action itself consumes — for a CREATE, revm's `return_create` code-deposit charge — is recorded
//! as compute gas afterwards. A drop of that snapshot on the inspected copy alone would silently
//! under-meter every inspected CREATE.
//!
//! REX4 is the spec that exercises the snapshot on the CREATE path: REX5+ pre-charges the
//! code-deposit compute gas before the action runs and deliberately passes `None` here instead.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    alloy_op_evm::OpTxError, test_utils::MemoryDatabase, EvmTxRuntimeLimits, LimitUsage,
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    context::{result::ResultAndState, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::TxKind,
};

const CALLER: Address = address!("0000000000000000000000000000000000400200");
/// Runtime code length the init code below deploys.
const DEPLOYED_LEN: u64 = 64;
/// revm's per-byte code-deposit gas (`revm::interpreter::gas::CODEDEPOSIT`).
const CODEDEPOSIT: u64 = 200;

/// Init code that returns `DEPLOYED_LEN` zero bytes: `PUSH1 len; PUSH0; RETURN`.
fn init_code() -> Bytes {
    Bytes::from(vec![0x60, DEPLOYED_LEN as u8, 0x5F, 0xF3])
}

fn create_tx() -> TxEnv {
    TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        data: init_code(),
        gas_limit: 1_000_000_000,
        gas_price: 0,
        ..Default::default()
    }
}

fn run(inspected: bool) -> (ResultAndState<MegaHaltReason>, LimitUsage) {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000u64));
    let mut context = MegaContext::new(db, MegaSpecId::REX4)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });

    let mut tx = MegaTransaction::new(create_tx());
    tx.enveloped_tx = Some(Bytes::new());

    // Both arms must produce the same `MegaEvm` type, so build the inspected one by toggling
    // the inspector flag rather than by changing the inspector type.
    let mut evm = MegaEvm::new(context).with_inspector(NoOpInspector);
    if !inspected {
        alloy_evm::Evm::set_inspector_enabled(&mut evm, false);
    }
    let result: Result<ResultAndState<MegaHaltReason>, mega_evm::EVMError<Infallible, OpTxError>> =
        alloy_evm::Evm::transact_raw(&mut evm, tx);
    let result = result.expect("CREATE tx must execute");
    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    (result, usage)
}

/// A REX4 CREATE must record the same compute gas whether or not an inspector is attached, and
/// that total must include revm's code-deposit charge — which is only observable through the
/// pre-`process_next_action` gas snapshot.
#[test]
fn test_rex4_create_compute_gas_matches_with_and_without_inspector() {
    let (plain_result, plain_usage) = run(false);
    let (inspected_result, inspected_usage) = run(true);

    assert!(plain_result.result.is_success(), "got {:?}", plain_result.result);
    assert!(inspected_result.result.is_success(), "got {:?}", inspected_result.result);

    let code_deposit_gas = DEPLOYED_LEN * CODEDEPOSIT;
    assert!(
        plain_usage.compute_gas >= code_deposit_gas,
        "the plain loop must meter the code-deposit gas ({code_deposit_gas}); \
         compute_gas was {}",
        plain_usage.compute_gas,
    );
    assert_eq!(
        inspected_usage.compute_gas, plain_usage.compute_gas,
        "the inspected loop must meter a CREATE identically to the plain loop; \
         dropping the pre-action gas snapshot loses exactly {code_deposit_gas} gas",
    );
    assert_eq!(
        inspected_result.result.tx_gas_used(),
        plain_result.result.tx_gas_used(),
        "inspected and plain runs must consume the same gas",
    );
}
