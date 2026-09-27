//! Four of Foundry's cheatcodes, driven the way Foundry's inspector drives an EVM, on Satin.
//!
//! A test contract calls the cheatcode address with the cheatcode's ABI call; the inspector's
//! `call` hook answers that call itself, on the gas it was forwarded, as Foundry's does — no frame
//! is built — and does what the cheatcode says through the surfaces every inspector has:
//!
//! - `deal(address,uint256)` sets a balance through the journal;
//! - `store(address,bytes32,bytes32)` writes a slot through the journal;
//! - `pauseGasMetering()` / `resumeGasMetering()` hold the calling frame's gas where it was, in
//!   `step`, by putting the whole of it back before each instruction, memory expansion aside;
//! - `expectRevert()` rewrites the next call's result in `call_end`: a failure into a success that
//!   keeps its revert data, a success into the failure Foundry reports.
//!
//! What each does to Satin's ledgers follows from the inspector contract: a journal write from a
//! callback is on no gas ledger and in no count, a paused region spends no gas of any kind while
//! what it writes is still counted, a failed call `expectRevert` turns into a success settles as
//! the failure on every ledger, and a success it turns into a failure keeps none of its writes.
//! `expectRevert` is honored on a result revm answers without running a frame — a precompile's, a
//! call to an account with no code — as on a frame's.

use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::{sol, SolCall};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    LimitUsage, MegaContext, MegaEvm, MegaTransactionOutcome, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        BALANCE, CALL, GAS, POP, PUSH0, RETURNDATACOPY, RETURNDATASIZE, SLOAD, SSTORE,
    },
    context::{ContextTr, JournalTr},
    context_interface::{cfg::GasId, journaled_state::account::JournaledAccountTr},
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, Gas, InstructionResult, Interpreter,
        InterpreterResult,
    },
    Database, Inspector,
};

use crate::{
    common::{call, context},
    salt::entry,
};

sol! {
    /// The cheatcodes these tests drive, as Foundry's `Vm` interface declares them.
    interface Vm {
        function deal(address who, uint256 newBalance) external;
        function store(address target, bytes32 slot, bytes32 value) external;
        function pauseGasMetering() external;
        function resumeGasMetering() external;
        function expectRevert() external;
    }
}

/// Foundry's cheatcode address.
const CHEATCODES: Address = address!("7109709ecfa91a80626ff3989d68f67f5b1dd12d");
const CALLER: Address = address!("0000000000000000000000000000000000f00000");
/// The test contract.
const TEST: Address = address!("0000000000000000000000000000000000f00001");
/// An account the cheatcodes and calls target.
const TARGET: Address = address!("0000000000000000000000000000000000f00002");
/// The BN254 addition precompile, which a point off the curve fails.
const ADDITION: Address = address!("0000000000000000000000000000000000000006");
/// Below the execution cap, where a transaction has no reservoir.
const GAS_LIMIT: u64 = 10_000_000;
/// Foundry's default gas limit for a test, 2^30: above the execution cap, so a test transaction
/// runs with a reservoir.
const FOUNDRY_GAS_LIMIT: u64 = 1 << 30;

/// The revert data Foundry's `expectRevert` gives a call that did not revert.
const DID_NOT_REVERT: &[u8] = b"call did not revert as expected";

/// Foundry's cheatcode handler, cut down to the four cheatcodes these tests use.
#[derive(Default)]
struct Cheatcodes {
    /// Whether gas metering is paused.
    paused: bool,
    /// The gas the paused frame is held at, once its first step after the pause took it.
    paused_gas: Option<Gas>,
    /// The journal depth of the frame whose next call `expectRevert` rewrites.
    expect_revert_at: Option<usize>,
    /// How many cheatcode calls were answered.
    answered: usize,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Cheatcodes {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        if !self.paused {
            return;
        }
        match self.paused_gas {
            None => self.paused_gas = Some(interp.gas),
            Some(paused) => {
                let memory = *interp.gas.memory();
                interp.gas = paused;
                *interp.gas.memory_mut() = memory;
            }
        }
    }

    fn call(&mut self, ctx: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        if inputs.target_address != CHEATCODES {
            return None;
        }
        let input = inputs.input.bytes(ctx);
        let selector: [u8; 4] = input[..4].try_into().expect("a selector");
        match selector {
            Vm::dealCall::SELECTOR => {
                let deal = Vm::dealCall::abi_decode(&input).expect("deal's arguments");
                let mut account = ctx.journal_mut().load_account_mut(deal.who).expect("loads");
                account.data.set_balance(deal.newBalance);
            }
            Vm::storeCall::SELECTOR => {
                let store = Vm::storeCall::abi_decode(&input).expect("store's arguments");
                let journal = ctx.journal_mut();
                journal.load_account(store.target).expect("loads");
                let slot = U256::from_be_bytes(store.slot.0);
                let value = U256::from_be_bytes(store.value.0);
                journal.sstore(store.target, slot, value).expect("stores");
            }
            Vm::pauseGasMeteringCall::SELECTOR => self.paused = true,
            Vm::resumeGasMeteringCall::SELECTOR => {
                self.paused = false;
                self.paused_gas = None;
            }
            Vm::expectRevertCall::SELECTOR => {
                self.expect_revert_at = Some(ctx.journal_ref().depth())
            }
            _ => panic!("not a cheatcode these tests use"),
        }
        self.answered += 1;
        Some(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                Bytes::new(),
                Gas::new(inputs.gas_limit),
            ),
            inputs.return_memory_offset.clone(),
        ))
    }

    fn call_end(
        &mut self,
        ctx: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if inputs.target_address == CHEATCODES ||
            self.expect_revert_at != Some(ctx.journal_ref().depth())
        {
            return;
        }
        self.expect_revert_at = None;
        let result = &mut outcome.result;
        if result.result.is_ok() {
            result.result = InstructionResult::Revert;
            result.output = Bytes::from_static(DID_NOT_REVERT);
        } else {
            result.result = InstructionResult::Return;
        }
    }
}

/// `code` then a call of the cheatcode `cheat`, its calldata written to memory at offset 0, its
/// success flag dropped.
fn cheat(code: BytecodeBuilder, cheat: impl SolCall) -> BytecodeBuilder {
    let calldata = cheat.abi_encode();
    code.mstore(0, &calldata)
        .append_many([PUSH0, PUSH0])
        .push_number(calldata.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(CHEATCODES)
        .append(GAS)
        .append(CALL)
        .append(POP)
}

/// `code` then a call of `target` carrying `input`, written to memory at offset 0, on a million
/// gas — a failure that burns what it was forwarded leaves the rest; the success flag plus one is
/// stored in slot 0 — a fresh slot whichever way the call goes — and the first word of its return
/// data in slot 1.
fn call_and_note(code: BytecodeBuilder, target: Address, input: &[u8]) -> BytecodeBuilder {
    code.mstore(0, input)
        .append_many([PUSH0, PUSH0])
        .push_number(input.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(target)
        .push_number(1_000_000_u64)
        .append(CALL)
        .push_number(1_u8)
        .append(revm::bytecode::opcode::ADD)
        .append(PUSH0)
        .append(SSTORE)
        .append_many([RETURNDATASIZE, PUSH0, PUSH0, RETURNDATACOPY])
        .append_many([PUSH0, revm::bytecode::opcode::MLOAD])
        .push_number(1_u8)
        .append(SSTORE)
}

/// Runs a call from `CALLER` to `TEST` over `db`, under the cheatcode handler when `cheats` is set.
fn run(db: MemoryDatabase, cheats: bool) -> (MegaTransactionOutcome, usize) {
    run_with_gas_limit(db, cheats, GAS_LIMIT)
}

/// [`run`] with the transaction's gas limit `gas_limit`.
fn run_with_gas_limit(
    db: MemoryDatabase,
    cheats: bool,
    gas_limit: u64,
) -> (MegaTransactionOutcome, usize) {
    let tx = call(CALLER, TEST, U256::ZERO, gas_limit);
    if !cheats {
        return (MegaEvm::new(context(db)).execute_transaction(tx).expect("valid"), 0);
    }
    let mut evm = MegaEvm::new(context(db)).with_inspector(Cheatcodes::default());
    let outcome = evm.execute_transaction(tx).expect("valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    (outcome, evm.inspector().answered)
}

/// The value of `address`'s `slot` in `outcome`'s state.
fn slot(outcome: &MegaTransactionOutcome, address: Address, slot: u64) -> U256 {
    outcome.state[&address].storage[&U256::from(slot)].present_value()
}

/// `deal` sets a balance — up from nothing, creating the account, and down — and the program reads
/// it back with `BALANCE`. The journal write is on no ledger: the account `deal` creates costs no
/// state gas and makes no write record, and the transaction's state gas and records are the slots
/// the program itself writes.
#[test]
fn test_deal_sets_a_balance() {
    let rich = address!("0000000000000000000000000000000000f00003");
    let code = cheat(
        BytecodeBuilder::default(),
        Vm::dealCall { who: TARGET, newBalance: U256::from(1_234) },
    );
    let code = cheat(code, Vm::dealCall { who: rich, newBalance: U256::from(7) });
    let code = code
        .push_address(TARGET)
        .append(BALANCE)
        .append(PUSH0)
        .append(SSTORE)
        .push_address(rich)
        .append(BALANCE)
        .push_number(1_u8)
        .append(SSTORE)
        .stop();
    let db = MemoryDatabase::default()
        .account_code(TEST, code.build())
        .account_balance(rich, U256::from(10_000));
    let (outcome, answered) = run(db, true);
    assert_eq!(answered, 2);
    assert_eq!(outcome.state[&TARGET].info.balance, U256::from(1_234), "up, from nothing");
    assert_eq!(outcome.state[&rich].info.balance, U256::from(7), "and down");
    assert_eq!(
        (slot(&outcome, TEST, 0), slot(&outcome, TEST, 1)),
        (U256::from(1_234), U256::from(7))
    );
    assert_eq!(outcome.gas.state, 2 * entry(GasId::sstore_set_state_gas()), "the two slots alone");
    assert_eq!(outcome.usage.write_records, 2, "the two slots alone");
}

/// `store` writes a slot, and the program reads it back with `SLOAD`. The journal write is on no
/// ledger: the transaction's state gas and records are the slot the program itself writes.
#[test]
fn test_store_writes_a_slot() {
    let value = B256::from(U256::from(0x42));
    let code = cheat(
        BytecodeBuilder::default(),
        Vm::storeCall { target: TARGET, slot: B256::from(U256::from(5)), value },
    );
    // `TEST` reads the slot through a call to `TARGET`, whose code returns it.
    let reader = BytecodeBuilder::default().push_number(5_u8).append(SLOAD).return_top();
    let code = call_and_note(code, TARGET, &[]).stop();
    let db = MemoryDatabase::default()
        .account_code(TEST, code.build())
        .account_code(TARGET, reader.build());
    let (outcome, answered) = run(db, true);
    assert_eq!(answered, 1);
    assert_eq!(slot(&outcome, TARGET, 5), U256::from(0x42), "the slot is in the state");
    assert_eq!(slot(&outcome, TEST, 1), U256::from(0x42), "and the program read it");
    assert_eq!(outcome.gas.state, 2 * entry(GasId::sstore_set_state_gas()), "TEST's two slots");
    assert_eq!(outcome.usage.write_records, 2, "TEST's two slots");
}

/// Paused, gas metering stands still: a region that runs instructions and fills a fresh slot
/// spends nothing on any gas ledger — the state and history ledgers included, the whole gas being
/// put back before each instruction — so the transaction costs exactly what it costs with nothing
/// between the pause and the resume. The write is still a write: it is in the state, and its record
/// in the counts the limits hold and in the history bytes. Unpaused, the same region costs gas.
#[test]
fn test_pause_and_resume_gas_metering_hold_the_gas() {
    let region = |code: BytecodeBuilder| {
        (0..10)
            .fold(code, |code, _| code.append_many([PUSH0, POP]))
            .sstore(U256::from(9), U256::from(1))
    };
    let paused = |work: bool| {
        let code = cheat(BytecodeBuilder::default(), Vm::pauseGasMeteringCall {});
        let code = if work { region(code) } else { code };
        cheat(code, Vm::resumeGasMeteringCall {}).stop().build()
    };
    let db = |code: Bytes| MemoryDatabase::default().account_code(TEST, code);

    let (worked, answered) = run(db(paused(true)), true);
    assert_eq!(answered, 2);
    let (idle, _) = run(db(paused(false)), true);
    assert_eq!(slot(&worked, TEST, 9), U256::from(1), "the slot was written");
    let ledgers = |o: &MegaTransactionOutcome| {
        (o.gas.regular, o.gas.state, o.gas.history, o.gas.gas_used, o.gas.reservoir_remaining)
    };
    assert_eq!(ledgers(&worked), ledgers(&idle), "the region spent nothing");
    assert_eq!(worked.gas.state, 0, "not even the slot's state gas");
    assert_eq!(
        worked.usage,
        LimitUsage {
            data_size: idle.usage.data_size + WRITE_RECORD_SIZE,
            write_records: idle.usage.write_records + 1,
        },
        "the record is counted"
    );
    assert_eq!(worked.gas.history_bytes, idle.gas.history_bytes + WRITE_RECORD_SIZE);

    let unpaused = region(BytecodeBuilder::default()).stop().build();
    let (metered, _) = run(db(unpaused), false);
    assert!(metered.gas.state > 0 && metered.gas.gas_used > 0, "unpaused, the region pays");
}

/// `expectRevert` turns the next call's revert into a success that keeps the revert data. `TARGET`
/// fills a fresh slot and reverts with a word; the program notes the flag and the word. The
/// failed call settles as the failure it was on every ledger — the transaction is billed and
/// counted as it is without the cheatcode — and only the flag the program noted differs.
#[test]
fn test_expect_revert_turns_a_revert_into_a_success_that_keeps_its_reason() {
    let reason = keccak256(b"reason");
    let target = BytecodeBuilder::default()
        .sstore(U256::from(3), U256::from(1))
        .revert_with_data(reason)
        .build();
    let program = |expect: bool| {
        let code = BytecodeBuilder::default();
        let code = if expect { cheat(code, Vm::expectRevertCall {}) } else { code };
        call_and_note(code, TARGET, &[]).stop().build()
    };
    let db = |expect| {
        MemoryDatabase::default()
            .account_code(TEST, program(expect))
            .account_code(TARGET, target.clone())
    };
    let (expected, answered) = run(db(true), true);
    assert_eq!(answered, 1);
    assert_eq!(slot(&expected, TEST, 0), U256::from(2), "the caller saw a success");
    assert_eq!(slot(&expected, TEST, 1), U256::from_be_bytes(reason.0), "with the reason");
    assert!(!expected.state[&TARGET].storage.get(&U256::from(3)).is_some_and(|s| s.is_changed()));

    // Without the handler the same program runs: the cheatcode's call is a call to an account with
    // no code, which costs what the answered one does, and `TARGET`'s call reverts.
    let (reverted, _) = run(db(true), false);
    assert_eq!(slot(&reverted, TEST, 0), U256::from(1), "the caller saw the revert");
    assert_eq!(slot(&reverted, TEST, 1), U256::from_be_bytes(reason.0));
    assert_eq!(expected.gas, reverted.gas, "every ledger");
    assert_eq!(expected.usage, reverted.usage, "every count");
}

/// `expectRevert` is honored on a result revm answers without running a frame. A point off the
/// curve fails the BN254 addition precompile, and the cheatcode turns the failure into a success;
/// a call to an account with no code succeeds, and the cheatcode turns the success into the failure
/// Foundry reports, whose data the program notes.
#[test]
fn test_expect_revert_is_honored_on_a_result_no_frame_ran_to_produce() {
    let mut off_the_curve = [0_u8; 128];
    off_the_curve[31] = 1;
    off_the_curve[63] = 1;
    let program = |target: Address, input: &[u8]| {
        call_and_note(cheat(BytecodeBuilder::default(), Vm::expectRevertCall {}), target, input)
            .stop()
            .build()
    };

    let db = MemoryDatabase::default().account_code(TEST, program(ADDITION, &off_the_curve));
    let (precompile, _) = run(db, true);
    assert_eq!(slot(&precompile, TEST, 0), U256::from(2), "the failure became a success");

    let db = MemoryDatabase::default().account_code(TEST, program(TARGET, &[]));
    let (no_code, _) = run(db, true);
    assert_eq!(slot(&no_code, TEST, 0), U256::from(1), "the success became a failure");
    let mut word = [0_u8; 32];
    word[..DID_NOT_REVERT.len()].copy_from_slice(DID_NOT_REVERT);
    assert_eq!(slot(&no_code, TEST, 1), U256::from_be_bytes(word), "with Foundry's reason");
}

/// At Foundry's default gas limit a test transaction runs above the execution cap, with a
/// reservoir, and Foundry answers every cheatcode call on `Gas::new`, which carries none. The
/// answered frame never ran, so its caller merges back the reservoir it forwarded: a program that
/// calls `deal`, fills a fresh slot and notes `GAS` costs, on every ledger, what it costs where the
/// cheatcode's call reaches an account with no code — its state gas drawn from the reservoir rather
/// than spilled onto regular gas — reads the same `GAS`, and ends with the same reservoir.
#[test]
fn test_a_cheatcode_keeps_the_reservoir_at_foundrys_default_gas_limit() {
    let code =
        cheat(BytecodeBuilder::default(), Vm::dealCall { who: TARGET, newBalance: U256::ONE })
            .sstore(U256::ZERO, U256::ONE)
            .append(GAS)
            .push_number(1_u8)
            .append(SSTORE)
            .stop()
            .build();
    let db = || MemoryDatabase::default().account_code(TEST, code.clone());
    let (cheated, answered) = run_with_gas_limit(db(), true, FOUNDRY_GAS_LIMIT);
    assert_eq!(answered, 1);
    assert_eq!(cheated.state[&TARGET].info.balance, U256::ONE, "the cheatcode ran");
    let (plain, _) = run_with_gas_limit(db(), false, FOUNDRY_GAS_LIMIT);
    assert!(plain.gas.reservoir_remaining > 0, "the transaction runs with a reservoir");
    assert_eq!(cheated.gas, plain.gas, "every ledger, the reservoir included");
    assert_eq!(cheated.usage, plain.usage, "every count");
    assert_eq!(slot(&cheated, TEST, 1), slot(&plain, TEST, 1), "the same `GAS`");
}
