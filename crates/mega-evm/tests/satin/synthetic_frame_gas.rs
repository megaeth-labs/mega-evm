//! Frame results `MegaETH` builds without running a frame carry the reservoir the frame
//! inherited and settle like revm's own.
//!
//! The matrix at the end runs every method a system contract interceptor answers at a gas limit
//! under the execution cap, where a transaction has no reservoir, and at one far above it, where
//! the reservoir is the whole difference. One branch is not reachable that way: a `keylessDeploy`
//! call forwarded less regular gas than the fixed overhead. A transaction carries a reservoir only
//! when its gas limit is above the execution cap, and then its own frame is forwarded the cap
//! itself, which is far above the overhead — so that branch is reached through `frame_init`
//! instead.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use alloy_sol_types::{SolCall, SolError};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    system::{
        keyless::{
            IKeylessDeploy, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE,
            KEYLESS_DEPLOY_OVERHEAD_GAS,
        },
        IMegaAccessControl, IMegaLimitControl, IOracle, ACCESS_CONTROL_ADDRESS,
        ACCESS_CONTROL_CODE, LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE, ORACLE_CONTRACT_ADDRESS,
        ORACLE_CONTRACT_CODE,
    },
    test_utils::MemoryDatabase,
    EvmTxRuntimeLimits, LimitKind, MegaEvm,
};
use revm::{
    context::{result::ExecutionResult, ContextTr, JournalTr},
    handler::{EvmTr, FrameResult, ItemOrResult},
    inspector::InspectorEvmTr,
    interpreter::{
        interpreter::SharedMemory, interpreter_action::FrameInit,
        interpreter_types::InterpreterTypes, CallInput, CallInputs, CallOutcome, CallScheme,
        CallValue, FrameInput, Gas, InstructionResult, InterpreterResult,
    },
    primitives::CALL_STACK_LIMIT,
    Inspector,
};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000300010");
const TARGET: Address = address!("0000000000000000000000000000000000300001");
const GAS_LIMIT: u64 = 100_000;
const RESERVOIR: u64 = 7_000_000;

/// The history gas the body of a transaction carrying `calldata_len` bytes costs. It is charged
/// before the first frame and comes out of the reservoir, so a transaction with a pool starts
/// with this much less of one. It is read at the price the engine runs.
fn body_history(calldata_len: u64) -> u64 {
    mega_evm::history_gas(mega_evm::TX_BODY_SIZE + calldata_len).expect("a body has a price")
}

fn call_frame_init(depth: usize) -> FrameInit {
    FrameInit {
        depth,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(Bytes::new()),
            return_memory_offset: 0..0,
            gas_limit: GAS_LIMIT,
            bytecode_address: TARGET,
            known_bytecode: Default::default(),
            target_address: TARGET,
            caller: CALLER,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            reservoir: RESERVOIR,
            charged_new_account_state_gas: false,
        })),
    }
}

/// [`call_frame_init`] carrying `data` to `to`, with `gas_limit` of regular gas.
fn call_frame_init_to(depth: usize, to: Address, data: Bytes, gas_limit: u64) -> FrameInit {
    let mut init = call_frame_init(depth);
    if let FrameInput::Call(inputs) = &mut init.frame_input {
        inputs.target_address = to;
        inputs.bytecode_address = to;
        inputs.input = CallInput::Bytes(data);
        inputs.gas_limit = gas_limit;
    }
    init
}

fn assert_call_too_deep(result: &FrameResult) {
    let FrameResult::Call(outcome) = result else { panic!("expected a call result: {result:?}") };
    assert_eq!(outcome.result.result, InstructionResult::CallTooDeep);
    assert_eq!(outcome.result.gas.remaining(), GAS_LIMIT, "the forwarded gas is untouched");
    assert_eq!(outcome.result.gas.reservoir(), RESERVOIR, "the inherited reservoir is carried");
    assert_eq!(outcome.result.gas.state_gas_spent(), 0);
}

/// A call past the call-stack limit is answered with `CallTooDeep` before anything could
/// intercept it, with the forwarded gas untouched and the inherited reservoir carried.
#[test]
fn test_depth_guard_returns_call_too_deep_with_the_inherited_reservoir() {
    let mut evm = MegaEvm::new(context(MemoryDatabase::default()));
    let result = EvmTr::frame_init(&mut evm, call_frame_init(CALL_STACK_LIMIT as usize + 1))
        .expect("frame_init does not fail");
    let ItemOrResult::Result(result) = result else { panic!("no frame is built past the limit") };
    assert_call_too_deep(&result);
}

/// `CALL_STACK_LIMIT` itself is the last permitted depth: the guard does not fire there.
#[test]
fn test_depth_boundary_allows_call_at_limit() {
    let mut evm = MegaEvm::new(context(MemoryDatabase::default()));
    let journal = evm.ctx_mut().journal_mut();
    journal.load_account(CALLER).unwrap();
    journal.load_account(TARGET).unwrap();
    let result = EvmTr::frame_init(&mut evm, call_frame_init(CALL_STACK_LIMIT as usize))
        .expect("frame_init does not fail");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("a call to an account without code is answered at once");
    };
    assert_eq!(outcome.result.result, InstructionResult::Stop, "the call went through");
    assert_eq!(outcome.result.gas.reservoir(), RESERVOIR);
}

/// Answers every call itself, and counts the calls and their ends.
#[derive(Default)]
struct AlwaysAnswers {
    calls: usize,
    call_ends: usize,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for AlwaysAnswers {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.calls += 1;
        Some(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Stop,
                Bytes::new(),
                Gas::new(inputs.gas_limit),
            ),
            inputs.return_memory_offset.clone(),
        ))
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.call_ends += 1;
    }
}

/// An inspector's answer to a call past the limit gives way to `CallTooDeep`, and the inspector
/// still sees the call end.
#[test]
fn test_inspect_frame_init_depth_guard_overrides_inspector() {
    let mut evm =
        MegaEvm::new(context(MemoryDatabase::default())).with_inspector(AlwaysAnswers::default());
    let result = InspectorEvmTr::inspect_frame_init(
        &mut evm,
        call_frame_init(CALL_STACK_LIMIT as usize + 1),
    )
    .expect("inspect_frame_init does not fail");
    let ItemOrResult::Result(result) = result else { panic!("the frame was answered") };
    assert_call_too_deep(&result);
    assert_eq!(
        (evm.inspector().calls, evm.inspector().call_ends),
        (1, 1),
        "call and call_end pair"
    );
}

/// A latched transaction's frame is answered with the stop, even past the call-stack limit: the
/// latch is checked first, and its answer carries the reservoir too.
#[test]
fn test_exceeded_tx_limit_wins_over_call_too_deep() {
    let mut evm = MegaEvm::new(context(MemoryDatabase::default()));
    let stop = evm.ctx_mut().additional_limit_mut().latch(LimitKind::KVUpdate, 0, 1);
    let result = EvmTr::frame_init(&mut evm, call_frame_init(CALL_STACK_LIMIT as usize + 1))
        .expect("frame_init does not fail");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("a latched transaction's frame is answered");
    };
    assert_eq!(outcome.result.result, InstructionResult::Revert, "the stop, not CallTooDeep");
    assert_eq!(outcome.result.output, stop.revert_data());
    assert_eq!(outcome.result.gas.remaining(), GAS_LIMIT);
    assert_eq!(outcome.result.gas.reservoir(), RESERVOIR);
}

/// A first frame answered with the stop before it runs gives the whole reservoir back, and costs
/// the same at a gas limit under the execution cap and far above it.
#[test]
fn test_stopped_first_frame_keeps_the_reservoir_at_any_gas_limit() {
    let at = |gas_limit: u64| {
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
        let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(0);
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
        let result =
            alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, TARGET, U256::from(1), gas_limit))
                .unwrap();
        assert!(!result.result.is_success() && !result.result.is_halt(), "{:?}", result.result);
        *result.result.gas()
    };
    let (small, large) = (at(100_000_000), at(1_000_000_000));
    assert_eq!(small.reservoir_remaining(), 0);
    assert_eq!(large.reservoir_remaining(), 1_000_000_000 - TX_GAS_LIMIT_CAP - body_history(0));
    assert_eq!(small.tx_gas_used(), large.tx_gas_used());
    assert_eq!(small.state_gas_spent_final(), large.state_gas_spent_final());
}

/// `settle_frame_result` settles a result into its caller's gas exactly as revm's frame return
/// does, for a success, a revert and a halt, with state gas drawn from the reservoir and spilled
/// onto regular gas.
#[test]
fn test_settle_frame_result_matches_revms_frame_return() {
    use mega_evm::{settle_frame_result, synthetic_frame_result};
    use revm::context::result::EVMError;

    for result_kind in
        [InstructionResult::Stop, InstructionResult::Revert, InstructionResult::OutOfGas]
    {
        let code = Bytes::from_static(&[revm::bytecode::opcode::STOP]);
        let mut evm =
            MegaEvm::new(context(MemoryDatabase::default().account_code(TARGET, code.clone())));
        let journal = evm.ctx_mut().journal_mut();
        journal.load_account(CALLER).unwrap();
        journal.load_account(TARGET).unwrap();
        let mut caller_init = call_frame_init(1);
        if let FrameInput::Call(inputs) = &mut caller_init.frame_input {
            let bytecode = revm::state::Bytecode::new_raw(code.clone());
            inputs.known_bytecode = (bytecode.hash_slow(), bytecode);
        }
        let ItemOrResult::Item(_) = EvmTr::frame_init(&mut evm, caller_init).unwrap() else {
            panic!("the caller frame is built");
        };

        // The child the caller starts: 30,000 gas of the caller's, and its reservoir.
        let (ctx, _, _, frames) = EvmTr::all_mut(&mut evm);
        let caller_frame = frames.get();
        assert!(caller_frame.interpreter.gas.record_regular_cost(30_000));
        let child_input = {
            let FrameInput::Call(mut inputs) = call_frame_init(2).frame_input else {
                unreachable!()
            };
            inputs.gas_limit = 30_000;
            inputs.reservoir = caller_frame.interpreter.gas.reservoir();
            FrameInput::Call(inputs)
        };
        let mut result = synthetic_frame_result(&child_input, result_kind, Bytes::new());
        // The child spent regular gas, and state gas beyond the reservoir that spilled.
        let child_gas = result.gas_mut();
        assert!(child_gas.record_regular_cost(1_000));
        assert!(child_gas.record_state_cost(RESERVOIR + 500));

        let mut expected = *caller_frame.interpreter.gas.tracker();
        settle_frame_result::<_, EVMError<core::convert::Infallible>>(
            ctx,
            &mut expected,
            &mut result.clone(),
        )
        .unwrap();
        caller_frame.return_result::<_, EVMError<core::convert::Infallible>>(ctx, result).unwrap();
        assert_eq!(caller_frame.interpreter.gas.tracker(), &expected, "{result_kind:?}");
    }
}

/// A gas limit below the execution cap: the transaction has no state-gas reservoir.
const NARROW: u64 = 100_000_000;

/// A gas limit far above the execution cap: everything above the cap is the reservoir.
const WIDE: u64 = 1_000_000_000;

/// What a call to a system contract must end as.
///
/// The gas comparison alone cannot tell an interceptor's answer from a fall-through into the
/// contract's own bytecode — both cost the same at both gas limits — so every row of the matrix
/// says which of the two it expects, and says it at both limits.
#[derive(Clone, Debug)]
enum Expected {
    /// The call succeeded with exactly these bytes.
    Answer(Bytes),
    /// The call reverted with exactly this data.
    Revert(Bytes),
    /// `remainingComputeGas`, called by the transaction itself, answers the regular gas the
    /// transaction's own frame was forwarded, the one answer of the matrix that depends on the
    /// transaction's gas limit: under the execution cap it is
    /// the transaction's limit less what pre-execution took, above the cap it is the cap's. So
    /// the two answers differ by what the cap holds back at the wider limit, and by the body's
    /// history, which the narrow limit has no reservoir to pay from.
    ForwardedRegularGas,
}

/// Runs the same call to `to` at both gas limits and requires `expected` at both, the reservoir
/// to come back whole and the two to cost the same: an answer that carried no reservoir would
/// hand the caller an empty pool and bill the sender for all of it.
fn assert_the_reservoir_survives(to: Address, data: Bytes, value: U256, expected: Expected) {
    let at = |gas_limit: u64| {
        let mut tx = crate::common::call_with_data(CALLER, to, data.clone(), gas_limit);
        tx.0.base.value = value;
        let (result, _) = crate::common::run(system_db(), tx);
        let output = result.result.output().cloned().unwrap_or_default();
        match &expected {
            Expected::Answer(answer) => {
                assert!(result.result.is_success(), "at {gas_limit}: {:?}", result.result);
                assert_eq!(&output, answer, "at a gas limit of {gas_limit}");
            }
            Expected::Revert(revert_data) => {
                assert!(
                    matches!(result.result, ExecutionResult::Revert { .. }),
                    "at {gas_limit}: {:?}",
                    result.result,
                );
                assert_eq!(&output, revert_data, "at a gas limit of {gas_limit}");
            }
            Expected::ForwardedRegularGas => {
                assert!(result.result.is_success(), "at {gas_limit}: {:?}", result.result);
            }
        }
        (*result.result.gas(), output)
    };
    let ((narrow, narrow_answer), (wide, wide_answer)) = (at(NARROW), at(WIDE));

    if matches!(expected, Expected::ForwardedRegularGas) {
        let forwarded = |answer: &Bytes| {
            IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(answer)
                .expect("the answer is the method's own return type")
        };
        assert_eq!(
            forwarded(&wide_answer) - forwarded(&narrow_answer),
            TX_GAS_LIMIT_CAP - NARROW + body_history(data.len() as u64),
            "the answer is the forwarded regular gas, which the execution cap holds back; the \
             narrow limit has no reservoir, so its body's history comes off that budget too",
        );
    }
    assert_eq!(narrow.reservoir_remaining(), 0, "there is no pool below the cap");
    assert_eq!(
        wide.reservoir_remaining(),
        WIDE - TX_GAS_LIMIT_CAP - body_history(data.len() as u64),
        "the call spends no state gas, so the whole pool but the body's history comes back",
    );
    assert_eq!(
        narrow.tx_gas_used(),
        wide.tx_gas_used(),
        "the call costs the same with a pool and without one",
    );
}

/// A database holding what a call to a system contract needs: the contracts' code, a balance for
/// the sender, and an account without code, which a transaction reaches for its intrinsic cost
/// alone.
fn system_db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_000_u64))
        .account_balance(TARGET, U256::from(1))
        .account_balance(ACCESS_CONTROL_ADDRESS, U256::from(1))
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
        .account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE)
        .account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE)
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
}

/// The calldata of a `keylessDeploy` call the dispatch recognises.
fn keyless_deploy_call() -> Bytes {
    Bytes::from(
        IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: Bytes::from_static(b"a transaction"),
            gasLimitOverride: U256::from(1_000_000),
        }
        .abi_encode(),
    )
}

/// Every method a system contract interceptor answers carries the reservoir the frame inherited,
/// and answers what its own ABI names rather than falling through to the contract's bytecode.
#[test]
fn test_an_intercepted_answer_keeps_the_reservoir() {
    let answers_nothing = Expected::Answer(Bytes::new());
    for (to, selector, expected) in [
        (
            ACCESS_CONTROL_ADDRESS,
            IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR,
            answers_nothing.clone(),
        ),
        (
            ACCESS_CONTROL_ADDRESS,
            IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR,
            answers_nothing,
        ),
        (
            ACCESS_CONTROL_ADDRESS,
            IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR,
            Expected::Answer(Bytes::from(
                IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(&false),
            )),
        ),
        (
            LIMIT_CONTROL_ADDRESS,
            IMegaLimitControl::remainingComputeGasCall::SELECTOR,
            Expected::ForwardedRegularGas,
        ),
    ] {
        assert_the_reservoir_survives(to, Bytes::copy_from_slice(&selector), U256::ZERO, expected);
    }
}

/// So does the refusal of a call that carries value to a method that takes none, on every method
/// of the two control contracts.
#[test]
fn test_an_intercepted_refusal_keeps_the_reservoir() {
    let refused =
        Expected::Revert(Bytes::from_static(&IMegaAccessControl::NonZeroTransfer::SELECTOR));
    for (to, selector) in [
        (ACCESS_CONTROL_ADDRESS, IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR),
        (ACCESS_CONTROL_ADDRESS, IMegaAccessControl::enableVolatileDataAccessCall::SELECTOR),
        (ACCESS_CONTROL_ADDRESS, IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR),
        (LIMIT_CONTROL_ADDRESS, IMegaLimitControl::remainingComputeGasCall::SELECTOR),
    ] {
        assert_the_reservoir_survives(
            to,
            Bytes::copy_from_slice(&selector),
            U256::from(1),
            refused.clone(),
        );
    }
}

/// The Oracle's hint is the one dispatch that answers nothing: it forwards the payload and lets
/// the contract's own bytecode run. What the matrix pins for it is that the pools reach that
/// bytecode untouched.
#[test]
fn test_a_forwarded_hint_keeps_the_reservoir() {
    let data = Bytes::from(
        IOracle::sendHintCall { topic: B256::ZERO, data: Bytes::from_static(b"a hint") }
            .abi_encode(),
    );
    assert_the_reservoir_survives(
        ORACLE_CONTRACT_ADDRESS,
        data,
        U256::ZERO,
        Expected::Answer(Bytes::new()),
    );
}

/// A dispatched `keylessDeploy` call is charged its overhead out of regular gas, so it costs the
/// same whether there is a pool or not and the pool comes back whole. A call the rules refuse —
/// here one whose bytes are not a signed transaction — is answered after the overhead with the
/// rule's error, and the answer carries the reservoir.
#[test]
fn test_the_keyless_overhead_comes_out_of_regular_gas() {
    assert_the_reservoir_survives(
        KEYLESS_DEPLOY_ADDRESS,
        keyless_deploy_call(),
        U256::ZERO,
        Expected::Revert(Bytes::from_static(&IKeylessDeploy::MalformedEncoding::SELECTOR)),
    );
}

/// A `keylessDeploy` call that carries value is answered by the dispatch itself, with the ABI's
/// own `NoEtherTransfer()` and after the overhead was charged: the answer carries the reservoir,
/// and the overhead is charged once, at either gas limit.
#[test]
fn test_the_keyless_value_refusal_keeps_the_reservoir() {
    let data = keyless_deploy_call();
    assert_the_reservoir_survives(
        KEYLESS_DEPLOY_ADDRESS,
        data.clone(),
        U256::from(1),
        Expected::Revert(Bytes::from_static(&IKeylessDeploy::NoEtherTransfer::SELECTOR)),
    );

    // The same transaction to an account without code spends its intrinsic cost and nothing
    // else, and the refusal is answered before a frame runs, so what the dispatched call spends
    // beyond it is the overhead — once — less the one thing the refusal takes back: the history
    // of the write record a transfer that goes through leaves on its recipient.
    let record = mega_evm::write_record_history_gas(1).expect("a record has a price");
    let spent = |to: Address, gas_limit: u64| {
        let mut tx = crate::common::call_with_data(CALLER, to, data.clone(), gas_limit);
        tx.0.base.value = U256::from(1);
        crate::common::run(system_db(), tx).0.result.gas().total_gas_spent()
    };
    for gas_limit in [NARROW, WIDE] {
        assert_eq!(
            spent(KEYLESS_DEPLOY_ADDRESS, gas_limit) + record - spent(TARGET, gas_limit),
            KEYLESS_DEPLOY_OVERHEAD_GAS,
            "at a gas limit of {gas_limit}",
        );
    }
}

/// A `keylessDeploy` call forwarded less regular gas than the overhead runs out of gas on its
/// first run, with the reservoir it inherited carried, and that result settles into a caller
/// exactly as revm's own frame return settles a frame that ran out.
#[test]
fn test_a_keyless_call_below_the_overhead_runs_out_of_gas() {
    use core::convert::Infallible;

    use mega_evm::settle_frame_result;
    use revm::context::result::EVMError;

    let forwarded = KEYLESS_DEPLOY_OVERHEAD_GAS - 1;
    let mut evm = MegaEvm::new(context(system_db()));
    // The accounts a transaction's validation and its first frame input load.
    let journal = evm.ctx_mut().journal_mut();
    journal.load_account(CALLER).unwrap();
    journal.load_account_with_code(KEYLESS_DEPLOY_ADDRESS).unwrap();
    let init = call_frame_init_to(0, KEYLESS_DEPLOY_ADDRESS, keyless_deploy_call(), forwarded);
    let ItemOrResult::Item(_) =
        EvmTr::frame_init(&mut evm, init).expect("frame_init does not fail")
    else {
        panic!("the call's frame is built");
    };
    let ItemOrResult::Result(result) = EvmTr::frame_run(&mut evm).expect("frame_run does not fail")
    else {
        panic!("a call that cannot pay the overhead starts no creation");
    };
    let FrameResult::Call(outcome) = &result else { panic!("expected a call result: {result:?}") };
    assert_eq!(outcome.result.result, InstructionResult::OutOfGas);
    assert!(outcome.result.output.is_empty());
    assert_eq!(outcome.result.gas.remaining(), 0, "an out-of-gas answer spends what it had");
    assert_eq!(outcome.result.gas.reservoir(), RESERVOIR, "the inherited reservoir is carried");

    // The caller the answer goes back to, built as the settlement test above builds one.
    let code = Bytes::from_static(&[revm::bytecode::opcode::STOP]);
    let mut evm = MegaEvm::new(context(system_db().account_code(TARGET, code.clone())));
    let journal = evm.ctx_mut().journal_mut();
    journal.load_account(CALLER).unwrap();
    journal.load_account(TARGET).unwrap();
    let mut caller_init = call_frame_init(1);
    if let FrameInput::Call(inputs) = &mut caller_init.frame_input {
        let bytecode = revm::state::Bytecode::new_raw(code);
        inputs.known_bytecode = (bytecode.hash_slow(), bytecode);
    }
    let ItemOrResult::Item(_) = EvmTr::frame_init(&mut evm, caller_init).unwrap() else {
        panic!("the caller frame is built");
    };

    let (ctx, _, _, frames) = EvmTr::all_mut(&mut evm);
    let caller_frame = frames.get();
    let mut expected = *caller_frame.interpreter.gas.tracker();
    settle_frame_result::<_, EVMError<Infallible>>(ctx, &mut expected, &mut result.clone())
        .unwrap();
    caller_frame.return_result::<_, EVMError<Infallible>>(ctx, result).unwrap();
    assert_eq!(caller_frame.interpreter.gas.tracker(), &expected);
}
