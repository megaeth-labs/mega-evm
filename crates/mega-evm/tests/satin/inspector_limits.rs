//! What an inspector's writes to gas cannot do.
//!
//! - Gas changed at a frame's end has no effect: the instruction that ends a frame copies the
//!   frame's gas into the result it returns, so what `step_end` writes to the interpreter's gas
//!   after it reaches nobody. What a frame returns is changed on its result, in `call_end`.
//! - An inspector cannot overdraw a frame: a charge the frame's spendable gas cannot pay is refused
//!   and takes nothing, the frame runs on, and nothing is left behind to classify its end — not
//!   even under gas detention, where the same refusal of an opcode's charge is the crossing.
//! - An answer carries only the regular gas its inspector chose: the frame never ran, so its caller
//!   merges back the reservoir it forwarded, whatever `Gas` the answer was built on.

use alloy_primitives::{address, Address, Bytes, Log, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    untouched_call_gas, with_pools_of, EvmTxRuntimeLimits, MegaContext, MegaEvm,
    MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{LOG0, POP, PUSH0, TIMESTAMP},
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::LoopControl, CallInputs, CallOutcome, Gas,
        InstructionResult, Interpreter, InterpreterAction, InterpreterResult,
    },
    Database, Inspector,
};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000e00000");
const A: Address = address!("0000000000000000000000000000000000e00001");
const B: Address = address!("0000000000000000000000000000000000e00002");
const GAS_LIMIT: u64 = 1_000_000;

/// `A` calls `B` and stops; `B` pushes and pops a word and stops.
fn a_calls_b() -> MemoryDatabase {
    let a = BytecodeBuilder::default().call(B, U256::ZERO).append(POP).stop().build();
    let b = BytecodeBuilder::default().append_many([PUSH0, POP]).stop().build();
    MemoryDatabase::default().account_code(A, a).account_code(B, b)
}

/// Runs a call from `CALLER` to `A` under `limits`, under `inspector` when there is one, and
/// hands the inspector back as the run left it.
fn run<I: Inspector<MegaContext<MemoryDatabase>, EthInterpreter> + Clone>(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    gas_limit: u64,
    inspector: Option<I>,
) -> (MegaTransactionOutcome, Option<I>) {
    let tx = call(CALLER, A, U256::ZERO, gas_limit);
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
    let Some(inspector) = inspector else {
        return (evm.execute_transaction(tx).expect("a valid transaction"), None);
    };
    let mut evm = evm.with_inspector(inspector);
    let outcome = evm.execute_transaction(tx).expect("a valid transaction");
    (outcome, Some(evm.inspector().clone()))
}

/// Where [`Charges`] charges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    /// In the `step_end` of every instruction that ended its frame.
    FramesEnd,
    /// In the `step_end` of every other instruction that left no action pending.
    Elsewhere,
    /// On the result of every call, in `call_end`.
    CallResult,
}

/// Charges `amount` of regular gas at every place `at` names, and counts the charges it made.
#[derive(Clone)]
struct Charges {
    at: At,
    amount: u64,
    made: u64,
}

impl Charges {
    const fn new(at: At, amount: u64) -> Self {
        Self { at, amount, made: 0 }
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Charges {
    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        let at = match interp.bytecode.action() {
            Some(InterpreterAction::Return(_)) => At::FramesEnd,
            None => At::Elsewhere,
            Some(_) => return,
        };
        if at == self.at {
            assert!(interp.gas.record_regular_cost(self.amount), "the frame can pay");
            self.made += 1;
        }
    }

    fn call_end(&mut self, _: &mut MegaContext<DB>, _: &CallInputs, outcome: &mut CallOutcome) {
        if self.at == At::CallResult {
            assert!(outcome.result.gas.record_regular_cost(self.amount), "the result can pay");
            self.made += 1;
        }
    }
}

/// Gas an inspector charges in the `step_end` of the instruction that ended a frame — `B`'s
/// `STOP`, and `A`'s — is taken from an interpreter whose gas the instruction already copied into
/// the frame's result: neither `A` nor the transaction sees it. The same charge after any other
/// instruction lands on the receipt, once per instruction; and charged on a frame's result, in
/// `call_end`, it reaches whoever settles that result — `B`'s reaches `A`, `A`'s the transaction.
#[test]
fn test_gas_changed_at_a_frames_end_has_no_effect() {
    const AMOUNT: u64 = 1_000;
    let limits = EvmTxRuntimeLimits::no_limits();
    let (plain, _) = run::<Charges>(a_calls_b(), limits, GAS_LIMIT, None);
    assert!(plain.result.is_success(), "{:?}", plain.result);

    let (at_end, charges) =
        run(a_calls_b(), limits, GAS_LIMIT, Some(Charges::new(At::FramesEnd, AMOUNT)));
    assert_eq!(charges.unwrap().made, 2, "B's STOP and A's");
    assert_eq!(at_end.result, plain.result, "no charge reached the receipt");
    assert_eq!(at_end.gas, plain.gas);

    let (elsewhere, charges) =
        run(a_calls_b(), limits, GAS_LIMIT, Some(Charges::new(At::Elsewhere, AMOUNT)));
    let made = charges.unwrap().made;
    assert!(made > 2, "every other instruction but the CALL: {made}");
    assert_eq!(elsewhere.gas.gas_used, plain.gas.gas_used + made * AMOUNT, "each one lands");

    let (on_result, charges) =
        run(a_calls_b(), limits, GAS_LIMIT, Some(Charges::new(At::CallResult, AMOUNT)));
    assert_eq!(charges.unwrap().made, 2, "B's result and A's");
    assert_eq!(
        on_result.gas.gas_used,
        plain.gas.gas_used + 2 * AMOUNT,
        "B's reaches A, and A's the transaction"
    );
}

/// The callbacks holding the interpreter that [`Overdraws`] charges from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Callback {
    Step,
    StepEnd,
    LogFull,
}

/// Charges, once, one more gas than the running frame's spendable gas from the callback `from`,
/// the first time it runs while part of the frame's gas is withheld — or at all, when
/// `detained_only` is off — and records whether the charge was refused.
#[derive(Clone)]
struct Overdraws {
    from: Callback,
    detained_only: bool,
    refused: Option<bool>,
}

impl Overdraws {
    const fn new(from: Callback, detained_only: bool) -> Self {
        Self { from, detained_only, refused: None }
    }

    fn charge(&mut self, from: Callback, interp: &mut Interpreter<EthInterpreter>) {
        let gas = interp.gas.tracker();
        if from != self.from ||
            self.refused.is_some() ||
            (self.detained_only && gas.withheld() == 0)
        {
            return;
        }
        let over = gas.spendable() + 1;
        self.refused = Some(!interp.gas.record_regular_cost(over));
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Overdraws {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.charge(Callback::Step, interp);
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.charge(Callback::StepEnd, interp);
    }

    fn log_full(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _: &mut MegaContext<DB>,
        _: Log,
    ) {
        self.charge(Callback::LogFull, interp);
    }
}

/// A charge the frame cannot pay is refused: the interpreter's gas is left as it was, the frame is
/// not halted and runs to its end, and the transaction is the one no inspector runs.
///
/// Under gas detention the same holds for a charge only the withheld part could pay, from each
/// callback that holds the interpreter. An opcode's charge there is the crossing, which stops the
/// transaction when its frame ends; the inspector's is none of the frame's, so it classifies
/// nothing. `A` reads the block's timestamp, which caps its compute a thousand gas past the read,
/// then logs and runs a few more instructions.
#[test]
fn test_an_inspector_cannot_overdraw_a_frame() {
    let limits = EvmTxRuntimeLimits::no_limits();
    let (plain, _) = run::<Overdraws>(a_calls_b(), limits, GAS_LIMIT, None);
    let overdraws = Overdraws::new(Callback::StepEnd, false);
    let (overdrawn, inspector) = run(a_calls_b(), limits, GAS_LIMIT, Some(overdraws));
    assert_eq!(inspector.unwrap().refused, Some(true), "the charge was refused");
    assert_eq!(overdrawn.result, plain.result, "nothing was taken, and nothing halted");
    assert_eq!(overdrawn.gas, plain.gas);
    assert_eq!(overdrawn.state, plain.state);

    let reads = BytecodeBuilder::default()
        .append(TIMESTAMP)
        .append(POP)
        .append_many([PUSH0, PUSH0, LOG0])
        .append_many([PUSH0, POP, PUSH0, POP, PUSH0, POP])
        .stop()
        .build();
    let db = || MemoryDatabase::default().account_code(A, reads.clone());
    let detained = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(1_000);
    let (plain, _) = run::<Overdraws>(db(), detained, GAS_LIMIT, None);
    assert!(plain.result.is_success(), "{:?}", plain.result);
    for from in [Callback::Step, Callback::StepEnd, Callback::LogFull] {
        let (overdrawn, inspector) =
            run(db(), detained, GAS_LIMIT, Some(Overdraws::new(from, true)));
        assert_eq!(inspector.unwrap().refused, Some(true), "{from:?}: the withheld part");
        assert_eq!(overdrawn.limit_exceeded, None, "{from:?}: no crossing");
        assert_eq!(overdrawn.result, plain.result, "{from:?}");
        assert_eq!(overdrawn.gas, plain.gas, "{from:?}: everything the frame ran is billed");
    }
}

/// How [`AnswersB`] builds the gas of its answer.
#[derive(Clone, Copy, Debug)]
enum AnswerGas {
    /// The call's untouched gas, which carries the reservoir the frame inherited.
    Untouched,
    /// `Gas::new` of the call's gas limit, as Foundry answers a cheatcode: no reservoir.
    Fresh,
    /// Fresh regular gas carrying the pools of the untouched gas after a state and a history
    /// charge ([`with_pools_of`]): charges the frame, which never ran, did not make.
    Charged,
}

/// Answers every call to `B` with `result`, on gas built as `gas` says.
#[derive(Clone)]
struct AnswersB {
    result: InstructionResult,
    gas: AnswerGas,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for AnswersB {
    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (inputs.target_address == B).then(|| {
            let gas = match self.gas {
                AnswerGas::Untouched => untouched_call_gas(inputs),
                AnswerGas::Fresh => Gas::new(inputs.gas_limit),
                AnswerGas::Charged => {
                    let mut charged = untouched_call_gas(inputs);
                    assert!(
                        charged.record_state_cost(40_000) && charged.record_history_cost(3_000)
                    );
                    with_pools_of(inputs.gas_limit, &charged)
                }
            };
            CallOutcome::new(
                InterpreterResult::new(self.result, Bytes::new(), gas),
                inputs.return_memory_offset.clone(),
            )
        })
    }
}

/// An answered frame never ran, so its gas carries nothing but the regular gas its inspector
/// chose: its caller merges back the reservoir it forwarded, whatever `Gas` the answer was built
/// on. Above the execution cap, an answer on the frame's untouched gas, one on `Gas::new` — which
/// carries no reservoir — and one carrying charges the frame never made settle alike, a success, a
/// revert and a halt each, and the transaction keeps the reservoir it keeps when `B` runs. The
/// charges are rolled back against the answer's own reservoir before the inherited one replaces
/// it, so neither the reservoir nor a ledger counts them.
#[test]
fn test_an_answer_carries_the_reservoir_its_caller_merges() {
    let gas_limit = TX_GAS_LIMIT_CAP + 100_000_000;
    let limits = EvmTxRuntimeLimits::no_limits();
    let (plain, _) = run::<AnswersB>(a_calls_b(), limits, gas_limit, None);
    assert!(plain.gas.reservoir_remaining > 0, "the reservoir comes back");

    for result in [InstructionResult::Stop, InstructionResult::Revert, InstructionResult::OutOfGas]
    {
        let answer = |gas| {
            let inspector = AnswersB { result, gas };
            run(a_calls_b(), limits, gas_limit, Some(inspector)).0
        };
        let untouched = answer(AnswerGas::Untouched);
        assert_eq!(untouched.gas.reservoir_remaining, plain.gas.reservoir_remaining, "{result:?}");
        for gas in [AnswerGas::Fresh, AnswerGas::Charged] {
            let answered = answer(gas);
            assert!(answered.result.is_success(), "{result:?}, {gas:?}: `A` ignores the answer");
            assert_eq!(answered.gas, untouched.gas, "{result:?}, {gas:?}: every ledger");
            assert_eq!(answered.usage, untouched.usage, "{result:?}, {gas:?}: every count");
        }
    }
}
