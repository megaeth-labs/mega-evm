//! Inspectors in block execution: any inspector reaches a block, and what an observer sees of its
//! phases.

use alloy_consensus::transaction::Recovered;
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
};
use alloy_evm::{
    block::{BlockExecutor, BlockExecutorFactory},
    EvmFactory, FromRecoveredTx,
};
use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::{
        BytecodeBuilder, CommittedStates, GasInspector, MemoryDatabase, ReadOnlyObserver,
    },
    transaction_body_bytes, BlockLimits, LimitUsage, MegaBlockExecutionCtx, MegaContext,
    MegaEvmFactory, MegaTransaction, MegaTransactionOutcome, MegaTxEnvelope,
};
use revm::{
    bytecode::opcode::{ADD, POP, PUSH0, SLOAD, SSTORE},
    context::{ContextTr, JournalTr},
    database::State,
    handler::FrameResult,
    interpreter::{
        CallInputs, CallOutcome, FrameInput, Gas, InstructionResult, Interpreter,
        InterpreterResult, InterpreterTypes,
    },
    Inspector,
};
use std::cell::Cell;

use crate::common::{self, empty_call_gas, system_tx, user_tx};

/// An inspector reaches a block: the engine refuses none, and a node decides which inspectors run
/// on block production.
#[test]
fn test_an_inspector_reaches_block_execution() {
    let mut state = common::state();
    let factory = common::factory();

    let mut executor = factory.create_executor(
        MegaEvmFactory::new().create_evm_with_inspector(
            &mut state,
            common::evm_env(),
            GasInspector::new(),
        ),
        common::unlimited_ctx(),
    );

    assert!(executor.evm().is_inspecting(), "the inspector runs");
    executor.apply_pre_execution_changes().expect("the pre-block calls run");
    executor.execute_transaction(&user_tx(0, empty_call_gas())).expect("the transaction executes");

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.receipts().len(), 1);
}

/// Counts the frames an inspector sees start and the instructions it sees run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen {
    frames: usize,
    steps: usize,
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for Seen {
    fn step(&mut self, _: &mut Interpreter<INTR>, _: &mut CTX) {
        self.steps += 1;
    }

    fn frame_start(&mut self, _: &mut CTX, _: &mut FrameInput) -> Option<FrameResult> {
        self.frames += 1;
        None
    }
}

/// The state a block runs on, with the EIP-2935 and EIP-4788 contracts' own code, so the
/// pre-block calls run instructions, and a contract that writes a slot.
fn state_with_code() -> State<MemoryDatabase> {
    let mut db = common::database();
    db.set_account_code(HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE.clone());
    db.set_account_code(BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE.clone());
    db.set_account_code(
        common::CONTRACT,
        BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build(),
    );
    State::builder().with_database(db).build()
}

/// A block whose parent hash and beacon root the pre-block calls record.
fn recording_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(
        B256::repeat_byte(0xab),
        Some(B256::repeat_byte(0xcd)),
        Bytes::new(),
        BlockLimits::no_limits(),
    )
}

/// An observer sees each phase of a block as the inspector contract says: nothing of the
/// pre-block calls, which run code of their own, the frames of a system transaction and of a user
/// one, and nothing of the block's end, after which the EVM hands the inspector back with what it
/// saw.
#[test]
fn test_an_observer_sees_the_transactions_of_a_block_and_nothing_else() {
    let mut state = state_with_code();
    let committed = CommittedStates::install(&mut state);
    let factory = common::factory();
    let mut executor = factory.create_executor(
        MegaEvmFactory::new().create_evm_with_inspector(
            &mut state,
            common::evm_env(),
            ReadOnlyObserver::new(Seen::default()),
        ),
        recording_ctx(),
    );

    executor.apply_pre_execution_changes().expect("the pre-block calls run");
    let states = committed.states();
    assert_eq!(
        common::eip_calls(&states),
        [HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS],
        "both pre-block calls ran"
    );
    for target in [HISTORY_STORAGE_ADDRESS, BEACON_ROOTS_ADDRESS] {
        let wrote = common::call_state(&states, target)
            .values()
            .any(|account| account.storage.values().any(|slot| slot.is_changed()));
        assert!(wrote, "the call to {target} wrote");
    }
    assert_eq!(executor.evm().inspector().0, Seen::default(), "and the observer saw none of it");

    let outcome = executor.run_transaction(&system_tx()).expect("a system transaction");
    assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);
    assert!(executor.evm().ctx().is_system_originated(), "it is the protocol's own");
    let after_system = executor.evm().inspector().0;
    assert_eq!(after_system.frames, 1, "the system transaction's call");
    assert!(after_system.steps > 0, "and the Oracle's code");
    executor.commit_transaction(outcome);

    executor.execute_transaction(&user_tx(0, 1_000_000)).expect("a user transaction");
    let after_user = executor.evm().inspector().0;
    assert_eq!(after_user.frames, 2, "the user transaction's call");
    assert!(after_user.steps > after_system.steps);

    let (evm, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.inner.receipts.len(), 2);
    assert_eq!(evm.inspector().0, after_user, "the block's end ran no code");
}

/// An inspector works with the block executor: it
/// sees the transaction's instructions — `CONTRACT` loads a slot, adds one and stores it — each
/// taking gas or none, and the block finishes with the transaction's receipt.
#[test]
fn test_inspector_works_with_block_executor() {
    let increment = BytecodeBuilder::default()
        .append_many([PUSH0, SLOAD])
        .push_number(1_u8)
        .append(ADD)
        .append_many([PUSH0, SSTORE])
        .stop()
        .build();
    let mut db = common::database();
    db.set_account_code(common::CONTRACT, increment);
    let mut state = State::builder().with_database(db).build();
    let factory = common::factory();
    let mut executor = factory.create_executor(
        MegaEvmFactory::new().create_evm_with_inspector(
            &mut state,
            common::evm_env(),
            ReadOnlyObserver::new(GasInspector::new()),
        ),
        common::unlimited_ctx(),
    );

    executor.execute_transaction(&user_tx(0, 1_000_000)).expect("the transaction executes");

    let records = executor.evm().inspector().0.records();
    let opcodes: Vec<_> = records.iter().map(|record| record.opcode.as_str()).collect();
    for opcode in ["SLOAD", "ADD", "SSTORE"] {
        assert!(opcodes.contains(&opcode), "{opcode} ran: {opcodes:?}");
    }
    for record in &records {
        assert!(record.gas_before >= record.gas_after, "no instruction gives gas: {record:?}");
    }
    let (_, result) = executor.finish().expect("the block finishes");
    assert_eq!(result.receipts.len(), 1);
}

/// The contract `CONTRACT` calls in the early-return rows.
const CONTRACT_B: Address = address!("0x3000000000000000000000000000000000000003");

/// Answers every call below the transaction's own frame itself with a success, and counts those
/// answers and every call's end.
#[derive(Default)]
struct SkipNestedCallInspector {
    calls_intercepted: Cell<u32>,
    call_ends: Cell<u32>,
}

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for SkipNestedCallInspector {
    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (context.journal().depth() > 0).then(|| {
            self.calls_intercepted.set(self.calls_intercepted.get() + 1);
            CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                0..0,
            )
        })
    }

    fn call_end(&mut self, _: &mut CTX, _: &CallInputs, _: &mut CallOutcome) {
        self.call_ends.set(self.call_ends.get() + 1);
    }
}

/// Runs `tx` on an EVM over `state` under `inspector`, outside block execution: the route a tool
/// takes.
fn run_as_a_tool<'a, I>(
    state: &'a mut State<MemoryDatabase>,
    tx: &Recovered<MegaTxEnvelope>,
    inspector: &mut I,
) -> (MegaTransaction, MegaTransactionOutcome)
where
    I: Inspector<MegaContext<&'a mut State<MemoryDatabase>>>,
{
    let tx = MegaTransaction::from_recovered_tx(tx.inner(), tx.signer());
    let mut evm =
        MegaEvmFactory::new().create_evm(state, common::evm_env()).with_inspector(inspector);
    let outcome = evm.execute_transaction(tx.clone()).expect("a valid transaction");
    (tx, outcome)
}

/// An inspector that answers the calls below the transaction's own frame rewrites what execution
/// produces. Block execution does not refuse it, and on the EVM alike its answer stands in for the
/// frame: it settles with the share and the start charge the frame was given, the answered call
/// ends as every call does, and the transaction settles as one whose nested call never ran.
#[test]
fn test_inspector_early_return_with_additional_limits() {
    let caller = BytecodeBuilder::default().call(CONTRACT_B, U256::ZERO).append(POP).stop().build();
    let database = || {
        let mut db = common::database();
        db.set_account_code(common::CONTRACT, caller.clone());
        db.set_account_code(CONTRACT_B, BytecodeBuilder::default().stop().build());
        State::builder().with_database(db).build()
    };
    let tx = user_tx(0, 1_000_000);

    let mut state = database();
    let evm = MegaEvmFactory::new()
        .create_evm(&mut state, common::evm_env())
        .with_inspector(SkipNestedCallInspector::default());
    let factory = common::factory();
    let mut executor = factory.create_executor(evm, common::unlimited_ctx());
    let in_block = executor.run_transaction(&tx).expect("block execution admits the inspector");
    assert!(in_block.inner.result.is_success(), "{:?}", in_block.inner.result);
    assert_eq!(executor.evm().inspector().calls_intercepted.get(), 1, "the nested call answered");

    let mut state = database();
    let mut inspector = SkipNestedCallInspector::default();
    let (tx, outcome) = run_as_a_tool(&mut state, &tx, &mut inspector);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(inspector.calls_intercepted.get(), 1, "the nested call was answered");
    assert_eq!(inspector.call_ends.get(), 2, "and both calls ended");
    let body = transaction_body_bytes(&tx);
    assert_eq!(outcome.usage, LimitUsage { data_size: body, write_records: 0 });
    assert_eq!(outcome.gas.state, 0);
    assert_eq!(in_block.inner.usage, outcome.usage, "a block and a tool settle it alike");
}
