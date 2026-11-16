//! Frame results `MegaETH` builds without running a frame carry the reservoir the frame
//! inherited and the give-back mode of the configuration the execution runs under, and settle like
//! revm's own.
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
    test_utils::{neutral_cfg, neutralize_evm, BytecodeBuilder, MemoryDatabase},
    EthSpecId, EvmTxRuntimeLimits, LimitUsage, MegaEvm,
};
use revm::{
    bytecode::opcode::{CALL, GAS, POP, PUSH0},
    context::{result::ExecutionResult, ContextTr, JournalTr},
    handler::{EvmTr, FrameResult, ItemOrResult},
    inspector::InspectorEvmTr,
    interpreter::{
        interpreter::SharedMemory, interpreter_action::FrameInit,
        interpreter_types::InterpreterTypes, CallInput, CallInputs, CallOutcome, CallScheme,
        CallValue, CreateInputs, CreateScheme, FrameInput, Gas, InstructionResult,
        InterpreterResult,
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

/// Every start past the call-stack limit is answered by the guard as revm answers it — a creation
/// and every call scheme alike — with the forwarded gas untouched and the inherited reservoir
/// carried, before anything reads its caller's account or counts its start: nothing is counted,
/// and the creator's nonce is not bumped, so no nonce record outlives the answer.
#[test]
fn test_depth_guard_answers_a_creation_and_every_call_scheme() {
    let past = CALL_STACK_LIMIT as usize + 1;
    let creation = FrameInit {
        depth: past,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Create(Box::new(CreateInputs::new(
            CALLER,
            CreateScheme::Create,
            U256::from(1),
            Bytes::new(),
            GAS_LIMIT,
            RESERVOIR,
        ))),
    };
    let mut inits = vec![creation];
    for (scheme, value) in [
        (CallScheme::Call, CallValue::Transfer(U256::from(1))),
        (CallScheme::CallCode, CallValue::Transfer(U256::from(1))),
        (CallScheme::DelegateCall, CallValue::Apparent(U256::from(1))),
        (CallScheme::StaticCall, CallValue::Transfer(U256::ZERO)),
    ] {
        let mut init = call_frame_init(past);
        if let FrameInput::Call(inputs) = &mut init.frame_input {
            inputs.scheme = scheme;
            inputs.value = value;
            inputs.is_static = scheme == CallScheme::StaticCall;
        }
        inits.push(init);
    }
    for init in inits {
        let what = format!("{:?}", init.frame_input);
        // The caller's account is never loaded: nothing reads it for a start the guard answers.
        let mut evm = MegaEvm::new(context(MemoryDatabase::default()));
        let result = EvmTr::frame_init(&mut evm, init).expect("frame_init does not fail");
        let ItemOrResult::Result(result) = result else { panic!("{what}: no frame is built") };
        let gas = result.gas();
        assert_eq!(result.instruction_result(), InstructionResult::CallTooDeep, "{what}");
        assert_eq!(gas.remaining(), GAS_LIMIT, "{what}: the forwarded gas is untouched");
        assert_eq!(gas.reservoir(), RESERVOIR, "{what}: the inherited reservoir is carried");
        if let FrameResult::Create(outcome) = &result {
            assert_eq!(outcome.address, None, "{what}");
        }
        assert_eq!(evm.ctx().limit_usage(), LimitUsage::ZERO, "{what}: counted");
        assert!(!evm.ctx().journal_ref().state.contains_key(&CALLER), "{what}: no nonce bumped");
    }
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

/// An inspector's answer to a call past the limit is taken as given, as revm's inspected frame
/// start takes it: the depth guard is the engine's frame start, which a frame the inspector
/// answers never reaches, so the answer is the inspector's `Stop` on the gas it built, and the
/// inspector sees the call end.
#[test]
fn test_an_inspectors_answer_past_the_depth_limit_is_taken_as_given() {
    let mut evm =
        MegaEvm::new(context(MemoryDatabase::default())).with_inspector(AlwaysAnswers::default());
    let result = InspectorEvmTr::inspect_frame_init(
        &mut evm,
        call_frame_init(CALL_STACK_LIMIT as usize + 1),
    )
    .expect("inspect_frame_init does not fail");
    let ItemOrResult::Result(FrameResult::Call(outcome)) = result else {
        panic!("the frame was answered")
    };
    assert_eq!(outcome.result.result, InstructionResult::Stop, "the inspector's answer");
    assert_eq!(outcome.result.gas.remaining(), GAS_LIMIT, "on the gas the inspector built");
    assert_eq!(outcome.result.gas.reservoir(), 0, "which carries no reservoir");
    assert_eq!(
        (evm.inspector().calls, evm.inspector().call_ends),
        (1, 1),
        "call and call_end pair"
    );
}

/// A first frame answered with the stop before it runs — its body fits the limit, the record and
/// the transfer log of its value do not — gives the whole reservoir back, and costs the same at a
/// gas limit under the execution cap and far above it. The stop gives back the new account and
/// the record charged before the frame into the transaction's reservoir: under the cap regular gas
/// paid them, so that is what its reservoir holds.
#[test]
fn test_stopped_first_frame_keeps_the_reservoir_at_any_gas_limit() {
    let at = |gas_limit: u64| {
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
        let limits =
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(mega_evm::TX_BODY_SIZE);
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
        let result =
            alloy_evm::Evm::transact_raw(&mut evm, call(CALLER, TARGET, U256::from(1), gas_limit))
                .unwrap();
        assert!(!result.result.is_success() && !result.result.is_halt(), "{:?}", result.result);
        (*result.result.gas(), evm.ctx().additional_limit().reservoir_remaining())
    };
    let ((small, small_reservoir), (large, large_reservoir)) = (at(100_000_000), at(1_000_000_000));
    let record = mega_evm::write_record_history_gas(1).expect("a record has a price");
    assert_eq!(small_reservoir, crate::common::account_state_gas() + record);
    assert_eq!(large_reservoir, 1_000_000_000 - TX_GAS_LIMIT_CAP - body_history(0));
    assert_eq!(small.tx_gas_used(), large.tx_gas_used());
    assert_eq!(small.state_gas_spent_final(), large.state_gas_spent_final());
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
        let result = crate::common::execute(system_db(), tx);
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
        (result, output)
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
    // A value call that is refused gives back the history its transfer's record was charged before
    // the frame, into the reservoir: below the cap, where regular gas paid it, that is the pool.
    let given_back = if !value.is_zero() && matches!(expected, Expected::Revert(_)) {
        mega_evm::write_record_history_gas(1).expect("a record has a price")
    } else {
        0
    };
    assert_eq!(
        narrow.gas.reservoir_remaining, given_back,
        "there is no pool below the cap but what was given back"
    );
    assert_eq!(
        wide.gas.reservoir_remaining,
        WIDE - TX_GAS_LIMIT_CAP - body_history(data.len() as u64),
        "the call spends no state gas, so the whole pool but the body's history comes back",
    );
    assert_eq!(
        narrow.result.gas().tx_gas_used(),
        wide.result.gas().tx_gas_used(),
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
/// first action, with the reservoir it inherited carried: the frame revm built for it ends before
/// it runs, and its frame start returns the out-of-gas as an answer is returned. revm's frame
/// return hands its caller none of the call's regular gas back and the reservoir whole, as for
/// any frame that ran out.
#[test]
fn test_a_keyless_call_below_the_overhead_runs_out_of_gas() {
    use core::convert::Infallible;

    use revm::context::result::EVMError;

    let forwarded = KEYLESS_DEPLOY_OVERHEAD_GAS - 1;
    let mut evm = MegaEvm::new(context(system_db()));
    // The accounts a transaction's validation and its first frame input load.
    let journal = evm.ctx_mut().journal_mut();
    journal.load_account(CALLER).unwrap();
    journal.load_account_with_code(KEYLESS_DEPLOY_ADDRESS).unwrap();
    let init = call_frame_init_to(0, KEYLESS_DEPLOY_ADDRESS, keyless_deploy_call(), forwarded);
    let ItemOrResult::Result(result) =
        EvmTr::frame_init(&mut evm, init).expect("frame_init does not fail")
    else {
        panic!("a call that cannot pay the overhead starts no creation");
    };
    assert_eq!(EvmTr::all(&evm).3.index(), None, "the call's frame is off the stack");
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
    let before = *caller_frame.interpreter.gas.tracker();
    // The call's opcode handed the answer a copy of the caller's reservoir, which the answer's
    // return puts back in its place.
    caller_frame.return_result::<_, EVMError<Infallible>>(ctx, result).unwrap();
    let after = caller_frame.interpreter.gas.tracker();
    assert_eq!(after.remaining(), before.remaining(), "a halt hands none of its gas back");
    assert_eq!(after.reservoir(), RESERVOIR, "and the inherited reservoir comes back whole");
    assert_eq!(after.state_gas_spent(), before.state_gas_spent());
}

/// The configuration an execution runs under: Satin's own, whose give-backs credit the reservoir
/// alone, or the neutral one the execution-spec gate runs Ethereum's fixtures under, which credits
/// them as EIP-8037 does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Configuration {
    Satin,
    Neutral,
}

impl Configuration {
    const BOTH: [Self; 2] = [Self::Satin, Self::Neutral];

    /// The fork whose pricing the neutral configuration takes: Amsterdam, whose EIP-8037 has a
    /// reservoir to give back into.
    const NEUTRAL_FORK: EthSpecId = EthSpecId::AMSTERDAM;

    /// Whether every give-back of the configuration credits the reservoir alone: the mode every
    /// tracker built under it must carry.
    const fn reservoir_only_give_backs(self) -> bool {
        matches!(self, Self::Satin)
    }

    /// An EVM over `db` under this configuration, held to `limits`, inspected by `inspector`.
    fn evm<INSP>(
        self,
        db: MemoryDatabase,
        limits: EvmTxRuntimeLimits,
        inspector: INSP,
    ) -> MegaEvm<MemoryDatabase, INSP> {
        let mut ctx = context(db).with_tx_runtime_limits(limits);
        if self == Self::Neutral {
            ctx = ctx.with_neutral_cfg(neutral_cfg(Self::NEUTRAL_FORK).expect("a neutral fork"));
        }
        let mut evm = MegaEvm::new(ctx).with_inspector(inspector);
        if self == Self::Neutral {
            neutralize_evm(&mut evm, Self::NEUTRAL_FORK).expect("a neutral fork");
        }
        evm
    }
}

/// Keeps, for every call that ends, its target, how it ended and the give-back mode of the gas
/// its result carries, as an inspector sees the result in `call_end`.
#[derive(Default)]
struct SeesGiveBackModes {
    ends: Vec<(Address, InstructionResult, bool)>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SeesGiveBackModes {
    fn call_end(&mut self, _: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let mode = outcome.result.gas.reservoir_only_give_backs();
        self.ends.push((inputs.target_address, outcome.result.result, mode));
    }
}

/// Runs `tx` over `db` under `configuration` and `limits`, and returns what the transaction's own
/// frame ended as and the give-back mode its result carried when the inspector saw it.
fn own_frame_end(
    configuration: Configuration,
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: mega_evm::MegaTransaction,
) -> (Bytes, InstructionResult, bool) {
    let to = tx.0.base.kind.to().copied().expect("a call");
    let mut evm = configuration.evm(db, limits, SeesGiveBackModes::default());
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    let output = outcome.result.output().cloned().unwrap_or_default();
    let ends = &evm.inspector().ends;
    let [(target, result, mode)] = ends.as_slice() else {
        panic!("{configuration:?}: the transaction's own frame alone ends: {ends:?}")
    };
    assert_eq!(*target, to, "{configuration:?}");
    (output, *result, *mode)
}

/// The answer a system contract interceptor or the keyless dispatch gives a call carries the give-
/// back mode of the configuration the execution runs under when an inspector sees it, as the gas
/// of every frame revm builds does: give-backs to the reservoir alone under Satin's, EIP-8037's
/// own under the neutral one. An inspector that charges and gives back on the answer's gas
/// credits it where the configuration says, whoever built the result.
///
/// Every row is an answer the engine builds without running the contract's code, which the output
/// tells apart from a fall-through into the code, whose frame revm builds with the mode anyway:
/// the methods the two control contracts answer, the refusal of a value they take none of, a
/// `keylessDeploy` call carrying value, and a hint the Oracle answers with the stop because its
/// payload would cross the transaction's data-size limit.
#[test]
fn test_an_interceptor_answer_carries_the_configured_give_back_mode() {
    let selector = |selector: [u8; 4]| Bytes::copy_from_slice(&selector);
    let hint = Bytes::from(
        IOracle::sendHintCall { topic: B256::ZERO, data: Bytes::from_static(b"a hint") }
            .abi_encode(),
    );
    let no_room_for_a_hint = EvmTxRuntimeLimits::no_limits()
        .with_tx_data_size_limit(mega_evm::TX_BODY_SIZE + hint.len() as u64);
    let rows = [
        (
            LIMIT_CONTROL_ADDRESS,
            selector(IMegaLimitControl::remainingComputeGasCall::SELECTOR),
            U256::ZERO,
            EvmTxRuntimeLimits::no_limits(),
            InstructionResult::Return,
            None,
        ),
        (
            ACCESS_CONTROL_ADDRESS,
            selector(IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR),
            U256::ZERO,
            EvmTxRuntimeLimits::no_limits(),
            InstructionResult::Return,
            Some(Bytes::from(
                IMegaAccessControl::isVolatileDataAccessDisabledCall::abi_encode_returns(&false),
            )),
        ),
        (
            ACCESS_CONTROL_ADDRESS,
            selector(IMegaAccessControl::disableVolatileDataAccessCall::SELECTOR),
            U256::ZERO,
            EvmTxRuntimeLimits::no_limits(),
            InstructionResult::Return,
            Some(Bytes::new()),
        ),
        (
            LIMIT_CONTROL_ADDRESS,
            selector(IMegaLimitControl::remainingComputeGasCall::SELECTOR),
            U256::from(1),
            EvmTxRuntimeLimits::no_limits(),
            InstructionResult::Revert,
            Some(Bytes::from_static(&IMegaAccessControl::NonZeroTransfer::SELECTOR)),
        ),
        (
            KEYLESS_DEPLOY_ADDRESS,
            keyless_deploy_call(),
            U256::from(1),
            EvmTxRuntimeLimits::no_limits(),
            InstructionResult::Revert,
            Some(Bytes::from_static(&IKeylessDeploy::NoEtherTransfer::SELECTOR)),
        ),
        (
            ORACLE_CONTRACT_ADDRESS,
            hint,
            U256::ZERO,
            no_room_for_a_hint,
            InstructionResult::Revert,
            None,
        ),
    ];
    for configuration in Configuration::BOTH {
        for (to, data, value, limits, expected, expected_output) in rows.clone() {
            let case = format!("{configuration:?}, {to}, {:?}", &data[..4]);
            let mut tx = crate::common::call_with_data(CALLER, to, data, NARROW);
            tx.0.base.value = value;
            let (output, result, mode) = own_frame_end(configuration, system_db(), limits, tx);
            assert_eq!(result, expected, "{case}: {output}");
            match (to, expected_output) {
                (_, Some(expected_output)) => assert_eq!(output, expected_output, "{case}"),
                // The answer is the caller's forward: no word the bytecode returns.
                (LIMIT_CONTROL_ADDRESS, None) => assert_eq!(output.len(), 32, "{case}"),
                // The stop: `MegaLimitExceeded` of the data size.
                (_, None) => assert_eq!(
                    mega_evm::decode_mega_limit_exceeded(&output).map(|(kind, _)| kind),
                    Some(mega_evm::LimitKind::DataSize),
                    "{case}: {output}"
                ),
            }
            assert_eq!(mode, configuration.reservoir_only_give_backs(), "{case}");
        }
    }
}

/// The stops the engine answers a frame with in place of what revm or the frame would have made
/// carry the configured give-back mode when an inspector sees them: a call revm answered with a
/// success whose start crossed a limit, rebuilt into the stop on the gas its caller forwarded, and
/// the transaction's own frame stopped before it runs because its authorization list was skipped
/// for a limit.
#[test]
fn test_an_engine_stop_carries_the_configured_give_back_mode() {
    let authority = address!("0000000000000000000000000000000000300077");
    for configuration in Configuration::BOTH {
        // The transfer's record and log cross a data-size limit the body alone fits: revm
        // answers the call to an account without code with a success, which gives way to the
        // stop.
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
        let body_only =
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(mega_evm::TX_BODY_SIZE);
        let tx = call(CALLER, TARGET, U256::from(1), NARROW);
        let (output, result, mode) = own_frame_end(configuration, db, body_only, tx);
        assert_eq!(result, InstructionResult::Revert, "{configuration:?}: the answer's stop");
        assert!(mega_evm::decode_mega_limit_exceeded(&output).is_some(), "{configuration:?}");
        assert_eq!(mode, configuration.reservoir_only_give_backs(), "{configuration:?}");

        // An authorization that would create its authority, under a state-growth limit of
        // nothing: the list is skipped and the first frame answered with the stop.
        let db = MemoryDatabase::default().account_balance(CALLER, U256::from(1_000));
        let no_growth = EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(0);
        let tx = crate::common::authorizing_call(
            CALLER,
            TARGET,
            U256::ZERO,
            NARROW,
            TARGET,
            &[(authority, 0)],
        );
        let (output, result, mode) = own_frame_end(configuration, db, no_growth, tx);
        assert_eq!(result, InstructionResult::Revert, "{configuration:?}: the list's stop");
        assert!(mega_evm::decode_mega_limit_exceeded(&output).is_some(), "{configuration:?}");
        assert_eq!(mode, configuration.reservoir_only_give_backs(), "{configuration:?}");
    }
}

/// A synthetic frame result — here the depth guard's `CallTooDeep`, to a call and to a creation —
/// carries the configured give-back mode, beside the forwarded gas untouched and the inherited
/// reservoir.
#[test]
fn test_a_synthetic_frame_result_carries_the_configured_give_back_mode() {
    let past = CALL_STACK_LIMIT as usize + 1;
    let creation = || FrameInit {
        depth: past,
        memory: SharedMemory::new(),
        frame_input: FrameInput::Create(Box::new(CreateInputs::new(
            CALLER,
            CreateScheme::Create,
            U256::ZERO,
            Bytes::new(),
            GAS_LIMIT,
            RESERVOIR,
        ))),
    };
    for configuration in Configuration::BOTH {
        for init in [call_frame_init(past), creation()] {
            let what = format!("{configuration:?}, {:?}", init.frame_input);
            let mut evm = configuration.evm(
                MemoryDatabase::default(),
                EvmTxRuntimeLimits::no_limits(),
                SeesGiveBackModes::default(),
            );
            let result = EvmTr::frame_init(&mut evm, init).expect("frame_init does not fail");
            let ItemOrResult::Result(result) = result else { panic!("{what}: answered") };
            assert_eq!(result.instruction_result(), InstructionResult::CallTooDeep, "{what}");
            let gas = result.gas();
            assert_eq!((gas.remaining(), gas.reservoir()), (GAS_LIMIT, RESERVOIR), "{what}");
            assert_eq!(gas.reservoir_only_give_backs(), configuration.reservoir_only_give_backs());
        }
    }
}

/// The forward of the call [`queries_remaining_compute_gas`] makes.
const QUERY_FORWARD: u64 = 20_000;

/// What [`ChargesAndGivesBack`] charges the answer's gas, and gives back at once.
const INSPECTOR_CHARGE: u64 = 1_000;

/// Code that calls `MegaLimitControl.remainingComputeGas()`, forwarding [`QUERY_FORWARD`], and
/// returns what `GAS` reads once the call returned.
fn queries_remaining_compute_gas() -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, IMegaLimitControl::remainingComputeGasCall::SELECTOR)
        .append_many([PUSH0, PUSH0])
        .push_number(4_u8)
        .append_many([PUSH0, PUSH0])
        .push_address(LIMIT_CONTROL_ADDRESS)
        .push_number(QUERY_FORWARD)
        .append_many([CALL, POP, GAS])
        .return_top()
        .build()
}

/// Charges [`INSPECTOR_CHARGE`] of history on the gas of the answer to every call to
/// `MegaLimitControl` once the call ended, through the tracker's own methods, and gives it back
/// at once; it keeps the answer's regular gas and reservoir afterwards. It neither replaces the
/// tracker nor sets its give-back mode.
#[derive(Default)]
struct ChargesAndGivesBack {
    after: Option<(u64, u64)>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for ChargesAndGivesBack {
    fn call_end(&mut self, _: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if inputs.target_address != LIMIT_CONTROL_ADDRESS {
            return;
        }
        let gas = &mut outcome.result.gas;
        assert!(gas.record_history_cost(INSPECTOR_CHARGE), "the answer holds its forward");
        gas.refill_history(INSPECTOR_CHARGE);
        self.after = Some((gas.remaining(), gas.reservoir()));
    }
}

/// An inspector that charges history on an interceptor's answer and gives it back credits the
/// give-back where the configuration says, as on the gas of a frame revm built. Under Satin's
/// give-backs to the reservoir alone the charge, which regular gas paid, comes back into the
/// answer's reservoir: the caller resumes with that much less regular gas, which `GAS` reads, and
/// that much more reservoir, which goes back to the sender, so the receipt is the same. Under the
/// neutral configuration it comes back into regular gas, as EIP-8037 has it, and the caller reads
/// what it reads without the inspector.
#[test]
fn test_an_inspectors_give_back_on_an_interceptor_answer_follows_the_configuration() {
    const GAS_LIMIT: u64 = 1_000_000;
    let db = || system_db().account_code(TARGET, queries_remaining_compute_gas());
    let tx = || crate::common::call(CALLER, TARGET, U256::ZERO, GAS_LIMIT);
    let read_gas = |outcome: &mega_evm::MegaTransactionOutcome| {
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        let output = outcome.result.output().cloned().unwrap_or_default();
        U256::from_be_slice(&output).to::<u64>()
    };
    for configuration in Configuration::BOTH {
        let limits = EvmTxRuntimeLimits::no_limits();
        let mut plain = configuration.evm(db(), limits, SeesGiveBackModes::default());
        let without = plain.execute_transaction(tx()).expect("the transaction is valid");
        let mut evm = configuration.evm(db(), limits, ChargesAndGivesBack::default());
        let with = evm.execute_transaction(tx()).expect("the transaction is valid");

        let after = evm.inspector().after.expect("the inspector saw the answer");
        let gas_read = read_gas(&with);
        if configuration.reservoir_only_give_backs() {
            assert_eq!(after, (QUERY_FORWARD - INSPECTOR_CHARGE, INSPECTOR_CHARGE), "Satin");
            assert_eq!(gas_read, read_gas(&without) - INSPECTOR_CHARGE, "Satin");
        } else {
            assert_eq!(after, (QUERY_FORWARD, 0), "neutral");
            assert_eq!(gas_read, read_gas(&without), "neutral");
        }
        assert_eq!(
            with.result.gas().tx_gas_used(),
            without.result.gas().tx_gas_used(),
            "{configuration:?}: the receipt is the same",
        );
    }
}
