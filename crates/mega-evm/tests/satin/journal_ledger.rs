//! The journal's ledger beside the lanes: what it counts of writes made by anything but the
//! transaction's own execution, and how it follows a tool's checkpoints.
//!
//! The ledger counts what the transaction's own execution keeps, at the journal's writes, and the
//! journal's checkpoints rewind it. A precompile's writes through `EvmInternals` and an
//! inspector's are not the transaction's own execution: neither is counted, and neither moves the
//! ledger away from the lanes. A tool's checkpoint around a call is a level of the journal like any
//! other: committed, it changes nothing; reverted, it takes back what the call kept, and the ledger
//! follows it where the lanes, which follow frames, do not.
//!
//! Every test here, as every other, runs under the shadow in a `ledger-shadow` build, which holds
//! the ledger to the lanes wherever the two must agree.

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{address, map::HashMap, Address, Bytes, Log, LogData, B256, U256};
use mega_evm::{
    test_utils::{transfer_log, BytecodeBuilder, MemoryDatabase},
    transaction_body_bytes, LimitUsage, MegaContext, MegaEvm, TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::STOP,
    context::{ContextTr, JournalTr},
    context_interface::journaled_state::JournalCheckpoint,
    handler::FrameResult,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, FrameInput, Gas, InstructionResult,
        InterpreterResult,
    },
    precompile::{PrecompileId, PrecompileOutput},
    Inspector,
};

use crate::common::{call, context};

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

/// Room for every scenario, at any byte price the grid reaches.
const GAS_LIMIT: u64 = 50_000_000;

/// The data size of a body, `records` write records and `logs` transfer logs.
fn kept(body: u64, records: u64, logs: u64) -> LimitUsage {
    LimitUsage {
        data_size: body + records * WRITE_RECORD_SIZE + logs * TRANSFER_LOG_SIZE,
        write_records: records,
    }
}

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
/// log, as for a precompile that writes nothing. The ledger counts what the lanes count.
#[test]
fn test_a_precompiles_writes_through_evm_internals_count_nothing() {
    let code = BytecodeBuilder::default().call(STATEFUL, U256::from(VALUE)).stop().build();
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(VALUE));
    let tx = call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT);
    let body = transaction_body_bytes(&tx);
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
    assert_eq!(outcome.usage, kept(body, 2, 1), "the call's start alone is counted");
    assert_eq!(evm.ctx().journal_ref().ledger_usage(), outcome.usage, "the ledger counts the same");
}

/// An inspector that opens a checkpoint when a call to `target` starts, and commits it or reverts
/// it when the call ends: a tool's checkpoint around a frame start.
struct Encloses {
    target: Address,
    revert: bool,
    open: Vec<JournalCheckpoint>,
}

impl Inspector<MegaContext<MemoryDatabase>, EthInterpreter> for Encloses {
    fn call(
        &mut self,
        context: &mut MegaContext<MemoryDatabase>,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        if inputs.target_address == self.target {
            self.open.push(context.journal_mut().checkpoint());
        }
        None
    }

    fn call_end(
        &mut self,
        context: &mut MegaContext<MemoryDatabase>,
        inputs: &CallInputs,
        _outcome: &mut CallOutcome,
    ) {
        if inputs.target_address != self.target {
            return;
        }
        let checkpoint = self.open.pop().expect("opened when the call started");
        if self.revert {
            context.journal_mut().checkpoint_revert(checkpoint);
        } else {
            context.journal_mut().checkpoint_commit();
        }
    }
}

/// A tool's checkpoint opened as a value call to [`RECEIVER`] starts and closed as it ends, before
/// [`CONTRACT`] makes a second value call, to [`PAYEE`].
///
/// Committed, it changes nothing: both calls keep their starts — the two recipients' records,
/// `CONTRACT`'s own, once, and the two transfer logs — and the ledger counts what the lanes count.
///
/// Reverted, it takes back what the first call kept, its value and its transfer log included, so
/// the receipt carries the second call's log alone; the ledger takes back the first call's records
/// and `CONTRACT`'s flag with it, so the second call records `CONTRACT` again, and the ledger
/// holds what the journal kept: two records and one log. The lanes follow frames, not a tool's
/// checkpoints, and keep counting the first call.
#[test]
fn test_a_tool_checkpoint_around_a_call_closes_with_it() {
    for revert in [false, true] {
        let case = if revert { "reverted" } else { "committed" };
        let code = BytecodeBuilder::default()
            .call(RECEIVER, U256::from(VALUE))
            .call(PAYEE, U256::from(VALUE))
            .stop()
            .build();
        let db = MemoryDatabase::default()
            .account_code(CONTRACT, code)
            .account_balance(CONTRACT, U256::from(2 * VALUE))
            .account_code(RECEIVER, Bytes::from_static(&[STOP]));
        let tx = call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT);
        let body = transaction_body_bytes(&tx);
        let inspector = Encloses { target: RECEIVER, revert, open: Vec::new() };
        let mut evm = MegaEvm::new(context(db)).with_inspector(inspector);
        let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
        let ledger = evm.ctx().journal_ref().ledger_usage();
        let paid = transfer_log(CONTRACT, PAYEE, U256::from(VALUE));
        if revert {
            assert_eq!(outcome.result.logs(), [paid], "{case}: the second call's log alone");
            assert_eq!(
                outcome.state.get(&RECEIVER).map(|account| account.info.balance),
                Some(U256::ZERO),
                "{case}: the first call's value went back"
            );
            assert_eq!(ledger, kept(body, 2, 1), "{case}: what the journal kept");
        } else {
            let received = transfer_log(CONTRACT, RECEIVER, U256::from(VALUE));
            assert_eq!(outcome.result.logs(), [received, paid], "{case}");
            assert_eq!(outcome.usage, kept(body, 3, 2), "{case}");
            assert_eq!(ledger, outcome.usage, "{case}: the ledger counts what the lanes count");
        }
    }
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
/// or one a contract makes, and the ledger counts what the lanes count.
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
        let tx = call(CALLER, to, U256::from(value), GAS_LIMIT);
        let body = transaction_body_bytes(&tx);
        let mut evm = MegaEvm::new(context(db)).with_inspector(AnswersAtFrameStart { target });
        let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
        assert!(outcome.result.logs().is_empty(), "{case}: nothing moved");
        assert_eq!(outcome.usage, kept(body, 0, 0), "{case}");
        assert_eq!(evm.ctx().journal_ref().ledger_usage(), outcome.usage, "{case}");
    }
}
