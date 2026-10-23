//! The regular gas a call or creation forwards: at most
//! `parent_remaining − parent_remaining × 2 / 100` of what its caller has left after the charges
//! the opcode makes before the forward, Rex6's rule in place of EIP-150's 63/64. A value call's
//! 2,300 stipend joins the forward after the cap and is not taken from the caller; a creation
//! forwards the cap and adds none.
//!
//! An inspector reads the caller's regular gas at the end of the opcode that starts a frame —
//! after the forward is taken, before the frame runs — and the forward off the frame's input, so
//! what the caller had at the forward is the two added up.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLCODE, CREATE, CREATE2, DELEGATECALL, POP, PUSH0, PUSH20, STATICCALL, STOP,
    },
    context_interface::cfg::GasId,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Interpreter,
    },
    Database, Inspector,
};

use crate::common::{
    account_state_gas, body_history, call, context, forward_cap, history, holding_to_keep,
};

const SENDER: Address = address!("0000000000000000000000000000000000730000");
const A: Address = address!("0000000000000000000000000000000000730001");
const B: Address = address!("0000000000000000000000000000000000730002");

/// The value call's stipend.
const STIPEND: u64 = 2_300;

/// One frame start seen by [`Forwards`]: the opcode, the caller's regular gas once the opcode took
/// the forward, and the gas limit of the frame it starts.
#[derive(Clone, Copy, Debug)]
struct Start {
    opcode: u8,
    kept: u64,
    gas_limit: u64,
}

/// Records every frame start of the transaction's own frame: the opcode that made it, the regular
/// gas that frame kept, and the frame's gas limit.
#[derive(Default)]
struct Forwards {
    opcode: Option<u8>,
    kept: u64,
    starts: Vec<Start>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Forwards {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.opcode = Some(interp.bytecode.opcode());
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.kept = interp.gas.remaining();
    }

    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if let Some(opcode) = self.opcode.take() {
            self.starts.push(Start { opcode, kept: self.kept, gas_limit: inputs.gas_limit });
        }
        None
    }

    fn create(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        if let Some(opcode) = self.opcode.take() {
            self.starts.push(Start { opcode, kept: self.kept, gas_limit: inputs.gas_limit() });
        }
        None
    }
}

/// `A` starts one frame with `opcode`, asking for all its gas, and stops; `B` stops. A call carries
/// `value`, a creation's init code is a `STOP`.
fn starting(opcode: u8, value: u64) -> MemoryDatabase {
    let code = match opcode {
        CREATE => BytecodeBuilder::default().create(U256::ZERO, [STOP]).append(POP),
        CREATE2 => BytecodeBuilder::default().create2(U256::ZERO, [STOP], U256::ZERO).append(POP),
        CALL | CALLCODE => {
            let code = BytecodeBuilder::default().append_many([PUSH0, PUSH0, PUSH0, PUSH0]);
            code.push_number(value).append(PUSH20).append_many(B.into_array()).append_many([
                revm::bytecode::opcode::GAS,
                opcode,
                POP,
            ])
        }
        _ => BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH20])
            .append_many(B.into_array())
            .append_many([revm::bytecode::opcode::GAS, opcode, POP]),
    };
    MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10_u64.pow(18)))
        .account_balance(A, U256::from(1))
        .account_code(A, code.stop().build())
        .account_code(B, Bytes::from_static(&[STOP]))
}

/// The one start `A` makes with `opcode` at `gas_limit`.
fn start_of(opcode: u8, value: u64, gas_limit: u64) -> Start {
    let mut evm =
        MegaEvm::new(context(starting(opcode, value))).with_inspector(Forwards::default());
    let outcome = evm
        .execute_transaction(call(SENDER, A, U256::ZERO, gas_limit))
        .expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{opcode:#04x}: {:?}", outcome.result);
    let [start] = evm.inspector().starts[..] else {
        panic!("{opcode:#04x}: one start: {:?}", evm.inspector().starts)
    };
    assert_eq!(start.opcode, opcode);
    start
}

/// Each of the six opcodes, asking for all its gas, forwards all but 2 per cent of what its caller
/// has left, and the caller keeps the 2 per cent; a value call's child gets the stipend on top,
/// which the caller does not pay. Below and above the execution cap alike. A creation's start
/// charges the history of its two records — the created account and the creator's nonce — after
/// the forward, which spills onto the caller's regular gas where there is no reservoir; above the
/// cap the reservoir pays it, as it pays a value call's, so the caller's regular gas is the
/// forward's alone.
#[test]
fn test_a_start_forwards_all_but_two_percent_of_its_callers_regular_gas() {
    // The smallest tier holds, below the cap, the body's history and the created account's state
    // gas before the forward, and fifty times the records' history after it, which the 2 per cent
    // kept must pay, with a million to spare.
    let small = account_state_gas() +
        body_history(0) +
        holding_to_keep(history(2 * WRITE_RECORD_SIZE)) +
        1_000_000;
    for gas_limit in [small, 30_000_000.max(small), TX_GAS_LIMIT_CAP + 10_000_000] {
        for (opcode, value) in [
            (CALL, 0),
            (CALLCODE, 0),
            (DELEGATECALL, 0),
            (STATICCALL, 0),
            (CREATE, 0),
            (CREATE2, 0),
        ] {
            let Start { kept, gas_limit: forwarded, .. } = start_of(opcode, value, gas_limit);
            let creation = matches!(opcode, CREATE | CREATE2);
            let records = if creation && gas_limit <= TX_GAS_LIMIT_CAP {
                history(2 * WRITE_RECORD_SIZE)
            } else {
                0
            };
            let had = kept + records + forwarded;
            assert_eq!(forwarded, forward_cap(had), "{opcode:#04x} at {gas_limit}");
            assert_eq!(
                kept + records,
                had * 2 / 100,
                "{opcode:#04x} at {gas_limit}: the caller keeps 2%"
            );
        }
    }
    let above = TX_GAS_LIMIT_CAP + 10_000_000;
    for opcode in [CALL, CALLCODE] {
        let Start { kept, gas_limit, .. } = start_of(opcode, 1, above);
        let forwarded = gas_limit - STIPEND;
        let had = kept + forwarded;
        assert_eq!(forwarded, forward_cap(had), "{opcode:#04x} with value");
        assert_eq!(
            kept,
            had * 2 / 100,
            "{opcode:#04x} with value: the stipend is not the caller's"
        );
    }
}

/// The spec's worked example: a value call whose caller has 10,000 regular gas left after the
/// transfer's charge forwards a cap of 9,800, so its child gets 12,100 and the caller keeps 200;
/// EIP-150 would have forwarded 9,844.
#[test]
fn test_a_value_call_with_ten_thousand_left_forwards_9800_and_its_child_gets_12100() {
    assert_eq!(forward_cap(10_000), 9_800);
    assert_eq!(forward_cap(10_000) + STIPEND, 12_100);
    assert_eq!(10_000 - forward_cap(10_000), 200);
    assert_eq!(10_000 - 10_000 / 64, 9_844);
    for kept in [0, 1, 7_039] {
        let held = holding_to_keep(kept);
        assert_eq!(held - forward_cap(held), kept, "{held} keeps {kept}");
        assert_eq!(held + 1 - forward_cap(held + 1), kept + 1, "one gas more keeps one more");
    }
}

/// The regular gas a frame holds as its call or creation starts, before the opcode's charges, and
/// what it forwards.
#[derive(Default)]
struct Held {
    held: Option<u64>,
    forwarded: Option<u64>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Held {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        if matches!(interp.bytecode.opcode(), CALL | CREATE) {
            self.held = Some(interp.gas.remaining());
        }
    }

    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address == B {
            self.forwarded = Some(inputs.gas_limit);
        }
        None
    }

    fn create(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.forwarded = Some(inputs.gas_limit());
        None
    }
}

/// Runs `A` holding `code` above the execution cap, where the reservoir pays every state and
/// history charge, and returns the regular gas held at the opcode and the gas forwarded.
fn held_and_forwarded(code: Bytes) -> (u64, u64) {
    let db = MemoryDatabase::default()
        .account_balance(SENDER, U256::from(10_u64.pow(18)))
        .account_balance(B, U256::from(1))
        .account_code(A, code);
    let mut evm = MegaEvm::new(context(db)).with_inspector(Held::default());
    let outcome = evm
        .execute_transaction(call(SENDER, A, U256::ZERO, 5 * TX_GAS_LIMIT_CAP))
        .expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let held = evm.inspector();
    (held.held.expect("the opcode ran"), held.forwarded.expect("a frame started"))
}

/// A `CALL` given all the gas forwards all but 2 per cent of what its caller holds once the call's
/// own charge — the cold access to its target — is paid.
#[test]
fn test_a_call_forwards_all_but_two_percent_of_what_its_charge_leaves() {
    let params = mega_evm::satin_gas_params();
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0, PUSH20])
        .append_many(B.into_array())
        .append_many([revm::bytecode::opcode::GAS, CALL, STOP])
        .build();
    let (held, forwarded) = held_and_forwarded(code);
    let access = params.get(GasId::warm_storage_read_cost()) +
        params.get(GasId::cold_account_additional_cost());
    let left = held - access;
    assert_eq!(forwarded, forward_cap(left));
}

/// A `CREATE` forwards all but 2 per cent of what its creator holds once the creation's own
/// charges — the `create` entry, EIP-3860's word and the memory the init code occupies — are paid;
/// the created account's state gas comes out of the reservoir.
#[test]
fn test_a_creation_forwards_all_but_two_percent_of_what_its_charges_leave() {
    let params = mega_evm::satin_gas_params();
    let code = BytecodeBuilder::default()
        .push_number(10_u8)
        .append_many([PUSH0, PUSH0, CREATE, STOP])
        .build();
    let (held, forwarded) = held_and_forwarded(code);
    let left = held - params.get(GasId::create()) - params.get(GasId::initcode_per_word()) - 3;
    assert_eq!(forwarded, forward_cap(left));
}
