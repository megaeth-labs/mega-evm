//! The order the two data-size counts are checked in, when one record crosses both at once.
//!
//! A record is counted on the transaction and on the frame that makes it: the transaction against
//! its limit, the frame against its budget. The frame's budget is checked first, and the first
//! count crossed is the one the stop names. Either reverts the frame that crossed it alone, and the
//! caller goes on.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitKind, LimitUsage, MegaLimitExceeded, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::bytecode::opcode::{CALL, GAS, PUSH0, RETURN, RETURNDATACOPY, RETURNDATASIZE};

use crate::common::{call, execute_under};

const CALLER: Address = address!("0000000000000000000000000000000000f20000");
/// The transaction's own frame: two fresh slots, then a call to [`B`], whose output it returns.
const A: Address = address!("0000000000000000000000000000000000f20001");
/// A frame one call down: one fresh slot.
const B: Address = address!("0000000000000000000000000000000000f20002");

/// Room for three fresh slots and the calls around them, at any byte price the grid reaches.
const GAS_LIMIT: u64 = 100_000_000;

/// One record that crosses the transaction's data-size limit and its frame's budget at once is
/// stopped with the frame's budget, which is checked first.
///
/// [`A`] keeps two records, so the transaction has 310 + 2 × 40 = 390 bytes when [`B`] makes the
/// record that crosses, which brings it to 430 bytes, and [`B`]'s own count to 40 bytes. `A`'s
/// budget is what the transaction has left, under the per-frame cap; `B`'s is 98/100 of what `A`
/// has left, rounded down. With a per-frame cap of 120, `A` has 120 − 80 = 40 bytes left and `B`
/// gets 39. A transaction limit of 390 leaves `A` 80 bytes, and `B` none.
///
/// Either way [`B`] reverts alone with `MegaLimitExceeded(kind, limit)` and [`A`] returns what it
/// reverted with: the limit named is the one the order picks, `B`'s budget of none in the first
/// row, where the transaction's limit is crossed too. The row that crosses the frame's budget alone
/// shows the same budget rule with room left in the transaction.
///
/// Rules: [S10.48], [S10.31], [S10.33], [S10.34], [S10]. Independence: constants — the usages and
/// budgets are hand arithmetic from `TX_BODY_SIZE`, `WRITE_RECORD_SIZE` and the 98/100 share.
#[test]
fn test_a_record_crossing_both_counts_names_the_frame_budget() {
    let unlimited = EvmTxRuntimeLimits::no_limits();
    let before = TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE;
    let rows: [(&str, EvmTxRuntimeLimits, u64); 2] = [
        (
            "the transaction's limit and the frame's budget: the frame's budget",
            unlimited.with_tx_data_size_limit(before).with_frame_data_size_limit(120),
            0,
        ),
        ("the frame's budget alone", unlimited.with_frame_data_size_limit(120), 39),
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
    for (row, limits, named) in rows {
        let db = MemoryDatabase::default().account_code(A, a.clone()).account_code(B, b.clone());
        let outcome = execute_under(db, call(CALLER, A, U256::ZERO, GAS_LIMIT), limits);
        let revert_data: Bytes =
            MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: named }
                .abi_encode()
                .into();
        assert!(outcome.result.is_success(), "{row}: {:?}", outcome.result);
        assert_eq!(
            outcome.result.output(),
            Some(&revert_data),
            "{row}: B reverted alone, and A returned what it reverted with"
        );
        assert_eq!(outcome.limit_exceeded, None, "{row}: no stop of the transaction's own frame");
        assert_eq!(
            outcome.usage,
            LimitUsage { data_size: before, write_records: 2, state_growth: 2 },
            "{row}: A's two records and two new slots, none of B's"
        );
    }
}
