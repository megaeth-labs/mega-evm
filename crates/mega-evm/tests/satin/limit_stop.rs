//! Reading a limit stop off a transaction's result, as an RPC maps it.
//!
//! Every limit stopping a transaction is the stops matrix's (`stops.rs`), which reads each stop
//! through [`MegaTransactionOutcome::limit_stop`] too. Here: revert data that only looks like a
//! stop, and which crossing the outcome names. The outcome's `limit_exceeded` decides: it names the
//! crossing that produced the result of the transaction's own frame, a frame budget's included,
//! and a revert carrying a stop's bytes without it is no stop.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    decode_mega_limit_exceeded,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitStop, MegaEvm, MegaTransactionOutcome,
    LOG_BASE_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{LOG0, POP, PUSH0},
    context::{result::ExecutionResult, ContextTr},
    context_interface::cfg::GasId,
    interpreter::{CallInputs, CallOutcome, InstructionResult, InterpreterTypes},
    Inspector,
};

use crate::common::{call, context, create, history};

const CALLER: Address = address!("0000000000000000000000000000000000570000");
const A: Address = address!("0000000000000000000000000000000000570001");
const B: Address = address!("0000000000000000000000000000000000570002");

/// Runs a call from `CALLER` to `A` over `db` under `limits`.
fn execute(db: MemoryDatabase, limits: EvmTxRuntimeLimits) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(call(CALLER, A, U256::ZERO, 10_000_000))
        .expect("the transaction is valid")
}

/// The output of a transaction that reverted.
fn reverted_with(outcome: &MegaTransactionOutcome) -> &Bytes {
    match &outcome.result {
        ExecutionResult::Revert { output, .. } => output,
        other => panic!("expected a revert, got {other:?}"),
    }
}

/// A contract that reverts with a stop's exact bytes, of every kind: the output decodes as the
/// stop's, the outcome names no limit, and no stop is read.
#[test]
fn test_a_contract_reverting_with_a_stops_bytes_is_no_stop() {
    let kinds = [LimitKind::DataSize, LimitKind::ComputeGas];
    for kind in kinds {
        let data =
            LimitCheck::ExceedsLimit { kind, limit: 7, used: 8, frame_local: false }.revert_data();
        let code = BytecodeBuilder::default().revert_with_data(&data).build();
        let outcome =
            execute(MemoryDatabase::default().account_code(A, code), EvmTxRuntimeLimits::default());

        let output = reverted_with(&outcome);
        assert_eq!(output, &data, "{kind:?}");
        assert_eq!(decode_mega_limit_exceeded(output), Some((kind, 7)), "{kind:?}: it decodes");
        assert_eq!(outcome.limit_exceeded, None, "{kind:?}");
        assert_eq!(outcome.limit_stop(), None, "{kind:?}: and it is no stop");
    }
}

/// A frame its data-size budget stopped reverts alone with a stop's bytes, and its caller runs
/// on. A caller that re-raises them ends the transaction on them, and the transaction was not
/// stopped: no limit of its own was crossed.
#[test]
fn test_a_frame_budget_re_raised_by_its_caller_is_no_stop() {
    const FRAME_CAP: u64 = 1_000;
    let logs_past_the_cap =
        BytecodeBuilder::default().push_number(2 * FRAME_CAP).append_many([PUSH0, LOG0]).stop();
    let re_raises =
        BytecodeBuilder::default().call(B, U256::ZERO).append(POP).revert_with_returndata();
    let db = MemoryDatabase::default()
        .account_code(A, re_raises.build())
        .account_code(B, logs_past_the_cap.build());
    let outcome =
        execute(db, EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(FRAME_CAP));

    let decoded = decode_mega_limit_exceeded(reverted_with(&outcome));
    assert!(
        matches!(decoded, Some((LimitKind::DataSize, budget)) if budget <= FRAME_CAP),
        "the output is the frame budget's stop: {decoded:?}"
    );
    assert_eq!(outcome.limit_exceeded, None);
    assert_eq!(outcome.limit_stop(), None);
}

/// Limits holding nothing but every frame's data size, to `cap` bytes.
fn frame_capped(cap: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(cap)
}

/// `A` logging `FRAME_CAP` bytes, which with the log's own 32 cross its frame cap of as many.
const FRAME_CAP: u64 = 1_000;

/// A database where `A` logs past [`FRAME_CAP`].
fn logging_past_the_cap() -> MemoryDatabase {
    let code = BytecodeBuilder::default().push_number(FRAME_CAP).append_many([PUSH0, LOG0]).stop();
    MemoryDatabase::default().account_code(A, code.build())
}

/// The transaction's own frame stopped by its own data-size budget ends the transaction on that
/// crossing: the outcome names it, a frame budget though it is, and the stop is read off the
/// result.
#[test]
fn test_the_transactions_own_frame_stopped_by_its_budget_is_a_stop() {
    let outcome = execute(logging_past_the_cap(), frame_capped(FRAME_CAP));

    let used = LOG_BASE_SIZE + FRAME_CAP;
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit: FRAME_CAP,
        used,
        frame_local: true,
    };
    assert_eq!(reverted_with(&outcome), &stop.revert_data());
    assert_eq!(outcome.limit_exceeded, Some(stop));
    assert_eq!(
        outcome.limit_stop(),
        Some(LimitStop { kind: LimitKind::DataSize, limit: FRAME_CAP, used })
    );
}

/// A creation transaction whose deposit crosses its own frame's budget is stopped by it: the
/// creation's frame records the account it creates, then the code it would deposit, one byte more
/// than its budget holds. The reservoir of a gas limit far above the execution cap pays the
/// deposit's state gas and history at the byte prices in effect, so the budget is what refuses it.
#[test]
fn test_a_creation_whose_deposit_crosses_its_budget_is_a_stop() {
    const CODE: u64 = 100;
    let init = BytecodeBuilder::default().return_with_data([0_u8; CODE as usize]).build();
    let used = WRITE_RECORD_SIZE + CODE;
    let cap = used - 1;
    let roomy = TX_GAS_LIMIT_CAP +
        CODE * mega_evm::satin_gas_params().get(GasId::code_deposit_state_gas()) +
        history(CODE) +
        1_000_000_000;
    let outcome =
        MegaEvm::new(context(MemoryDatabase::default()).with_tx_runtime_limits(frame_capped(cap)))
            .execute_transaction(create(CALLER, init, roomy))
            .expect("the transaction is valid");

    let stop =
        LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, limit: cap, used, frame_local: true };
    assert_eq!(reverted_with(&outcome), &stop.revert_data());
    assert_eq!(outcome.limit_exceeded, Some(stop));
    assert_eq!(
        outcome.limit_stop(),
        Some(LimitStop { kind: LimitKind::DataSize, limit: cap, used })
    );
}

/// Rewrites every call's result into a success, keeping its output.
#[derive(Debug, Default)]
struct Revives;

impl<CTX: ContextTr, INTR: InterpreterTypes> Inspector<CTX, INTR> for Revives {
    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        outcome.result.result = InstructionResult::Return;
    }
}

/// The transaction's own frame stopped by its budget and turned into a success by an inspector
/// ends on the success, the stop's bytes for its output: the crossing no longer produced the
/// result, which is no revert, so the outcome names none.
#[test]
fn test_a_stop_an_inspector_turns_into_a_success_is_no_stop() {
    let ctx = context(logging_past_the_cap()).with_tx_runtime_limits(frame_capped(FRAME_CAP));
    let outcome = MegaEvm::new(ctx)
        .with_inspector(Revives)
        .execute_transaction(call(CALLER, A, U256::ZERO, 10_000_000))
        .expect("the transaction is valid");

    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit: FRAME_CAP,
        used: LOG_BASE_SIZE + FRAME_CAP,
        frame_local: true,
    };
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("expected a success, got {:?}", outcome.result)
    };
    assert_eq!(output.data(), &stop.revert_data(), "the stop's bytes, in a success");
    assert_eq!(outcome.limit_exceeded, None);
    assert_eq!(outcome.limit_stop(), None);
}
