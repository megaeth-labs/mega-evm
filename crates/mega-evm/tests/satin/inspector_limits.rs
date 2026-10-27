//! What an inspector's writes to gas cannot do.
//!
//! - Gas changed at a frame's end has no effect: the instruction that ends a frame copies the
//!   frame's gas into the result it returns, so what `step_end` writes to the interpreter's gas
//!   after it reaches nobody. What a frame returns is changed on its result, in `call_end`.
//! - An inspector cannot overdraw a frame: a charge the frame's spendable gas cannot pay is refused
//!   and takes nothing, and the frame runs on. Under gas detention the refusal of an opcode's
//!   charge is the crossing; an inspector's leaves the same record on the frame's gas, as revm
//!   leaves it, which stops nothing in a frame that ends without an out-of-gas, and which a later
//!   halt that records nothing of its own is read off.
//! - An answer settles on the gas its inspector built it on, as revm settles it: its caller takes
//!   the answer's reservoir as its own, and the charges the answer carries as a success's.

use alloy_primitives::{address, Address, Bytes, Log, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    untouched_call_gas, EvmTxRuntimeLimits, LimitCheck, MegaContext, MegaEvm,
    MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{BALANCE, CALL, JUMP, JUMPDEST, LOG0, POP, PUSH0, PUSH20, TIMESTAMP},
    interpreter::{
        instructions::gas_table_spec, interpreter::EthInterpreter, interpreter_types::LoopControl,
        CallInputs, CallOutcome, Gas, InstructionResult, Interpreter, InterpreterAction,
        InterpreterResult, PoolId,
    },
    primitives::hardfork::SpecId,
    Database, Inspector,
};

use crate::common::{
    assert_answered_at_unlimited_budgets, body_history, call, context, reservoir_share,
};
use mega_evm::test_utils::{record_start_ledger_settlements, take_start_ledger_settlements};

const CALLER: Address = address!("0000000000000000000000000000000000e00000");
const A: Address = address!("0000000000000000000000000000000000e00001");
const B: Address = address!("0000000000000000000000000000000000e00002");
/// An account no transaction here touches before `B` reads its balance.
const COLD: Address = address!("0000000000000000000000000000000000e00003");
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
    InitializeInterp,
    Step,
    StepEnd,
    LogFull,
}

/// The frames [`Overdraws`] charges in.
#[derive(Clone, Copy, Debug)]
enum Frames {
    /// Any frame.
    Any,
    /// A frame part of whose gas is withheld.
    Detained,
    /// A frame running `B`'s code.
    B,
}

/// Charges, once, one more gas than the running frame's spendable gas from the callback `from`,
/// the first time it runs in one of `frames`, and records whether the charge was refused.
#[derive(Clone)]
struct Overdraws {
    from: Callback,
    frames: Frames,
    refused: Option<bool>,
}

impl Overdraws {
    const fn new(from: Callback, frames: Frames) -> Self {
        Self { from, frames, refused: None }
    }

    fn charge(&mut self, from: Callback, interp: &mut Interpreter<EthInterpreter>) {
        let gas = interp.gas.tracker();
        let in_frames = match self.frames {
            Frames::Any => true,
            Frames::Detained => gas.withheld() > 0,
            Frames::B => interp.input.target_address == B,
        };
        if from != self.from || self.refused.is_some() || !in_frames {
            return;
        }
        let over = gas.spendable() + 1;
        self.refused = Some(!interp.gas.record_regular_cost(over));
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Overdraws {
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _: &mut MegaContext<DB>,
    ) {
        self.charge(Callback::InitializeInterp, interp);
    }

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
    let overdraws = Overdraws::new(Callback::StepEnd, Frames::Any);
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
            run(db(), detained, GAS_LIMIT, Some(Overdraws::new(from, Frames::Detained)));
        assert_eq!(inspector.unwrap().refused, Some(true), "{from:?}: the withheld part");
        assert_eq!(overdrawn.limit_exceeded, None, "{from:?}: no crossing");
        assert_eq!(overdrawn.result, plain.result, "{from:?}");
        assert_eq!(overdrawn.gas, plain.gas, "{from:?}: everything the frame ran is billed");
    }
}

/// A charge an inspector's callback could not pay is refused and leaves its record of what the
/// frame had, as revm leaves it: revm does not guard what a callback writes to the interpreter's
/// gas, and neither does the engine. `A` reads the block's timestamp, calls `B` with 2,000 gas,
/// then loops until the compute limit stops it. `B`'s callback charge is refused; `B` then pushes
/// and pops a hundred words and halts on a cold `BALANCE` it cannot pay, which the interpreter
/// refuses before any charge, so no failed deduction of its own records what `B` had. What `B`
/// burns is read off the refused charge's record: what `B` had at the callback, not at its halt.
/// The gas `B` ran in between counts as burned rather than as compute, so `A` runs that much longer
/// to the limit, and is billed that much more, than in the transaction no inspector runs.
#[test]
fn test_a_refused_charge_is_read_as_what_a_later_halt_burns() {
    let b = BytecodeBuilder::default()
        .append_many([PUSH0, POP].repeat(100))
        .push_address(COLD)
        .append(BALANCE)
        .stop()
        .build();
    let a = BytecodeBuilder::default()
        .append_many([TIMESTAMP, POP, PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .push_number(2_000_u16)
        .append_many([CALL, POP]);
    let lap = a.len() as u8;
    let a = a.append(JUMPDEST).push_number(lap).append(JUMP).build();
    let db = || MemoryDatabase::default().account_code(A, a.clone()).account_code(B, b.clone());
    let detained = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(20_000);
    let (plain, _) = run::<Overdraws>(db(), detained, GAS_LIMIT, None);
    assert!(plain.limit_exceeded.is_some(), "the loop runs to the limit: {:?}", plain.result);
    // What `B` runs before its halt: the pushes and pops, the address and `BALANCE`'s static gas.
    let static_gas = gas_table_spec(SpecId::AMSTERDAM);
    let to_the_halt = 100 * u64::from(static_gas[PUSH0 as usize] + static_gas[POP as usize]) +
        u64::from(static_gas[PUSH20 as usize] + static_gas[BALANCE as usize]);
    // What `B` ran before the callback: nothing before its first step, its first `PUSH0` before
    // the step's end.
    for (from, before) in [
        (Callback::InitializeInterp, 0),
        (Callback::Step, 0),
        (Callback::StepEnd, u64::from(static_gas[PUSH0 as usize])),
    ] {
        let (overdrawn, inspector) =
            run(db(), detained, GAS_LIMIT, Some(Overdraws::new(from, Frames::B)));
        assert_eq!(inspector.unwrap().refused, Some(true), "{from:?}: the charge was refused");
        let stop = |outcome: &MegaTransactionOutcome| match outcome.limit_exceeded {
            Some(LimitCheck::ExceedsLimit { kind, limit, .. }) => Some((kind, limit)),
            _ => None,
        };
        assert_eq!(stop(&overdrawn), stop(&plain), "{from:?}: the same limit stops it");
        // The stop lands on the first of `A`'s loop charges the limit leaves no room for, so the
        // bill grows by the extra room to within the loop's dearest charge, its `JUMP`.
        let grown = overdrawn.gas.regular - plain.gas.regular;
        assert!(
            grown.abs_diff(to_the_halt - before) < u64::from(static_gas[JUMP as usize]),
            "{from:?}: what B ran after the callback is burned, and A runs that much longer: \
             {grown} for {}",
            to_the_halt - before
        );
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
    /// charge: charges the frame, which never ran, did not make.
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
                    let mut gas = Gas::new(inputs.gas_limit);
                    gas.set_reservoir(charged.reservoir());
                    gas.set_state_gas_spent(charged.state_gas_spent());
                    gas.set_state_gas_spilled(charged.state_gas_spilled());
                    gas.set_history_gas_spent(charged.history_gas_spent());
                    for pool in PoolId::ALL {
                        gas.set_pool_remaining(pool, charged.pool_remaining(pool));
                        gas.set_pool_spent(pool, charged.pool_spent(pool));
                    }
                    gas
                }
            };
            CallOutcome::new(
                InterpreterResult::new(self.result, Bytes::new(), gas),
                inputs.return_memory_offset.clone(),
            )
        })
    }
}

/// An answered frame never ran, and its caller settles the answer on the gas its inspector built
/// it on, as revm does. Above the execution cap, an answer on the frame's untouched gas carries the
/// reservoir share it inherited, and the transaction keeps the reservoir it keeps when `B` runs;
/// one on `Gas::new`, which carries none, leaves its caller only the part of its reservoir it kept
/// from the share, a success, a revert and a halt alike.
/// One carrying charges the frame never made merges them as a success's — the state and history
/// gas on their ledgers, drawn from the reservoir — and a revert or a halt rolls them back.
#[test]
fn test_an_answer_settles_on_the_gas_it_is_built_on() {
    let gas_limit = TX_GAS_LIMIT_CAP + 100_000_000;
    let limits = EvmTxRuntimeLimits::no_limits();
    let (plain, _) = run::<AnswersB>(a_calls_b(), limits, gas_limit, None);
    // Nothing draws the reservoir: `A` holds all of it when it calls `B`, which takes its share.
    let whole = gas_limit - TX_GAS_LIMIT_CAP - body_history(0);
    assert_eq!(plain.gas.reservoir_remaining, whole, "the reservoir comes back");
    let kept = whole - reservoir_share(whole);
    assert_ne!(kept, 0);

    for result in [InstructionResult::Stop, InstructionResult::Revert, InstructionResult::OutOfGas]
    {
        let answer = |gas| {
            let inspector = AnswersB { result, gas };
            let tx = call(CALLER, A, U256::ZERO, gas_limit);
            let mut evm = MegaEvm::new(context(a_calls_b()).with_tx_runtime_limits(limits))
                .with_inspector(inspector);
            record_start_ledger_settlements(evm.ctx_mut());
            let outcome = evm.execute_transaction(tx).expect("a valid transaction");
            // The start ledger settles an inspector's answer at unlimited budgets whatever pools
            // its gas carries — none on `Gas::new`, the call's shares set on it — and leaves `A`
            // with unlimited budgets.
            let settlements = take_start_ledger_settlements(evm.ctx_mut());
            assert_answered_at_unlimited_budgets(&settlements, 2, 1);
            outcome
        };
        let untouched = answer(AnswerGas::Untouched);
        assert!(untouched.result.is_success(), "{result:?}: `A` ignores the answer");
        assert_eq!(untouched.gas.reservoir_remaining, plain.gas.reservoir_remaining, "{result:?}");

        let fresh = answer(AnswerGas::Fresh);
        assert!(fresh.result.is_success(), "{result:?}, fresh: `A` ignores the answer");
        assert_eq!(fresh.gas.reservoir_remaining, kept, "{result:?}: `Gas::new` carries none");
        assert_eq!(fresh.usage, untouched.usage, "{result:?}, fresh: every count");

        let charged = answer(AnswerGas::Charged);
        assert!(charged.result.is_success(), "{result:?}, charged: `A` ignores the answer");
        assert_eq!(charged.usage, untouched.usage, "{result:?}, charged: every count");
        if result.is_ok() {
            assert_eq!(charged.gas.state, untouched.gas.state + 40_000, "{result:?}: merged");
            assert_eq!(charged.gas.history, untouched.gas.history + 3_000, "{result:?}: merged");
            assert_eq!(
                charged.gas.reservoir_remaining,
                untouched.gas.reservoir_remaining - 43_000,
                "{result:?}: drawn from the reservoir"
            );
        } else {
            assert_eq!(charged.gas, untouched.gas, "{result:?}: rolled back, on every ledger");
        }
    }
}
