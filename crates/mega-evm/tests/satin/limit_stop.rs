//! Reading a limit stop off a transaction's result, as an RPC maps it.
//!
//! Every limit stopping a transaction is the stops matrix's (`stops.rs`), which reads each stop
//! through [`MegaTransactionOutcome::limit_stop`] too. Here: revert data that only looks like a
//! stop. The outcome's `limit_exceeded` decides, and a revert carrying a stop's bytes without it is
//! no stop.

use std::collections::BTreeMap;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    decode_mega_limit_exceeded,
    test_utils::{BytecodeBuilder, MemoryDatabase, OutcomeView},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaEvm, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{LOG0, POP, PUSH0},
    context::result::ExecutionResult,
};

use crate::{
    cases::InsertCase,
    common::{call, context},
};

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
    let kinds =
        [LimitKind::DataSize, LimitKind::KVUpdate, LimitKind::ComputeGas, LimitKind::StateGrowth];
    let mut views = BTreeMap::new();
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
        views.insert_case(format!("{kind:?}"), OutcomeView::new(&outcome));
    }
    crate::assert_sorted_json_snapshot!(&views);
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
    crate::assert_sorted_json_snapshot!(&OutcomeView::new(&outcome));
}
