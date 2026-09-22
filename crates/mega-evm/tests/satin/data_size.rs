//! The data-size limit: the 98% a child frame receives, and what crossing it does.
//!
//! A child frame's budget is 98% of what its parent has left. Crossing that budget reverts the
//! child alone and its parent resumes. Crossing the transaction's own limit stops the transaction
//! through the same latch every other transaction-level limit uses.

use alloy_evm::Evm;
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::BytecodeBuilder, EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaEvm,
    MegaLimitExceeded, FRAME_DATA_SHARE_DENOMINATOR, FRAME_DATA_SHARE_NUMERATOR,
};
use revm::bytecode::opcode::{CALL, GAS, LOG0, POP, PUSH0, STOP};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000300000");
const A: Address = address!("0000000000000000000000000000000000300001");
const B: Address = address!("0000000000000000000000000000000000300002");
const C: Address = address!("0000000000000000000000000000000000300003");
const D: Address = address!("0000000000000000000000000000000000300004");

const GAS_LIMIT: u64 = 20_000_000;

/// 98% of `remaining`, the share a child frame is given.
fn share(remaining: u64) -> u64 {
    (u128::from(remaining) * u128::from(FRAME_DATA_SHARE_NUMERATOR) /
        u128::from(FRAME_DATA_SHARE_DENOMINATOR)) as u64
}

/// Calls `next` with no value, then writes slot 2: the write is what a resumed caller runs.
fn call_then_store(next: Address) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(next)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .sstore(U256::from(2), U256::from(1))
        .stop()
        .build()
}

/// `LOG0` of `data_len` bytes at memory offset 0.
fn log0(data_len: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(data_len)
        .push_number(0_u64)
        .append(LOG0)
        .append(STOP)
        .build()
}

/// `A -> B -> C -> D`. `D` is at depth 3.
fn chain(innermost: Bytes) -> mega_evm::test_utils::MemoryDatabase {
    mega_evm::test_utils::MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(A, call_then_store(B))
        .account_code(B, call_then_store(C))
        .account_code(C, call_then_store(D))
        .account_code(D, innermost)
}

fn stored(state: &revm::state::EvmState, address: Address) -> bool {
    state.get(&address).is_some_and(|account| {
        account.storage.get(&U256::from(2)).is_some_and(|slot| slot.is_changed())
    })
}

/// The budget at depth 3 when every frame cap is `cap` and no frame has spent any of it before
/// the call that starts the next one.
fn budget_at_depth_3(cap: u64) -> u64 {
    share(share(share(cap)))
}

/// A log that fills `D`'s budget is kept, and `A`, `B` and `C` resume. One byte more reverts
/// `D` alone: the three callers still write the slot after the call, and the transaction is not
/// latched.
#[test]
fn test_depth_3_frame_budget_reverts_the_child_and_the_parents_resume() {
    const CAP: u64 = 20_000;
    let budget = budget_at_depth_3(CAP);
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(CAP);
    let run = |data_len: u64| {
        let db = chain(log0(data_len));
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
        let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
        (
            result,
            evm.ctx().additional_limit().latched().copied(),
            evm.ctx().additional_limit().usage(),
        )
    };

    // 32 bytes for the log's address, and the data. Exactly the budget.
    let (fits, latched, usage) = run(budget - 32);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(latched, None);
    assert!(stored(&fits.state, A) && stored(&fits.state, B) && stored(&fits.state, C));
    assert_eq!(fits.result.logs().len(), 1);
    assert_eq!(usage.write_records, 3, "one slot in each caller");
    assert_eq!(usage.data_size, budget + 3 * 40);

    let (over, latched, usage) = run(budget - 31);
    assert!(over.result.is_success(), "the callers resume: {:?}", over.result);
    assert_eq!(latched, None, "a frame budget does not latch the transaction");
    assert!(stored(&over.state, A) && stored(&over.state, B) && stored(&over.state, C));
    assert!(over.result.logs().is_empty(), "the log died with D");
    assert_eq!(usage, mega_evm::LimitUsage { data_size: 3 * 40, write_records: 3 });
}

/// The same depth, with the transaction's limit tighter than the frame budgets. `D`'s log crosses
/// the transaction limit, so the latch stops every caller: the slots they wrote are not kept, and
/// the revert names the transaction limit.
#[test]
fn test_depth_3_transaction_limit_stops_the_transaction() {
    const TX_LIMIT: u64 = 8_000;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_LIMIT);
    let db = chain(log0(TX_LIMIT));
    let outcome = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT))
        .unwrap();

    assert!(!outcome.result.is_success());
    assert!(!outcome.result.is_halt(), "a data-size stop is a revert: {:?}", outcome.result);
    // Nothing was written before the log, so the crossing usage is the log itself: 32 for its
    // address and `TX_LIMIT` of data. The revert then drops it; the latch keeps the figure.
    let crossed = 32 + TX_LIMIT;
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: TX_LIMIT,
            used: crossed,
            frame_local: false,
        })
    );
    assert_eq!(outcome.usage.data_size, 0, "the revert drops the log that crossed");
    match &outcome.result {
        revm::context::result::ExecutionResult::Revert { output, .. } => {
            assert_eq!(
                output.as_ref(),
                MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: TX_LIMIT }
                    .abi_encode()
            );
        }
        other => panic!("expected the latched revert, got {other:?}"),
    }
    assert!(!stored(&outcome.state, A) && !stored(&outcome.state, B) && !stored(&outcome.state, C));

    // The same chain without the log stays inside the limit and keeps every caller's slot.
    let kept = MegaEvm::new(
        context(chain(BytecodeBuilder::default().append(STOP).build()))
            .with_tx_runtime_limits(limits),
    )
    .execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT))
    .unwrap();
    assert!(kept.result.is_success(), "{:?}", kept.result);
    assert_eq!(kept.limit_exceeded, None);
    assert!(stored(&kept.state, A) && stored(&kept.state, B) && stored(&kept.state, C));
}
