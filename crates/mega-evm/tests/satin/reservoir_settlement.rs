//! What a frame's caller holds of the EIP-8037 reservoir once its frame settled into it: the part
//! of its reservoir it kept while the frame ran, and what the frame hands back on top, once that
//! has unwound the caller's outstanding spill, last in first out. The transaction's frame is given
//! the whole reservoir, of which the transaction's gas keeps a copy until the frame settles, and
//! the transaction's gas then holds what its frame hands back alone.
//!
//! An inspector reads the transaction's frame's gas at the end of each `CALL` it makes, once the
//! start took the child's share out of it; the child's gas when the child ends, before revm settles
//! it; and the frame's gas again at the instruction after the `CALL`, once the child settled into
//! it. A child that failed hands back the share it was given, its draws undone; one that succeeded
//! hands back what it left, and its spill joins the caller's.

use alloy_primitives::{address, Address, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{
        record_start_ledger_settlements, take_start_ledger_settlements, BytecodeBuilder,
        MemoryDatabase,
    },
    MegaContext, MegaEvm,
};
use revm::{
    bytecode::opcode::{CALL, INVALID, POP},
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        InstructionResult, Interpreter,
    },
    Database, Inspector,
};

use crate::common::{body_history, call, context, history_is_free, slot_state_gas, state_is_free};

const SENDER: Address = address!("0000000000000000000000000000000000740000");
const A: Address = address!("0000000000000000000000000000000000740001");
/// Writes a new slot and stops.
const OK: Address = address!("0000000000000000000000000000000000740002");
/// Writes a new slot and reverts.
const REVERTER: Address = address!("0000000000000000000000000000000000740003");
/// Writes a new slot and halts.
const HALTER: Address = address!("0000000000000000000000000000000000740004");
/// Writes [`SPENT_SLOTS`] new slots, past any share it is given here, and stops.
const SPENDER: Address = address!("0000000000000000000000000000000000740005");
/// Stops.
const IDLE: Address = address!("0000000000000000000000000000000000740006");

/// The new slots [`SPENDER`] writes.
const SPENT_SLOTS: u64 = 4;

/// What funds a frame's state and history charges beside its regular gas: its reservoir, and the
/// spill those charges made onto its regular gas that is still outstanding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Funding {
    reservoir: u64,
    spilled: u64,
}

impl Funding {
    /// No reservoir and no spill.
    const NONE: Self = Self { reservoir: 0, spilled: 0 };
}

/// One child of the transaction's frame, from its start to its settlement.
#[derive(Clone, Copy, Debug, Default)]
struct Child {
    /// The caller's funding once the start took the child's share: what it kept.
    kept: Option<Funding>,
    /// The reservoir the child was given.
    share: u64,
    /// What the child hands back: its reservoir as revm settles it, and its spill.
    handed_back: Option<Funding>,
    /// The caller's funding once the child settled into it.
    after: Option<Funding>,
}

/// Reads [`Child`] for every child of the transaction's frame, and that frame's own reservoir as
/// it started and its funding as it ended.
#[derive(Clone, Default)]
struct Settlements {
    opcode: Option<u8>,
    depth: usize,
    children: Vec<Child>,
    /// The child that settled last, whose caller's funding the next instruction reads.
    settling: Option<usize>,
    frame_given: Option<u64>,
    frame_ended: Option<(InstructionResult, Funding)>,
}

const fn funding(gas: &revm::interpreter::Gas) -> Funding {
    Funding { reservoir: gas.reservoir(), spilled: gas.state_gas_spilled() }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Settlements {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        if self.depth == 1 {
            if let Some(index) = self.settling.take() {
                self.children[index].after = Some(funding(&interp.gas));
            }
            self.opcode = Some(interp.bytecode.opcode());
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        if self.depth == 1 && self.opcode.take() == Some(CALL) {
            self.children.push(Child { kept: Some(funding(&interp.gas)), ..Child::default() });
        }
    }

    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.depth += 1;
        match self.depth {
            1 => self.frame_given = Some(inputs.reservoir),
            2 => {
                self.children.last_mut().expect("the CALL's end came first").share =
                    inputs.reservoir
            }
            _ => {}
        }
        None
    }

    fn call_end(&mut self, _: &mut MegaContext<DB>, _: &CallInputs, outcome: &mut CallOutcome) {
        let result = outcome.result.result;
        let gas = &outcome.result.gas;
        match self.depth {
            1 => self.frame_ended = Some((result, funding(gas))),
            2 => {
                let index = self.children.len() - 1;
                let child = &mut self.children[index];
                child.handed_back = Some(if result.is_ok() {
                    funding(gas)
                } else {
                    Funding { reservoir: child.share, spilled: 0 }
                });
                self.settling = Some(index);
            }
            _ => {}
        }
        self.depth -= 1;
    }
}

/// `A` calls each of `children` in turn, dropping the success flags, then ends with `end`.
fn calling(children: &[Address], end: u8) -> MemoryDatabase {
    let mut code = BytecodeBuilder::default();
    for child in children {
        code = code.call(*child, U256::ZERO).append(POP);
    }
    let writes = |slots: u64| {
        (1..=slots)
            .fold(BytecodeBuilder::default(), |code, slot| code.sstore(U256::from(slot), U256::ONE))
    };
    MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10_u64.pow(18)))
        .account_code(A, code.append(end).build())
        .account_code(OK, writes(1).stop().build())
        .account_code(REVERTER, writes(1).revert().build())
        .account_code(HALTER, writes(1).append(INVALID).build())
        .account_code(SPENDER, writes(SPENT_SLOTS).stop().build())
        .account_code(IDLE, BytecodeBuilder::default().stop().build())
}

/// A call to `A`, as it ended.
struct Run {
    gas_limit: u64,
    gas_used: u64,
    reservoir_remaining: u64,
    /// The history gas the start ledger gave back to the transaction's gas at its start's
    /// settlement: the start charge, or nothing.
    start_refund: u64,
    read: Settlements,
}

/// The gas limit that leaves `reservoir` once the body is paid.
fn leaving(reservoir: u64) -> u64 {
    TX_GAS_LIMIT_CAP + body_history(0) + reservoir
}

/// Runs a call to `A` carrying `value` over `db` at `gas_limit`.
fn run(db: MemoryDatabase, gas_limit: u64, value: u64) -> Run {
    let mut evm = MegaEvm::new(context(db)).with_inspector(Settlements::default());
    record_start_ledger_settlements(evm.ctx_mut());
    let outcome = evm
        .execute_transaction(call(SENDER, A, U256::from(value), gas_limit))
        .expect("the call is valid");
    let settlements = take_start_ledger_settlements(evm.ctx_mut());
    let start = settlements.iter().find(|settled| settled.depth == 0).expect("the start settled");
    Run {
        gas_limit,
        gas_used: outcome.result.tx_gas_used(),
        reservoir_remaining: outcome.gas.reservoir_remaining,
        start_refund: start.refund,
        read: evm.inspector().clone(),
    }
}

/// What a caller that kept `kept` holds once a child that hands back `handed_back` settled into
/// it: the reservoir the child hands back unwinds the caller's spill, the child's own merged in,
/// as far as it goes, and the rest joins the part the caller kept.
const fn after_settlement(kept: Funding, handed_back: Funding) -> Funding {
    let spilled = kept.spilled + handed_back.spilled;
    let unwound = if handed_back.reservoir < spilled { handed_back.reservoir } else { spilled };
    Funding {
        reservoir: kept.reservoir + handed_back.reservoir - unwound,
        spilled: spilled - unwound,
    }
}

/// Each child of the transaction's frame settles into what the frame kept of its reservoir: one
/// that succeeded after a write hands back what it left of its share, one that reverted or halted
/// after a write its whole share, and one that spent past its share hands back nothing and its
/// spill; the next child's share then unwinds that spill as it comes back. The frame kept two per
/// cent of its reservoir at every start, and the transaction's gas, which holds none of it, ends
/// with what the frame hands back.
#[test]
fn test_a_callers_reservoir_after_a_child_is_what_it_kept_with_what_the_child_hands_back() {
    if state_is_free() {
        return;
    }
    let children = [OK, REVERTER, HALTER, SPENDER, IDLE];
    let Run { reservoir_remaining, read, .. } =
        run(calling(&children, revm::bytecode::opcode::STOP), leaving(4 * slot_state_gas()), 0);

    assert_eq!(read.children.len(), children.len());
    for (child, settled) in children.iter().zip(&read.children) {
        let kept = settled.kept.expect("the CALL ended");
        let handed_back = settled.handed_back.expect("the child ended");
        assert_eq!(
            settled.after,
            Some(after_settlement(kept, handed_back)),
            "{child}: kept {kept:?}, handed back {handed_back:?}"
        );
    }
    let [ok, _, _, spender, idle] = read.children[..] else { unreachable!() };
    let kept = ok.kept.expect("the CALL ended").reservoir;
    assert!(kept > 0, "the frame keeps part of its reservoir: {kept}");
    assert_eq!(kept, (kept + ok.share) * 2 / 100, "two per cent of it");
    assert!(spender.handed_back.expect("ended").spilled > 0, "the spender spills");
    let idle_kept = idle.kept.expect("the CALL ended");
    assert!(idle_kept.spilled > 0 && idle.share > 0, "the idle child's share unwinds the spill");
    assert_eq!(idle.after.expect("settled").spilled, idle_kept.spilled - idle.share);

    let (result, ended) = read.frame_ended.expect("the frame ended");
    assert!(result.is_ok(), "{result:?}");
    let to_the_transaction = after_settlement(Funding::NONE, ended);
    assert_eq!(reservoir_remaining, to_the_transaction.reservoir);
}

/// The transaction's frame hands back what a child's would, into gas whose copy of the reservoir it
/// gave the frame the settlement replaces, nothing added back: what the frame left after a child
/// that kept a part of it and a write of its own when it succeeds, the whole reservoir it was
/// given when it reverts or halts. A halt of a frame whose value transfer was charged a start
/// charge gives the charge back first and then burns the transaction's regular gas: above the cap
/// the charge comes back into the reservoir; below it, where the charge spilled onto the
/// transaction's regular gas, it comes back there and is burned with the rest.
#[test]
fn test_the_transactions_gas_holds_what_its_frame_hands_back_and_nothing_more() {
    if state_is_free() {
        return;
    }
    let above = leaving(4 * slot_state_gas());
    for (end, ok, value, gas_limit) in [
        (revm::bytecode::opcode::STOP, true, 0, above),
        (revm::bytecode::opcode::REVERT, false, 0, above),
        (INVALID, false, 0, above),
        (INVALID, false, 1, above),
        (INVALID, false, 1, 30_000_000),
    ] {
        let code = BytecodeBuilder::default()
            .call(OK, U256::ZERO)
            .append(POP)
            .sstore(U256::from(7), U256::ONE);
        let code =
            if end == revm::bytecode::opcode::REVERT { code.revert() } else { code.append(end) };
        let db = calling(&[], end).account_code(A, code.build());
        let Run { gas_limit, gas_used, reservoir_remaining, start_refund, read } =
            run(db, gas_limit, value);
        let case = format!("{end:#04x}, value {value}, gas limit {gas_limit}");

        let given = read.frame_given.expect("the frame started");
        let (result, ended) = read.frame_ended.expect("the frame ended");
        assert_eq!(result.is_ok(), ok, "{case}: {result:?}");
        assert_eq!(start_refund != 0, value != 0 && !ok && !history_is_free(), "{case}");
        let expected = if ok {
            after_settlement(Funding::NONE, ended).reservoir
        } else if gas_limit > TX_GAS_LIMIT_CAP {
            given + start_refund
        } else {
            0
        };
        assert_eq!(reservoir_remaining, expected, "{case}");
        if end == INVALID {
            assert_eq!(
                gas_used + reservoir_remaining,
                gas_limit,
                "{case}: the halt burns the regular gas"
            );
        }
    }
}
