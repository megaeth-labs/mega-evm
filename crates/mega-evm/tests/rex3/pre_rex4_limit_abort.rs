//! Pre-REX4 side of the "abort before the frame is short-circuited" guards.
//!
//! REX4 added two early aborts that run when a TX-level resource limit is already exceeded:
//! one in `frame_init` ahead of system-contract interceptor dispatch, one in `inspect_frame_init`
//! ahead of an inspector's synthetic output. Both short-circuit paths bypass
//! `AdditionalLimit::before_frame_init`, so before REX4 the exceed reaches the transaction result
//! without ever going through the gas-rescue path, and the whole gas limit is consumed.
//!
//! `tests/rex4/intrinsic_limit_bypass.rs` pins the REX4 behavior (halt plus rescued gas). This
//! module pins the frozen REX3 shape both guards must not retroactively change: the transaction
//! still fails, but it burns the entire gas limit.

use mega_evm::MegaTransaction;
use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    alloy_op_evm::OpTxError,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, IKeylessDeploy, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransactionNew as _, KEYLESS_DEPLOY_ADDRESS,
};
use revm::{
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        ContextTr, TxEnv,
    },
    inspector::Inspector,
    interpreter::{
        interpreter_types::InterpreterTypes, CallInputs, CallOutcome, Gas, InstructionResult,
        InterpreterResult,
    },
    primitives::TxKind,
};

const CALLER: Address = address!("0000000000000000000000000000000000300100");
const CALLEE: Address = address!("0000000000000000000000000000000000300101");
const GAS_LIMIT: u64 = 100_000_000;

/// A REX3 context whose compute-gas budget is one unit, so `validate`'s intrinsic
/// `record_compute_gas` latches a TX-level exceed before the first frame is ever created.
fn exhausted_context(
    db: MemoryDatabase,
) -> MegaContext<MemoryDatabase, mega_evm::EmptyExternalEnv> {
    let mut context = MegaContext::new(db, MegaSpecId::REX3)
        .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_compute_gas_limit(1));
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    context
}

fn call_tx(to: Address, data: Bytes) -> MegaTransaction {
    let mut tx = MegaTransaction::new(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data,
        gas_limit: GAS_LIMIT,
        gas_price: 0,
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

fn assert_compute_exceeded_without_rescue(result: &ExecutionResult<MegaHaltReason>) {
    assert!(
        matches!(
            result,
            ExecutionResult::Halt { reason: MegaHaltReason::ComputeGasLimitExceeded { .. }, .. }
        ),
        "expected a compute-gas exceed halt, got {result:?}",
    );
    assert_eq!(
        result.tx_gas_used(),
        GAS_LIMIT,
        "pre-REX4 must burn the whole gas limit: the exceed is only detected at \
         last_frame_result, after the short-circuit produced its result, so no gas is rescued",
    );
}

/// `frame_init`: a REX3 transaction targeting the keyless-deploy system contract still reaches
/// the interceptor even though a TX-level limit is already exceeded. The interceptor's synthetic
/// result skips `before_frame_init`, so the exceed only surfaces at `last_frame_result` and the
/// full gas limit is spent.
#[test]
fn test_rex3_interceptor_runs_despite_exceeded_tx_limit() {
    // A well-formed `keylessDeploy(bytes,uint256)` call whose payload is not a decodable
    // pre-EIP-155 transaction: the interceptor matches the selector and returns a revert
    // result, which is all this test needs from it.
    let data = IKeylessDeploy::keylessDeployCall {
        keylessDeploymentTransaction: Bytes::from_static(&[0xFFu8; 8]),
        gasLimitOverride: U256::ZERO,
    }
    .abi_encode();

    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000_000u64));
    let mut evm = MegaEvm::new(exhausted_context(db));

    let result: Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, OpTxError>> =
        alloy_evm::Evm::transact_raw(&mut evm, call_tx(KEYLESS_DEPLOY_ADDRESS, data.into()));
    assert_compute_exceeded_without_rescue(&result.expect("tx must not error out").result);
}

/// Inspector that short-circuits every call with a synthetic `Stop`, the same shape
/// `tests/rex4/intrinsic_limit_bypass.rs` uses to drive `inspect_frame_init`'s early return.
struct SkipAllCallsInspector;

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipAllCallsInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        Some(CallOutcome::new(
            InterpreterResult {
                result: InstructionResult::Stop,
                output: Bytes::new(),
                gas: Gas::new(inputs.gas_limit),
            },
            0..0,
        ))
    }
}

/// `inspect_frame_init`: the same frozen shape on the inspector path. REX3 hands the inspector's
/// synthetic output straight back, so the exceed is again only detected at `last_frame_result`.
#[test]
fn test_rex3_inspector_output_survives_exceeded_tx_limit() {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000u64))
        .account_code(CALLEE, BytecodeBuilder::default().stop().build());

    let mut inspector = SkipAllCallsInspector;
    let mut evm = MegaEvm::new(exhausted_context(db)).with_inspector(&mut inspector);

    let result = alloy_evm::Evm::transact_raw(&mut evm, call_tx(CALLEE, Bytes::new()))
        .expect("tx must not error out");
    assert_compute_exceeded_without_rescue(&result.result);
}
