//! The order the four counts are checked in, when one record crosses several at once.
//!
//! A record is counted on the transaction and on the frame that makes it, and each has a
//! data-size and a KV-update count: the transaction against its limits, the frame against its
//! budget. The counts are checked transaction data size, transaction KV updates, frame data size,
//! frame KV updates, and the first one crossed is the one reported. A transaction limit stops the
//! transaction; a frame budget reverts its frame alone, and the caller goes on.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaEvm, MegaLimitExceeded,
    TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::bytecode::opcode::{CALL, GAS, PUSH0, RETURN, RETURNDATACOPY, RETURNDATASIZE};

use crate::common::{call, context};

const CALLER: Address = address!("0000000000000000000000000000000000f20000");
/// The transaction's own frame: two fresh slots, then a call to [`B`], whose output it returns.
const A: Address = address!("0000000000000000000000000000000000f20001");
/// A frame one call down: one fresh slot.
const B: Address = address!("0000000000000000000000000000000000f20002");

/// Room for three fresh slots and the calls around them, at any byte price the grid reaches.
const GAS_LIMIT: u64 = 100_000_000;

/// What the record that crosses is expected to be reported as.
#[derive(Clone, Copy, Debug)]
enum Reported {
    /// The transaction is stopped: `MegaLimitExceeded(kind, limit)`, with the usage that crossed.
    Transaction(LimitKind, u64, u64),
    /// [`B`] reverts alone with `MegaLimitExceeded(kind, budget)`, and [`A`] returns it.
    Frame(LimitKind, u64),
}

/// One record that crosses several of the four counts at once is reported for the first of them
/// in the order transaction data size, transaction KV updates, frame data size, frame KV updates:
/// a record that crosses the frame's data-size budget and the transaction's KV limit stops the
/// transaction for KV updates, and one that crosses both of the frame's budgets reverts the frame
/// for its data size.
///
/// [`A`] keeps two records, so the transaction has 310 + 2 × 40 = 390 bytes and 2 records when
/// [`B`] makes the record that crosses, which brings it to 430 bytes and 3 records, and [`B`]'s
/// own count to 40 bytes and 1 record. `A`'s budget is what the transaction has left, under the
/// per-frame caps; `B`'s is 98/100 of what `A` has left, rounded down. With a per-frame data-size
/// cap of 120, `A` has 120 − 80 = 40 bytes left and `B` gets 39; with a transaction KV limit or a
/// per-frame KV cap of 2, `A` has no record left and `B` gets none. A transaction data-size limit
/// of 390 leaves `A` 80 bytes, and `B` none.
///
/// The rows that cross one count alone show that each count the ordered rows rank is crossed.
///
/// Rules: [S10.47], [S10.48], [S10.31], [S10.33], [S10.34], [S10]. Independence: constants — the
/// usages and budgets are hand arithmetic from `TX_BODY_SIZE`, `WRITE_RECORD_SIZE` and the 98/100
/// share.
#[test]
fn test_a_record_crossing_several_counts_is_reported_for_the_first_in_order() {
    let unlimited = EvmTxRuntimeLimits::no_limits();
    let before = TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE;
    let after = before + WRITE_RECORD_SIZE;
    let rows: [(&str, EvmTxRuntimeLimits, Reported); 5] = [
        (
            "all four: the transaction's data size",
            unlimited.with_tx_data_size_limit(before).with_tx_kv_update_limit(2),
            Reported::Transaction(LimitKind::DataSize, before, after),
        ),
        (
            "the transaction's KV updates and both frame budgets: the transaction's KV updates",
            unlimited.with_tx_kv_update_limit(2).with_frame_data_size_limit(120),
            Reported::Transaction(LimitKind::KVUpdate, 2, 3),
        ),
        (
            "both frame budgets: the frame's data size",
            unlimited.with_frame_data_size_limit(120).with_frame_kv_update_limit(2),
            Reported::Frame(LimitKind::DataSize, 39),
        ),
        (
            "the frame's data-size budget alone",
            unlimited.with_frame_data_size_limit(120),
            Reported::Frame(LimitKind::DataSize, 39),
        ),
        (
            "the frame's KV budget alone",
            unlimited.with_frame_kv_update_limit(2),
            Reported::Frame(LimitKind::KVUpdate, 0),
        ),
    ];
    let a = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(1))
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .append_many([GAS, CALL, RETURNDATASIZE, PUSH0, PUSH0, RETURNDATACOPY, RETURNDATASIZE])
        .append_many([PUSH0, RETURN])
        .build();
    let b = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build();
    for (row, limits, reported) in rows {
        let db = MemoryDatabase::default().account_code(A, a.clone()).account_code(B, b.clone());
        let outcome = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT))
            .expect("the transaction is valid");
        let revert_data = |kind: LimitKind, limit| -> Bytes {
            MegaLimitExceeded { kind: kind.as_u8(), limit }.abi_encode().into()
        };
        match reported {
            Reported::Transaction(kind, limit, used) => {
                assert!(
                    !outcome.result.is_success() && !outcome.result.is_halt(),
                    "{row}: a revert, {:?}",
                    outcome.result
                );
                assert_eq!(outcome.result.output(), Some(&revert_data(kind, limit)), "{row}");
                assert_eq!(
                    outcome.limit_exceeded,
                    Some(LimitCheck::ExceedsLimit { kind, limit, used, frame_local: false }),
                    "{row}"
                );
                assert_eq!(
                    outcome.usage,
                    LimitUsage { data_size: TX_BODY_SIZE, write_records: 0 },
                    "{row}: the frames' records go with the stop"
                );
            }
            Reported::Frame(kind, budget) => {
                assert!(outcome.result.is_success(), "{row}: {:?}", outcome.result);
                assert_eq!(
                    outcome.result.output(),
                    Some(&revert_data(kind, budget)),
                    "{row}: B reverted alone, and A returned what it reverted with"
                );
                assert_eq!(outcome.limit_exceeded, None, "{row}: no stop");
                assert_eq!(
                    outcome.usage,
                    LimitUsage { data_size: before, write_records: 2 },
                    "{row}: A's two records, none of B's"
                );
            }
        }
    }
}
