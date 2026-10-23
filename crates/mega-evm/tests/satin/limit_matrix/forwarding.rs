//! The gas cap and the reservoir share.
//!
//! A child frame is started with an explicit gas below the caller's cap, so the gas it holds at
//! `CALL` is the number the test names. The opcode's static warm read is not yet charged when the
//! inspector reads `remaining`.

use alloy_primitives::{Bytes, U256};
use mega_evm::{
    test_utils::{neutral_cfg, neutralize_evm, note_price_guard, BytecodeBuilder},
    EthSpecId, EvmTxRuntimeLimits, MegaEvm, MegaLimitExceeded,
};
use revm::bytecode::opcode::{
    CALL, CALLDATALOAD, GAS, MSTORE8, POP, PUSH0, SHR, SSTORE, STOP, TIMESTAMP,
};

use super::{
    at_spec_prices, call_tx, call_with_reservoir, execute, execute_forward, execute_forward_under,
    forward_to, gas_cap, returned_stop, slot_value, warm_call, world, CALL_STIPEND, CHILD,
    CONTRACT, EXECUTION_CAP, EXISTING, GRAND, HISTORY_ALLOWANCE_BYTES, HISTORY_ALLOWANCE_GAS,
    HISTORY_GAS_PER_BYTE, RESERVOIR_KEPT_PERCENT, SLOT_STATE_GAS_M1, VALUE_TRANSFER_GAS, WARM_READ,
};
use crate::common::{self, context};

/// Gas the non-value inner frame spends before `CALL`'s static warm read.
///
/// Five `PUSH0`, one `PUSH20` and `GAS`: `5 × 2 + 3 + 2`.
const NON_VALUE_PREFIX: u64 = 15;

/// Gas the value inner frame spends before `CALL`'s static warm read.
///
/// Four `PUSH0`, one `PUSH1`, one `PUSH20` and `GAS`: `4 × 2 + 3 + 3 + 2`.
const VALUE_PREFIX: u64 = 16;

/// Inner code that calls [`EXISTING`] with all the gas it has.
fn measuring_call(value: bool) -> Bytes {
    let mut code = BytecodeBuilder::default();
    if value {
        code = code.append_many([PUSH0, PUSH0, PUSH0, PUSH0]).push_number(1_u8);
    } else {
        code = code.append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]);
    }
    code.push_address(EXISTING).append_many([GAS, CALL, STOP]).build()
}

/// Outer code that gives [`CHILD`] exactly `frame_gas`, carrying no value, so no stipend is added
/// to it.
fn grant(frame_gas: u16) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CHILD)
        .push_number(frame_gas)
        .append_many([CALL, STOP])
        .build()
}

/// A warm call above the execution cap. The reservoir pays the history of the inner value call's
/// two records, which the caller could not pay from the 2% of its regular gas it keeps.
fn measuring_tx() -> super::MegaTransaction {
    warm_call(EXECUTION_CAP + common::value_call_room(), &[CHILD, EXISTING])
}

/// `(gas held at the inner CALL, gas the inner CALL forwarded)`.
fn measure(frame_gas: u16, value: bool) -> (u64, u64) {
    let db = world(&[
        (CONTRACT, grant(frame_gas)),
        (CHILD, measuring_call(value)),
        (EXISTING, Bytes::from_static(&[STOP])),
    ]);
    let (outcome, probe) = execute_forward(db, measuring_tx(), super::Forward::targeting(EXISTING));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let held = probe.held.unwrap_or_else(|| {
        panic!("the inner CALL was reached; calls {:?}", probe.calls);
    });
    let forwarded = probe.gas_limit.expect("the inner CALL forwarded gas");
    (held, forwarded)
}

/// What EIP-150's 63/64 rule would forward of `remaining`, for contrast.
const fn eip150(remaining: u64) -> u64 {
    remaining - remaining / 64
}

/// Regular gas left after the warm read, which is the cap's input.
fn left_after_warm(held: u64) -> u64 {
    held - WARM_READ
}

/// Parent reservoir `R = 50 × 97,920`. The child receives `R − R × 2 / 100`. The grandchild
/// receives that share of what the child has left.
#[test]
fn test_a_child_receives_the_reservoir_share() {
    const R: u64 = 50 * SLOT_STATE_GAS_M1;
    let db = world(&[
        (CONTRACT, forward_to(CHILD)),
        (CHILD, forward_to(GRAND)),
        (GRAND, Bytes::from_static(&[STOP])),
    ]);
    let (outcome, probe) = execute_forward(db, call_with_reservoir(R), super::Forward::any());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(probe.calls.len() >= 2, "child and grandchild: {:?}", probe.calls);
    assert_eq!(probe.reservoir_given(CONTRACT), R, "the transaction's own frame takes the whole");
    let child = probe.reservoir_given(CHILD);
    let grandchild = probe.reservoir_given(GRAND);
    assert_eq!(child, R - R * RESERVOIR_KEPT_PERCENT / 100, "child reservoir {child}");
    assert_eq!(
        grandchild,
        child - child * RESERVOIR_KEPT_PERCENT / 100,
        "grandchild reservoir {grandchild}"
    );
}

/// The child spends one fresh slot from its reservoir share and reverts. The caller had no
/// outstanding spill, so it holds the reservoir it had after the body's charge. The slot is not
/// kept and the state ledger is zero.
#[test]
fn test_a_reverted_child_returns_its_reservoir_share() {
    const R: u64 = 50 * SLOT_STATE_GAS_M1;
    if common::slot_state_gas() > R {
        note_price_guard("one fresh slot costs more than the reservoir this row forwards");
        return;
    }
    let mut child = super::store_n_prefix(1, 1, false);
    child.extend_from_slice(&[PUSH0, PUSH0, 0xfd]);
    let db = world(&[(CONTRACT, forward_to(CHILD)), (CHILD, Bytes::from(child))]);
    let outcome = execute(db, call_with_reservoir(R));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO);
    assert_eq!(outcome.gas.state, 0, "the child's state charge came back");
    assert_eq!(outcome.gas.reservoir_remaining, R);
}

/// The share a failed child returns when its caller has an outstanding spill.
///
/// The caller holds `R` and keeps `k = R × 2 / 100` when its first child takes the share. That
/// child fills 50 fresh slots, more than its share pays, and succeeds, so its spill joins the
/// caller's and the caller's kept `k` is not drawn. The second child is given `k − k × 2 / 100`
/// and reverts at once. The share it returns unwinds the caller's spill first, so the caller ends
/// with `k × 2 / 100`, not the `k` it held after the start's charges. At `m` = 1, `R` =
/// 4,896,000, `k` = 97,920 and the reservoir left is 1,958.
///
/// The caller is the transaction's own frame, so its reservoir is read where its frame ends:
/// the transaction then settles that frame as a caller settles a child, and the 1,958 unwinds
/// the frame's own outstanding spill there, coming back to the sender as regular gas.
#[test]
fn test_a_reverted_childs_share_unwinds_the_callers_spill_first() {
    if common::state_is_free() {
        return;
    }
    let r = 50 * common::slot_state_gas();
    let kept = r * RESERVOIR_KEPT_PERCENT / 100;
    let left = kept * RESERVOIR_KEPT_PERCENT / 100;
    if left == 0 {
        note_price_guard("the caller's second keep rounds to nothing");
        return;
    }
    let parent = BytecodeBuilder::default()
        .call(CHILD, U256::ZERO)
        .append(POP)
        .call(GRAND, U256::ZERO)
        .append(POP)
        .stop()
        .build();
    let db = world(&[
        (CONTRACT, parent),
        (CHILD, super::store_n_slots(50)),
        (GRAND, BytecodeBuilder::default().revert().build()),
    ]);
    let (outcome, probe) = execute_forward(db, call_with_reservoir(r), super::Forward::any());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(slot_value(&outcome, CHILD, 50), U256::from(1), "the first child succeeded");
    assert_eq!(probe.reservoir_given(GRAND), kept - left, "the second child's share");
    let ended = probe.call_ends.last().expect("the transaction's own frame ended");
    assert_eq!(ended.0, CONTRACT);
    assert_eq!(
        ended.1, left,
        "the returned share unwound the spill; k = {kept}, k × 2 / 100 = {left}"
    );
    assert_eq!(outcome.gas.reservoir_remaining, 0, "the transaction's settlement unwound the rest");
}

/// `CALL` with gas argument 1 still forwards the reservoir share, `R − R × 2 / 100` of the
/// reservoir `R` the caller holds. The gas argument bounds the regular gas only.
#[test]
fn test_a_one_gas_call_still_forwards_the_reservoir() {
    const R: u64 = SLOT_STATE_GAS_M1;
    let parent = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CHILD)
        .push_number(1_u8)
        .append_many([CALL, STOP])
        .build();
    let db = world(&[(CONTRACT, parent), (CHILD, super::store_n_slots(1))]);
    let (outcome, probe) =
        execute_forward(db, call_with_reservoir(R), super::Forward::targeting(CHILD));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(probe.gas_limit, Some(1), "the gas argument bounds regular gas");
    assert_eq!(probe.reservoir, Some(super::reservoir_share(R)), "the share, not 1 and not R");
}

/// With 1,000 regular gas left, the cap is 980. EIP-150 would forward 985.
#[test]
fn test_a_call_with_1000_gas_left_forwards_980() {
    let (held, forwarded) = measure((1_000 + NON_VALUE_PREFIX + WARM_READ) as u16, false);
    assert_eq!(left_after_warm(held), 1_000, "held {held}");
    assert_eq!(forwarded, gas_cap(1_000));
    assert_ne!(forwarded, eip150(1_000), "EIP-150's 985 is not the cap");
}

/// With 99 left, the subtraction forwards 98. The frame-budget product would forward 97.
/// EIP-150 also forwards 98, so a pass here does not decide the quotient.
#[test]
fn test_a_call_with_99_gas_left_forwards_98() {
    let (held, forwarded) = measure((99 + NON_VALUE_PREFIX + WARM_READ) as u16, false);
    assert_eq!(left_after_warm(held), 99, "held {held}");
    assert_eq!(forwarded, 99 - 99 * RESERVOIR_KEPT_PERCENT / 100);
    assert_ne!(forwarded, super::frame_share(99), "the product would forward 97");
}

/// With 50 left, the subtraction forwards 49. EIP-150 forwards 50, because `floor(50 / 64)` is 0.
#[test]
fn test_a_call_with_50_gas_left_forwards_49() {
    let (held, forwarded) = measure((50 + NON_VALUE_PREFIX + WARM_READ) as u16, false);
    assert_eq!(left_after_warm(held), 50, "held {held}");
    assert_eq!(forwarded, 49);
    assert_ne!(forwarded, eip150(50), "EIP-150 forwards the whole 50");
}

/// After the 11,300 value-transfer charge the caller has 10,000 left and requests all. The cap
/// on 10,000 is 9,800. The child receives 12,100. The caller keeps 200. The 2,300 stipend is
/// already inside the 11,300 and is added after the cap, not taken from the caller. The caller
/// is granted its gas without value, so its own gas has no stipend in it, and the transaction's
/// reservoir pays the history of the value call's records.
#[test]
fn test_a_value_call_with_10000_left_forwards_12100() {
    let frame = VALUE_PREFIX + WARM_READ + VALUE_TRANSFER_GAS + 10_000;
    let (held, forwarded) = measure(frame as u16, true);
    let after_value = held - WARM_READ - VALUE_TRANSFER_GAS;
    assert_eq!(after_value, 10_000, "held {held}");
    assert_eq!(forwarded, gas_cap(10_000) + CALL_STIPEND);
    assert_eq!(10_000 - gas_cap(10_000), 200, "the caller keeps 200 of the 10,000");
}

/// A value `CALL` below the top frame grants 160 history bytes (14,080 gas). The allowance is
/// outside the child's gas limit and outside the reservoir share: the child's gas is the 98/100
/// cap plus the 2,300 stipend, and its reservoir is the share of what the caller has left after
/// the history of the start's two records.
#[test]
fn test_a_value_call_grants_a_history_allowance_outside_gas() {
    if !at_spec_prices() || common::history_is_free() || common::history_rounds() {
        return;
    }
    const R: u64 = 50 * SLOT_STATE_GAS_M1;
    let parent = BytecodeBuilder::default().call(CHILD, U256::from(1)).stop().build();
    let db = world(&[
        (CONTRACT, parent),
        (CHILD, BytecodeBuilder::default().log3_word().stop().build()),
    ])
    .account_balance(CONTRACT, U256::from(10u128.pow(18)));
    // The access list adds one 20-byte address to the body.
    let gas = EXECUTION_CAP + common::history(super::BODY_BYTES + 20) + R;
    let (outcome, probe) =
        execute_forward(db, warm_call(gas, &[CHILD]), super::Forward::targeting(CHILD));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let gap = outcome.gas.history_bytes * HISTORY_GAS_PER_BYTE - outcome.gas.history;
    assert_eq!(
        gap,
        HISTORY_ALLOWANCE_BYTES * HISTORY_GAS_PER_BYTE,
        "history bytes {} gas {}",
        outcome.gas.history_bytes,
        outcome.gas.history
    );
    assert_eq!(gap, HISTORY_ALLOWANCE_GAS);
    let held = probe.held.expect("the value call was reached");
    let forwarded = probe.gas_limit.expect("the value call forwarded gas");
    let left = held - WARM_READ - VALUE_TRANSFER_GAS;
    assert_eq!(forwarded, gas_cap(left) + CALL_STIPEND, "the cap plus the stipend, no allowance");
    // The caller's and the child's balances change: two records, paid from the reservoir first.
    let after_records = R - 2 * super::RECORD_HISTORY;
    assert_eq!(probe.reservoir, Some(super::reservoir_share(after_records)));
}

/// Reservoir remaining 1 gives the child 1 and the caller 0. A frame whose data-size remaining is
/// 1 gives its child `floor(1 × 98 / 100)` = 0, so one 40-byte record stops that child, kind 0,
/// `limit` 0.
///
/// A transaction data-size limit of the body plus 2,002 gives the top frame 2,002 and its child
/// `floor(2,002 × 98 / 100)` = 1,961. That child keeps 49 records, 1,960 bytes, and has 1 left.
/// The transaction still has 42, so the grandchild's record crosses its own budget alone.
#[test]
fn test_a_reservoir_of_1_and_a_frame_budget_of_1() {
    const TOP: u64 = 2_002;
    assert_eq!(super::frame_share(TOP), 1_961);
    assert_eq!(super::frame_share(TOP) - 49 * super::WRITE_RECORD_SIZE, 1);
    let mut child = super::store_n_prefix(49, 1, false);
    child.extend_from_slice(
        &BytecodeBuilder::default().call(GRAND, U256::ZERO).return_returndata().build(),
    );
    let db = world(&[
        (CONTRACT, forward_to(CHILD)),
        (CHILD, Bytes::from(child)),
        (GRAND, super::store_n_slots(1)),
    ]);
    let limits = EvmTxRuntimeLimits::default().with_tx_data_size_limit(super::BODY_BYTES + TOP);
    let (outcome, probe) =
        execute_forward_under(db, call_with_reservoir(1), limits, super::Forward::targeting(CHILD));
    assert_eq!(probe.reservoir, Some(1), "a remaining 1 forwards the whole reservoir");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(outcome.limit_exceeded.is_none(), "the child caught the stop");
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 0, limit: 0 });
    assert_eq!(slot_value(&outcome, GRAND, 1), U256::ZERO);
    assert_eq!(slot_value(&outcome, CHILD, 49), U256::from(1), "the child kept its records");
}

/// A detained frame forwards with the 98/100 cap computed on spendable plus withheld. The child
/// then makes a regular charge past the compute cap, and that charge stops it, kind 2.
#[test]
fn test_a_detained_frame_forwards_on_spendable_plus_withheld() {
    const CAP: u64 = 50_000;
    let parent = BytecodeBuilder::default()
        .append_many([TIMESTAMP, POP])
        .call(CHILD, U256::ZERO)
        .return_returndata()
        .build();
    // One byte at offset 200_000 expands 6,251 words: about 95,000 gas, past the cap.
    let child = BytecodeBuilder::default()
        .push_number(1_u8)
        .push_number(200_000_u32)
        .append_many([MSTORE8, STOP])
        .build();
    let db = world(&[(CONTRACT, parent), (CHILD, child)]);
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(CAP);
    let (outcome, probe) = execute_forward_under(
        db,
        warm_call(5_000_000, &[CHILD]),
        limits,
        super::Forward::targeting(CHILD),
    );
    let held = probe.held.expect("the call after TIMESTAMP was reached");
    let forwarded = probe.gas_limit.expect("the call forwarded gas");
    let left = held - WARM_READ;
    assert!(left > CAP, "remaining() includes the withheld part: left {left}, cap {CAP}");
    assert_eq!(forwarded, gas_cap(left), "held {held}, forwarded {forwarded}");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome).kind, 2);
}

/// The equivalence-mode contrast to Satin's forward, of which the page pins no figure.
///
/// Neutral Amsterdam forwards EIP-150's 985 and passes each child the whole reservoir its caller
/// holds. `no_limits()` on Satin does not change the forward: 980 of regular gas, and at each of
/// the two calls the reservoir share, so the measured grandchild call gets the share of the share.
#[test]
fn test_neutral_forwards_eip150_and_satin_keeps_its_cap_without_limits() {
    const R: u64 = 50 * SLOT_STATE_GAS_M1;
    let frame = (1_000 + NON_VALUE_PREFIX + WARM_READ) as u16;
    let code = [
        (CONTRACT, grant(frame)),
        (CHILD, measuring_call(false)),
        (EXISTING, Bytes::from_static(&[STOP])),
    ];
    // The access list adds two 20-byte addresses to the body.
    let gas = EXECUTION_CAP + common::history(super::BODY_BYTES + 40) + R;
    let tx = warm_call(gas, &[CHILD, EXISTING]);

    let cfg = neutral_cfg(EthSpecId::AMSTERDAM).expect("Amsterdam has a neutral configuration");
    let mut neutral = MegaEvm::new(context(world(&code)).with_neutral_cfg(cfg))
        .with_inspector(super::Forward::targeting(EXISTING));
    neutralize_evm(&mut neutral, EthSpecId::AMSTERDAM).expect("Amsterdam neutralizes");
    let neutral_outcome =
        neutral.execute_transaction(tx.clone()).expect("the transaction is valid");
    let neutral_probe = neutral.inspector().clone();
    assert!(neutral_outcome.result.is_success(), "{:?}", neutral_outcome.result);
    let neutral_held = neutral_probe.held.expect("neutral reached the call");
    let neutral_forwarded = neutral_probe.gas_limit.expect("neutral forwarded");
    assert_eq!(neutral_held - WARM_READ, 1_000);
    assert_eq!(neutral_forwarded, eip150(1_000), "EIP-150 forwards 985");
    assert_eq!(
        neutral_probe.calls[1].2, neutral_probe.calls[0].2,
        "neutral passes the whole reservoir: {:?}",
        neutral_probe.calls
    );

    let limits = EvmTxRuntimeLimits::no_limits();
    let (outcome, probe) =
        execute_forward_under(world(&code), tx, limits, super::Forward::targeting(EXISTING));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let held = probe.held.expect("satin reached the call");
    let forwarded = probe.gas_limit.expect("satin forwarded");
    assert_eq!(held - WARM_READ, 1_000);
    assert_eq!(forwarded, gas_cap(1_000));
    let child = R - R * RESERVOIR_KEPT_PERCENT / 100;
    assert_eq!(probe.reservoir_given(CONTRACT), R, "the transaction's own frame takes the whole");
    assert_eq!(probe.reservoir_given(CHILD), child, "the child's share");
    assert_eq!(probe.reservoir, Some(child - child * RESERVOIR_KEPT_PERCENT / 100));
}

/// Below the execution cap the reservoir starts at 0. The callee fills a fresh slot, so 97,920 of
/// state gas spills onto its regular gas and, on return, onto the caller. The callee then clears
/// the slot. The refill lands in the callee's reservoir and the returned reservoir unwinds the
/// caller's spill, so the caller's reservoir stays 0. A settlement that left the 97,920 in the
/// reservoir would report at least that much still remaining.
#[test]
fn test_a_returned_reservoir_unwinds_the_callers_spill() {
    if !at_spec_prices() || common::state_is_free() {
        return;
    }
    // Calldata byte 0 selects the value stored into slot 1.
    let child = BytecodeBuilder::default()
        .append(PUSH0)
        .append(CALLDATALOAD)
        .push_number(248_u8)
        .append(SHR)
        .push_number(1_u8)
        .append(SSTORE)
        .stop()
        .build();
    let call_one_byte = |code: BytecodeBuilder| {
        code.append_many([PUSH0, PUSH0])
            .push_number(1_u8)
            .append_many([PUSH0, PUSH0])
            .push_address(CHILD)
            .append_many([GAS, CALL])
    };
    let parent = call_one_byte(
        call_one_byte(BytecodeBuilder::default().mstore(0, [1_u8])).append(POP).mstore(0, [0_u8]),
    )
    .stop()
    .build();
    let db = world(&[(CONTRACT, parent), (CHILD, child)]);
    let outcome = execute(db, call_tx(10_000_000));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO);
    assert_eq!(outcome.gas.state, 0, "the clear refilled the fresh-slot charge");
    assert_eq!(
        outcome.gas.reservoir_remaining, 0,
        "the returned refill unwinds the caller's spill instead of staying in the reservoir"
    );
}
