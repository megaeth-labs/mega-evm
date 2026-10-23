//! The data size of the writes made by anything but the transaction's own execution.
//!
//! What a transaction keeps is counted where its own execution writes: at the completed-write and
//! frame-start hooks, and where revm decides a start. A precompile's writes through
//! `EvmInternals` and an inspector's are not the transaction's own execution, and neither is
//! counted; neither is a start an inspector answers before revm decides it. The one exception is a
//! precompile's in a success answer, which revm runs inside the call's start: the start is read
//! off the journal once revm decided it, so what the precompile changes of the start's own
//! accounts, and a transfer log it journals, are read with the start.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{address, map::HashMap, Address, Bytes, Log, LogData, B256, U256};
use mega_evm::{
    test_utils::{transfer_log, BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm,
};
use revm::{
    bytecode::opcode::STOP,
    handler::FrameResult,
    interpreter::{
        interpreter::EthInterpreter, CallOutcome, FrameInput, Gas, InstructionResult,
        InterpreterResult,
    },
    precompile::{PrecompileId, PrecompileOutput},
    Inspector,
};

use crate::common::{call, context, grown, records_and_moves, value_call_room};

const CALLER: Address = address!("0000000000000000000000000000000000e10000");
/// The transaction's own frame.
const CONTRACT: Address = address!("0000000000000000000000000000000000e10001");
/// A contract whose code stops.
const RECEIVER: Address = address!("0000000000000000000000000000000000e10002");
/// An account with no code.
const PAYEE: Address = address!("0000000000000000000000000000000000e10003");
/// A node's own precompile, which writes through `EvmInternals`.
const STATEFUL: Address = address!("0000000000000000000000000000000000e10100");

/// What every value call moves.
const VALUE: u64 = 1_000;

/// A log of `address`'s with one topic and no data.
fn marker(address: Address, topic: u8) -> Log {
    Log { address, data: LogData::new_unchecked(vec![B256::repeat_byte(topic)], Bytes::new()) }
}

/// A precompile that writes through `EvmInternals` everything the API lets it: two slots, one of
/// them inside a checkpoint it commits, a log, a value transfer to [`PAYEE`], and, inside a
/// checkpoint it reverts, a third slot and a second log.
fn stateful() -> DynPrecompile {
    DynPrecompile::new(PrecompileId::Custom("stateful".into()), |mut input| {
        let internals = &mut input.internals;
        internals.sstore(STATEFUL, U256::from(1), U256::from(7)).unwrap();
        internals.log(marker(STATEFUL, 1));
        assert_eq!(internals.transfer(STATEFUL, PAYEE, U256::from(1)).unwrap(), None);
        let _ = internals.checkpoint();
        internals.sstore(STATEFUL, U256::from(2), U256::from(7)).unwrap();
        internals.checkpoint_commit();
        let checkpoint = internals.checkpoint();
        internals.sstore(STATEFUL, U256::from(3), U256::from(7)).unwrap();
        internals.log(marker(STATEFUL, 2));
        internals.checkpoint_revert(checkpoint);
        Ok(PrecompileOutput::new(0, Bytes::new(), input.reservoir))
    })
}

/// A node's own precompile that writes through `EvmInternals` — storage, a log, a value transfer,
/// a checkpoint it commits and one it reverts — keeps its writes in the state and the receipt, and
/// counts none of them: the transaction's data size is its body and the start of the value call
/// that reached the precompile, the precompile's record and its caller's and the call's transfer
/// log, as for a precompile that writes nothing. revm runs the precompile inside the call's start,
/// and the start is read off the journal once revm decided it, so what a precompile changes of the
/// call's own accounts, and a transfer log it journals, would be read with the start, which counts
/// one transfer log; its storage writes and its writes to any other account are not. This one's
/// transfer leaves the precompile's account changed, as the call's value did, its transfer log is
/// the start's second, and [`PAYEE`] is no account of the start.
#[test]
fn test_a_precompiles_writes_through_evm_internals_count_nothing() {
    let code = BytecodeBuilder::default().call(STATEFUL, U256::from(VALUE)).stop().build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(VALUE));
    let tx = call(CALLER, CONTRACT, U256::ZERO, value_call_room());
    let mut evm = MegaEvm::new(context(db))
        .with_dyn_precompiles(HashMap::from_iter([(STATEFUL, stateful())]));
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let storage = |slot: u64| {
        outcome.state[&STATEFUL].storage.get(&U256::from(slot)).map(|slot| slot.present_value)
    };
    assert_eq!(storage(1), Some(U256::from(7)), "the precompile's write is kept");
    assert_eq!(storage(2), Some(U256::from(7)), "and the one it committed");
    assert_ne!(storage(3), Some(U256::from(7)), "the one it reverted is not");
    assert_eq!(outcome.state[&PAYEE].info.balance, U256::from(1), "its transfer moved");
    assert_eq!(
        outcome.result.logs(),
        [
            transfer_log(CONTRACT, STATEFUL, U256::from(VALUE)),
            marker(STATEFUL, 1),
            transfer_log(STATEFUL, PAYEE, U256::from(1)),
        ],
        "the receipt carries the call's transfer log, then the precompile's log and transfer log",
    );
    assert_eq!(
        outcome.usage,
        grown(records_and_moves(2, 1), 1),
        "the call's start alone is counted, whose value creates STATEFUL"
    );
}

/// A precompile that moves one wei of its own to [`CONTRACT`], which calls it, through
/// `EvmInternals`.
fn paying_its_caller() -> DynPrecompile {
    DynPrecompile::new(PrecompileId::Custom("paying its caller".into()), |mut input| {
        assert_eq!(input.internals.transfer(STATEFUL, CONTRACT, U256::from(1)).unwrap(), None);
        Ok(PrecompileOutput::new(0, Bytes::new(), input.reservoir))
    })
}

/// A node's own precompile that, reached by a call moving no value, moves value between the call's
/// own accounts in its success answer has that move read with the call's start, which revm runs it
/// inside: the start counts both accounts' records and the precompile's transfer log, as a value
/// call between them would have, where the call alone counts nothing.
#[test]
fn test_a_precompiles_writes_to_the_starts_own_accounts_are_read_with_the_start() {
    let code = BytecodeBuilder::default().call(STATEFUL, U256::ZERO).stop().build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, code)
        .account_balance(STATEFUL, U256::from(VALUE));
    let tx = call(CALLER, CONTRACT, U256::ZERO, value_call_room());
    let mut evm = MegaEvm::new(context(db))
        .with_dyn_precompiles(HashMap::from_iter([(STATEFUL, paying_its_caller())]));
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.state[&CONTRACT].info.balance, U256::from(1), "the precompile's move");
    assert_eq!(outcome.result.logs(), [transfer_log(STATEFUL, CONTRACT, U256::from(1))]);
    assert_eq!(
        outcome.usage,
        records_and_moves(2, 1),
        "the call's two accounts and the precompile's transfer log, read with the start"
    );
}

/// An inspector that answers, in `frame_start`, the call to `target` with a success on the gas
/// the call was given.
struct AnswersAtFrameStart {
    target: Address,
}

impl Inspector<MegaContext<MemoryDatabase>, EthInterpreter> for AnswersAtFrameStart {
    fn frame_start(
        &mut self,
        _context: &mut MegaContext<MemoryDatabase>,
        frame_input: &mut FrameInput,
    ) -> Option<FrameResult> {
        let FrameInput::Call(inputs) = frame_input else { return None };
        (inputs.target_address == self.target).then(|| {
            FrameResult::Call(CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            ))
        })
    }
}

/// A value call an inspector answers in `frame_start`, before revm decides it, starts nothing:
/// the transaction keeps its body alone, whether the answered call is the transaction's own frame
/// or one a contract makes.
#[test]
fn test_a_value_call_answered_at_frame_start_counts_nothing() {
    let code = BytecodeBuilder::default().call(RECEIVER, U256::from(VALUE)).stop().build();
    for (case, to, target) in [
        ("below the transaction's frame", CONTRACT, RECEIVER),
        ("the transaction's frame", RECEIVER, RECEIVER),
    ] {
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, code.clone())
            .account_balance(CONTRACT, U256::from(VALUE))
            .account_balance(CALLER, U256::from(VALUE))
            .account_code(RECEIVER, Bytes::from_static(&[STOP]));
        let value = if to == RECEIVER { VALUE } else { 0 };
        let tx = call(CALLER, to, U256::from(value), value_call_room());
        let mut evm = MegaEvm::new(context(db)).with_inspector(AnswersAtFrameStart { target });
        let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
        assert!(outcome.result.logs().is_empty(), "{case}: nothing moved");
        assert_eq!(outcome.usage, records_and_moves(0, 0), "{case}");
    }
}
