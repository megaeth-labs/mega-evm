//! What a frame's caller holds of the EIP-8037 reservoir once its frame settled into it: the part
//! of its reservoir it kept while the frame ran, and what the frame hands back on top, once that
//! has unwound the caller's outstanding spill, last in first out. The transaction's gas, whose
//! frame takes the whole reservoir, holds what its frame hands back alone.
//!
//! An inspector reads the transaction's frame's gas at the end of each `CALL` it makes, once the
//! start took the child's share out of it; the child's gas when the child ends, before revm settles
//! it; and the frame's gas again at the instruction after the `CALL`, once the child settled into
//! it. A child that failed hands back the share it was given, its draws undone; one that succeeded
//! hands back what it left, and its spill joins the caller's.

use alloy_primitives::{address, Address, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
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

use crate::common::{body_history, call, context, slot_state_gas, state_is_free};

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

/// A frame's reservoir and outstanding spill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pools {
    reservoir: u64,
    spilled: u64,
}

/// One child of the transaction's frame, from its start to its settlement.
#[derive(Clone, Copy, Debug, Default)]
struct Settled {
    /// The caller's pools once the start took the child's share: what it kept.
    kept: Option<Pools>,
    /// The reservoir the child was given.
    share: u64,
    /// What the child hands back: its reservoir as revm settles it, and its spill.
    handed_back: Option<Pools>,
    /// The caller's pools once the child settled into it.
    after: Option<Pools>,
}

/// Reads [`Settled`] for every child of the transaction's frame, and that frame's own reservoir as
/// it started and its pools as it ended.
#[derive(Clone, Default)]
struct Settlements {
    opcode: Option<u8>,
    depth: usize,
    children: Vec<Settled>,
    /// The child that settled last, whose caller's pools the next instruction reads.
    settling: Option<usize>,
    frame_given: Option<u64>,
    frame_ended: Option<(InstructionResult, Pools)>,
}

const fn pools(gas: &revm::interpreter::Gas) -> Pools {
    Pools { reservoir: gas.reservoir(), spilled: gas.state_gas_spilled() }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Settlements {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        if self.depth == 1 {
            if let Some(index) = self.settling.take() {
                self.children[index].after = Some(pools(&interp.gas));
            }
            self.opcode = Some(interp.bytecode.opcode());
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        if self.depth == 1 && self.opcode.take() == Some(CALL) {
            self.children.push(Settled { kept: Some(pools(&interp.gas)), ..Settled::default() });
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
            1 => self.frame_ended = Some((result, pools(gas))),
            2 => {
                let index = self.children.len() - 1;
                let child = &mut self.children[index];
                child.handed_back = Some(if result.is_ok() {
                    pools(gas)
                } else {
                    Pools { reservoir: child.share, spilled: 0 }
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

/// Runs a call to `A` over `db` at a gas limit that leaves `reservoir` once the body is paid, and
/// returns the outcome's reservoir and gas used with what [`Settlements`] read.
fn run(db: MemoryDatabase, reservoir: u64) -> (u64, u64, u64, Settlements) {
    let gas_limit = TX_GAS_LIMIT_CAP + body_history(0) + reservoir;
    let mut evm = MegaEvm::new(context(db)).with_inspector(Settlements::default());
    let outcome =
        evm.execute_transaction(call(SENDER, A, U256::ZERO, gas_limit)).expect("the call is valid");
    let gas_used = outcome.result.tx_gas_used();
    let reservoir = outcome.gas.reservoir_remaining;
    (gas_limit, gas_used, reservoir, evm.inspector().clone())
}

/// What a caller that kept `kept` holds once a child that hands back `handed_back` settled into
/// it: the reservoir the child hands back unwinds the caller's spill, the child's own merged in,
/// as far as it goes, and the rest joins the part the caller kept.
const fn settled(kept: Pools, handed_back: Pools) -> Pools {
    let spilled = kept.spilled + handed_back.spilled;
    let unwound = if handed_back.reservoir < spilled { handed_back.reservoir } else { spilled };
    Pools {
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
    let (_, _, reservoir_remaining, read) =
        run(calling(&children, revm::bytecode::opcode::STOP), 4 * slot_state_gas());

    assert_eq!(read.children.len(), children.len());
    for (child, settled_child) in children.iter().zip(&read.children) {
        let kept = settled_child.kept.expect("the CALL ended");
        let handed_back = settled_child.handed_back.expect("the child ended");
        assert_eq!(
            settled_child.after,
            Some(settled(kept, handed_back)),
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
    let to_the_transaction = settled(Pools { reservoir: 0, spilled: 0 }, ended);
    assert_eq!(reservoir_remaining, to_the_transaction.reservoir);
}

/// The transaction's frame hands back what a child's would, into gas that holds nothing of the
/// reservoir it gave the frame: what the frame left after a child that kept a part of it and a
/// write of its own when it succeeds, the whole reservoir it was given when it reverts or halts.
/// A halt also burns the transaction's regular gas.
#[test]
fn test_the_transactions_gas_holds_what_its_frame_hands_back_and_nothing_more() {
    if state_is_free() {
        return;
    }
    for (end, ok) in [
        (revm::bytecode::opcode::STOP, true),
        (revm::bytecode::opcode::REVERT, false),
        (INVALID, false),
    ] {
        let code = BytecodeBuilder::default()
            .call(OK, U256::ZERO)
            .append(POP)
            .sstore(U256::from(7), U256::ONE);
        let code =
            if end == revm::bytecode::opcode::REVERT { code.revert() } else { code.append(end) };
        let db = calling(&[], end).account_code(A, code.build());
        let (gas_limit, gas_used, reservoir_remaining, read) = run(db, 4 * slot_state_gas());

        let given = read.frame_given.expect("the frame started");
        let (result, ended) = read.frame_ended.expect("the frame ended");
        assert_eq!(result.is_ok(), ok, "{end:#04x}: {result:?}");
        let expected =
            if ok { settled(Pools { reservoir: 0, spilled: 0 }, ended).reservoir } else { given };
        assert_eq!(reservoir_remaining, expected, "{end:#04x}");
        if end == INVALID {
            assert_eq!(gas_used + reservoir_remaining, gas_limit, "the halt burns the regular gas");
        }
    }
}
