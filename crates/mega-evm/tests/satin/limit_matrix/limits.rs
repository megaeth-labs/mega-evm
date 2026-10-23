//! Frame budgets and the transaction limits.
//!
//! Expected stops use the page's kind and `limit`. Gas room follows the schedule at the prices
//! in effect, so a measurement build can still reach the stop.

use alloy_primitives::{keccak256, Bytes, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IOracle, ORACLE_CONTRACT_ADDRESS},
    test_utils::BytecodeBuilder,
    EvmTxRuntimeLimits, MegaLimitExceeded,
};
use revm::bytecode::opcode::{CALL, GAS, LOG0, POP, PUSH0};

use super::{
    balance_of, call_tx, delegatecall, execute, execute_under, forward_to, frame_share,
    gas_for_slots, keep_returndata, nonce_of, return_kept, returned_stop, returned_stops,
    reverted_stop, slot_value, slots_gas, store_n_prefix, store_n_prefix_at, store_n_slots, world,
    BENEFICIARY, BODY_BYTES, CHILD, CONTRACT, GRAND, NEXT, TRANSFER_LOG_SIZE, TX_DATA_LIMIT,
    TX_KV_UPDATE_LIMIT, TX_STATE_GROWTH_LIMIT, WRITE_RECORD_SIZE,
};

fn join(prefix: Vec<u8>, rest: BytecodeBuilder) -> Bytes {
    let mut code = prefix;
    code.extend_from_slice(&rest.build());
    Bytes::from(code)
}

/// The transaction's own frame has a state-growth budget of 1,000. The 1,001st fresh slot stops
/// that frame, kind 3, `limit` 1,000. The slot is not kept. The receipt fails and the transaction
/// is included.
#[test]
fn test_the_1001st_fresh_slot_stops_the_transaction() {
    let Some(gas) = slots_gas(1_001) else { return };
    let db = world(&[(CONTRACT, store_n_slots(1_001))]);
    let outcome = execute(db, call_tx(gas));
    let stop = reverted_stop(&outcome);
    assert_eq!(stop, MegaLimitExceeded { kind: 3, limit: TX_STATE_GROWTH_LIMIT });
    assert!(outcome.limit_exceeded.is_some(), "the transaction's own frame stopped");
    assert_eq!(slot_value(&outcome, CONTRACT, 1), U256::ZERO);
    assert_eq!(slot_value(&outcome, CONTRACT, 1_001), U256::ZERO);
}

/// The caller has kept 20 of a 1,000 budget. The child budget is `floor(980 × 98 / 100)` = 960.
/// Those 960 slots succeed. The caller net becomes 980 and its remaining becomes 20, so a later
/// child receives `floor(20 × 98 / 100)` = 19.
#[test]
fn test_a_child_budget_of_960_then_a_later_child_of_19() {
    let Some(gas) = slots_gas(1_000) else { return };
    let parent = join(
        store_n_prefix(20, 1, false),
        BytecodeBuilder::default()
            .call(CHILD, U256::ZERO)
            .append(POP)
            .call(NEXT, U256::ZERO)
            .return_returndata(),
    );
    let db = world(&[(CONTRACT, parent), (CHILD, store_n_slots(960)), (NEXT, store_n_slots(20))]);
    let outcome = execute(db, call_tx(gas));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(outcome.limit_exceeded.is_none(), "the caller caught the later child's stop");
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 3, limit: 19 });
    assert_eq!(slot_value(&outcome, CONTRACT, 20), U256::from(1));
    assert_eq!(slot_value(&outcome, CHILD, 960), U256::from(1));
    assert_eq!(
        slot_value(&outcome, NEXT, 1),
        U256::ZERO,
        "the later child's 20 slots are discarded"
    );
}

/// The same caller. The child's 961st slot stops the child, kind 3, `limit` 960. The child's net
/// is discarded, so the caller net stays 20 and its remaining stays 980: a later child receives
/// `floor(980 × 98 / 100)` = 960 again, and its own 961st slot stops it at 960.
#[test]
fn test_a_childs_961st_slot_stops_at_960() {
    let Some(gas) = slots_gas(2_000) else { return };
    let rest = BytecodeBuilder::default().call(CHILD, U256::ZERO).append(POP);
    let rest = keep_returndata(rest, 0x100).call(NEXT, U256::ZERO).append(POP);
    let parent = join(store_n_prefix(20, 1, false), return_kept(keep_returndata(rest, 0x180)));
    let db = world(&[(CONTRACT, parent), (CHILD, store_n_slots(961)), (NEXT, store_n_slots(961))]);
    let outcome = execute(db, call_tx(gas));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let (child, next) = returned_stops(&outcome);
    assert_eq!(child, MegaLimitExceeded { kind: 3, limit: 960 });
    assert_eq!(next, MegaLimitExceeded { kind: 3, limit: 960 }, "the caller still had 980 left");
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO, "the child's net is discarded");
    assert_eq!(slot_value(&outcome, NEXT, 1), U256::ZERO);
    assert_eq!(slot_value(&outcome, CONTRACT, 20), U256::from(1));
}

/// Transaction KV limit 10, caller has used 2. The child budget is `floor(8 × 98 / 100)` = 7.
/// The 8th new record stops the child, kind 1, `limit` 7. On that stop the caller still has 8
/// left, and its own 8 further records fit. At the protocol value 500,000 this shape hits data
/// size first ([`test_kv_weighs_more_than_the_data_size_cap`]).
#[test]
fn test_a_child_kv_budget_of_7_stops_the_eighth_record() {
    assert_eq!(frame_share(8), 7);
    let Some(gas) = slots_gas(20) else { return };
    let mut parent = store_n_prefix(2, 1, false);
    parent
        .extend_from_slice(&BytecodeBuilder::default().call(CHILD, U256::ZERO).append(POP).build());
    // Slots 10 down to 1: slots 1 and 2 are already set, so this adds 8 records.
    parent.extend_from_slice(&store_n_prefix_at(parent.len(), 10, 1, false));
    // A store leaves the return data alone: the parent returns the child's stop.
    parent.extend_from_slice(&BytecodeBuilder::default().return_returndata().build());
    let db = world(&[(CONTRACT, Bytes::from(parent)), (CHILD, store_n_slots(8))]);
    let limits = EvmTxRuntimeLimits::default().with_tx_kv_update_limit(10);
    let outcome = execute_under(db, call_tx(gas), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 7 });
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO);
    assert_eq!(slot_value(&outcome, CONTRACT, 10), U256::from(1), "the caller had 8 left");
    assert_eq!(outcome.usage.write_records, 10);
}

/// Not a protocol-gas execution. 500,000 records weigh 20,000,000 bytes, above `TX_DATA_LIMIT`
/// 13,107,200. After a 310-byte body the top frame's data-size budget is 13,106,890. 327,672
/// records fit. A 327,673rd record would cross that frame budget, kind 0, `limit` 13,106,890.
/// The KV check itself is pinned under reduced KV limits
/// ([`test_a_child_kv_budget_of_7_stops_the_eighth_record`],
/// [`test_the_transactions_own_frame_stops_at_a_reduced_kv_limit`]).
#[test]
fn test_kv_weighs_more_than_the_data_size_cap() {
    let weight = TX_KV_UPDATE_LIMIT * WRITE_RECORD_SIZE;
    assert_eq!(weight, 20_000_000);
    assert!(weight > TX_DATA_LIMIT);
    let budget = TX_DATA_LIMIT - BODY_BYTES;
    assert_eq!(budget, 13_106_890);
    assert_eq!(327_672 * WRITE_RECORD_SIZE, 13_106_880);
    assert!(327_672 * WRITE_RECORD_SIZE <= budget);
    assert_eq!(327_673 * WRITE_RECORD_SIZE, 13_106_920);
    assert!(327_673 * WRITE_RECORD_SIZE > budget);
    let _ = MegaLimitExceeded { kind: 0, limit: budget };
}

/// A reduced KV transaction limit of 10, with nothing counted before the frame, so the
/// transaction's own frame has a KV budget of 10. Its 11th record stops it, kind 1, `limit` 10.
/// The data size does not bind.
#[test]
fn test_the_transactions_own_frame_stops_at_a_reduced_kv_limit() {
    let Some(gas) = slots_gas(11) else { return };
    let db = world(&[(CONTRACT, store_n_slots(11))]);
    let limits = EvmTxRuntimeLimits::default().with_tx_kv_update_limit(10);
    let outcome = execute_under(db, call_tx(gas), limits);
    assert_eq!(reverted_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 10 });
    assert!(outcome.limit_exceeded.is_some());
    assert_eq!(slot_value(&outcome, CONTRACT, 11), U256::ZERO);
}

/// One fresh `SSTORE` whose record crosses the data-size frame budget and the state-growth frame
/// budget names data size, kind 0, and `limit` is the data-size frame budget.
///
/// A transaction data-size limit of the body plus 40,000 gives the top frame 40,000 bytes and a
/// child `floor(40,000 × 98 / 100)` = 39,200, 980 records. The child's state-growth budget is 980
/// too. Its 981st slot crosses both, and not the transaction limit. When the top frame itself
/// makes the 1,001st record, that record crosses the data-size frame budget, 40,000, the
/// data-size transaction limit and the state-growth budget, 1,000, and the stop still names the
/// frame budget, 40,000.
#[test]
fn test_a_record_that_crosses_data_size_and_state_growth_names_data_size() {
    let Some(gas) = slots_gas(1_001) else { return };
    let budget = frame_share(40_000);
    assert_eq!(budget, 39_200);
    assert!(981 * WRITE_RECORD_SIZE > budget);
    assert!(980 * WRITE_RECORD_SIZE <= budget);
    assert_eq!(frame_share(TX_STATE_GROWTH_LIMIT), 980);
    let limits = EvmTxRuntimeLimits::default().with_tx_data_size_limit(BODY_BYTES + 40_000);
    let db = world(&[(CONTRACT, forward_to(CHILD)), (CHILD, store_n_slots(981))]);
    let outcome = execute_under(db, call_tx(gas), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(outcome.limit_exceeded.is_none(), "the caller caught the child's stop");
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 0, limit: budget });
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO);

    let db = world(&[(CONTRACT, store_n_slots(1_001))]);
    let outcome = execute_under(db, call_tx(gas), limits);
    assert_eq!(
        reverted_stop(&outcome),
        MegaLimitExceeded { kind: 0, limit: 40_000 },
        "the frame budget, not the transaction limit {}",
        BODY_BYTES + 40_000
    );
}

/// State growth at depth 2 and 3, top remaining 1,000. Child 980, grandchild 960, the next frame
/// `floor(960 × 98 / 100)` = 940. The 941st slot there stops that frame, kind 3, `limit` 940.
#[test]
fn test_state_growth_at_depth_2_and_3_stops_at_940() {
    let Some(gas) = slots_gas(941) else { return };
    assert_eq!(frame_share(1_000), 980);
    assert_eq!(frame_share(980), 960);
    assert_eq!(frame_share(960), 940);
    let db = world(&[
        (CONTRACT, forward_to(CHILD)),
        (CHILD, forward_to(GRAND)),
        (GRAND, forward_to(NEXT)),
        (NEXT, store_n_slots(941)),
    ]);
    let outcome = execute(db, call_tx(gas));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 3, limit: 940 });
    assert_eq!(slot_value(&outcome, NEXT, 1), U256::ZERO);
}

/// Parent budget 1,000, net 100. The child, a `DELEGATECALL` in the parent's storage, clears
/// those 100: its net is −100 and its remaining stays 882, so the frame it starts receives 864,
/// and that frame's 865th slot stops it at 864. When the child succeeds, the parent's net is 0
/// and its remaining is 1,000 again, so a later child receives 980. When the child reverts, the
/// parent's net stays 100 and its remaining 900, so a later child receives 882.
#[test]
fn test_a_cleared_net_does_not_raise_the_childs_remaining() {
    let Some(gas) = slots_gas(2_000) else { return };
    assert_eq!(frame_share(900), 882);
    assert_eq!(frame_share(882), 864);
    let parent = {
        let rest = keep_returndata(delegatecall(CHILD).append(POP), 0x100);
        let rest = keep_returndata(rest.call(NEXT, U256::ZERO).append(POP), 0x180);
        join(store_n_prefix(100, 1, false), return_kept(rest))
    };
    for (kept, next) in [(true, 980), (false, 882)] {
        let tail = BytecodeBuilder::default().call(GRAND, U256::ZERO);
        let tail = if kept { tail.return_returndata() } else { tail.revert_with_returndata() };
        let child = join(store_n_prefix(100, 0, false), tail);
        let db = world(&[
            (CONTRACT, parent.clone()),
            (CHILD, child),
            (GRAND, store_n_slots(865)),
            (NEXT, store_n_slots(TX_STATE_GROWTH_LIMIT as u16)),
        ]);
        let outcome = execute(db, call_tx(gas));
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        let (grand, later) = returned_stops(&outcome);
        assert_eq!(grand, MegaLimitExceeded { kind: 3, limit: 864 }, "kept {kept}");
        assert_eq!(later, MegaLimitExceeded { kind: 3, limit: next }, "kept {kept}");
        assert_eq!(slot_value(&outcome, GRAND, 1), U256::ZERO);
        let slot = if kept { U256::ZERO } else { U256::from(1) };
        assert_eq!(slot_value(&outcome, CONTRACT, 1), slot, "kept {kept}");
    }
}

/// Plants `sendHint`'s selector at memory 0 and calls the Oracle with `len` bytes of calldata.
fn hint_call(len: u32) -> BytecodeBuilder {
    BytecodeBuilder::default()
        .mstore(0, IOracle::sendHintCall::SELECTOR)
        .append_many([
            PUSH0, // retSize
            PUSH0, // retOffset
        ])
        .push_number(len)
        .append_many([PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append_many([GAS, CALL])
}

/// Body 310 plus twelve admitted hints of 1,048,576 bytes is 12,583,222. A child `LOG0` of
/// 524,288 data bytes is 524,320 bytes and crosses the transaction limit. The child's frame
/// budget is not crossed. Kind 0, `limit` 13,107,200. The hints stay counted. The log does not.
#[test]
fn test_hints_then_a_child_log_names_the_transaction_data_limit() {
    const HINT: u32 = 1_048_576;
    const LOG_DATA: u64 = 524_288;
    const BEFORE: u64 = BODY_BYTES + 12 * HINT as u64;
    assert_eq!(BEFORE, 12_583_222);
    assert_eq!(32 + LOG_DATA, 524_320);
    const { assert!(BEFORE + 32 + LOG_DATA > TX_DATA_LIMIT) };
    let mut code = Vec::new();
    for _ in 0..12 {
        code.extend_from_slice(&hint_call(HINT).build());
        code.push(POP);
    }
    code.extend_from_slice(
        &BytecodeBuilder::default().call(CHILD, U256::ZERO).return_returndata().build(),
    );
    let child = BytecodeBuilder::default()
        .push_number(LOG_DATA as u32)
        .append_many([PUSH0, LOG0])
        .stop()
        .build();
    let db = world(&[
        (CONTRACT, Bytes::from(code)),
        (CHILD, child),
        (ORACLE_CONTRACT_ADDRESS, Bytes::from_static(&[0x00])),
    ]);
    let outcome = execute(db, call_tx(40_000_000));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(outcome.limit_exceeded.is_none(), "the caller caught the log's stop");
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 0, limit: TX_DATA_LIMIT });
    assert_eq!(outcome.usage.data_size, BEFORE);
}

/// The same log in the transaction's own frame stops that frame, kind 0, `limit` 13,107,200.
/// Failed receipt, included. The admitted hint bytes stay counted.
#[test]
fn test_hints_then_a_top_frame_log_stops_the_transaction() {
    const HINT: u32 = 1_048_576;
    const LOG_DATA: u64 = 524_288;
    const BEFORE: u64 = BODY_BYTES + 12 * HINT as u64;
    let mut code = Vec::new();
    for _ in 0..12 {
        code.extend_from_slice(&hint_call(HINT).build());
        code.push(POP);
    }
    code.extend_from_slice(
        &BytecodeBuilder::default().push_number(LOG_DATA as u32).append_many([PUSH0, LOG0]).build(),
    );
    let db = world(&[
        (CONTRACT, Bytes::from(code)),
        (ORACLE_CONTRACT_ADDRESS, Bytes::from_static(&[0x00])),
    ]);
    let outcome = execute(db, call_tx(40_000_000));
    assert_eq!(reverted_stop(&outcome), MegaLimitExceeded { kind: 0, limit: TX_DATA_LIMIT });
    assert!(outcome.limit_exceeded.is_some(), "the transaction's own frame stopped");
    assert_eq!(outcome.usage.data_size, BEFORE);
}

/// One admitted hint whose calldata is exactly 100 bytes: the selector and 96 zero bytes.
fn hint_100() -> BytecodeBuilder {
    hint_call(100)
}

/// Body 310, one admitted hint of 100 bytes, a top-frame record of 40, and a successful child
/// whose net is −40. The transaction's net is 410. The same child reverting keeps the 40, so the
/// net is 450.
#[test]
fn test_a_successful_childs_negative_net_leaves_the_transaction_at_410() {
    let success_child = join(store_n_prefix(1, 0, false), BytecodeBuilder::default().stop());
    let db = world(&[
        (CONTRACT, hint_and_store()),
        (CHILD, success_child),
        (ORACLE_CONTRACT_ADDRESS, Bytes::from_static(&[0x00])),
    ]);
    let outcome = execute(db, call_tx(5_000_000));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 100);

    let failing = join(store_n_prefix(1, 0, false), BytecodeBuilder::default().revert());
    let db = world(&[
        (CONTRACT, hint_and_store()),
        (CHILD, failing),
        (ORACLE_CONTRACT_ADDRESS, Bytes::from_static(&[0x00])),
    ]);
    let outcome = execute(db, call_tx(5_000_000));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 100 + WRITE_RECORD_SIZE);
}

fn hint_and_store() -> Bytes {
    let mut code = hint_100().append(POP).sstore(U256::from(1), U256::from(1)).build().to_vec();
    code.extend_from_slice(&delegatecall(CHILD).stop().build());
    Bytes::from(code)
}

/// A transaction data-size limit of the body plus 81 gives the top frame 81 bytes. The parent
/// writes one 40-byte record, so 41 remain and the child receives 40. The child returns that record
/// and its remaining stays 40. The frame it starts receives `floor(40 × 98 / 100)` = 39, and one
/// 40-byte record there stops it, kind 0, `limit` 39.
#[test]
fn test_a_negative_data_size_net_does_not_raise_what_a_frame_forwards() {
    assert_eq!(frame_share(41), 40);
    assert_eq!(frame_share(40), 39);
    let child = join(
        store_n_prefix(1, 0, false),
        BytecodeBuilder::default().call(GRAND, U256::ZERO).return_returndata(),
    );
    let parent = join(
        BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).build().to_vec(),
        delegatecall(CHILD).return_returndata(),
    );
    let db = world(&[
        (CONTRACT, parent),
        (CHILD, child),
        (GRAND, BytecodeBuilder::default().sstore(U256::from(2), U256::from(1)).stop().build()),
    ]);
    let limits = EvmTxRuntimeLimits::default().with_tx_data_size_limit(BODY_BYTES + 81);
    let outcome = execute_under(db, call_tx(5_000_000), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 0, limit: 39 });
}

/// A pre-funded address, balance, nonce 0, no code, is created onto and destroyed. The leaf was
/// present. No refill and no fall, and the net stays 0. 1,000 further slot rises fit. The 1,001st
/// stops the frame, kind 3, `limit` 1,000.
#[test]
fn test_a_prefunded_creation_keeps_a_zero_net_until_the_1001st_slot() {
    let init = BytecodeBuilder::default().selfdestruct(BENEFICIARY).build();
    let created = CONTRACT.create2(B256::ZERO, keccak256(init.as_ref()));
    let only =
        BytecodeBuilder::default().create2(U256::ZERO, init.clone(), U256::ZERO).stop().build();
    let mut db = world(&[(CONTRACT, only), (BENEFICIARY, Bytes::from_static(&[0x00]))]);
    db = db.account_balance(created, U256::from(1));
    let Some(gas) = gas_for_slots(1) else { return };
    let settled = execute(db, call_tx(gas));
    assert!(settled.result.is_success(), "{:?}", settled.result);
    assert_eq!(settled.gas.state, 0, "a present leaf is not charged and not refilled");
    assert!(nonce_of(&settled, CONTRACT) >= 1, "the creator nonce bump stands");
    // The creator's nonce, the created account's nonce, and the beneficiary's balance: the
    // created account's record stands. One transfer log for the balance the destruction moved.
    assert_eq!(settled.usage.write_records, 3, "the nonce bump's record stands");
    assert_eq!(settled.usage.data_size, BODY_BYTES + 3 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE);
    assert_eq!(balance_of(&settled, created), U256::ZERO);

    // The loop's jumps are absolute. It has to start at its own pc, after the `CREATE2`.
    let prefix =
        BytecodeBuilder::default().create2(U256::ZERO, init, U256::ZERO).append(POP).build();
    let mut code = prefix.to_vec();
    code.extend_from_slice(&store_n_prefix_at(prefix.len(), 1_001, 1, true));
    let mut db =
        world(&[(CONTRACT, Bytes::from(code)), (BENEFICIARY, Bytes::from_static(&[0x00]))]);
    db = db.account_balance(created, U256::from(1));
    let Some(gas) = slots_gas(1_001) else { return };
    let outcome = execute(db, call_tx(gas));
    assert_eq!(
        reverted_stop(&outcome),
        MegaLimitExceeded { kind: 3, limit: TX_STATE_GROWTH_LIMIT }
    );
    assert_eq!(slot_value(&outcome, CONTRACT, 1), U256::ZERO);
}
