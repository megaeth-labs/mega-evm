//! Which dimension a crossing names, and a caller that catches it.
//!
//! Every kind is observed through the revert data.

use alloy_primitives::{Bytes, U256};
use mega_evm::{test_utils::BytecodeBuilder, EvmTxRuntimeLimits, MegaLimitExceeded};
use revm::{
    bytecode::opcode::{POP, TIMESTAMP},
    context::result::ExecutionResult,
};

use super::{
    call_tx, execute_under, returned_stop, slot_value, slots_gas, store_n_prefix, world, CHILD,
    CONTRACT, TX_STATE_GROWTH_LIMIT,
};

fn extend(prefix: Vec<u8>, rest: BytecodeBuilder) -> Bytes {
    let mut code = prefix;
    code.extend_from_slice(&rest.build());
    Bytes::from(code)
}

/// Under a transaction KV limit of 10, an applied authorization is one record before the first
/// frame, on no frame, so the transaction's own frame has a KV budget of 9. That frame's 10th
/// record crosses its budget and the transaction limit. Kind 1. `limit` is the frame budget, 9.
#[test]
fn test_a_record_that_crosses_kv_names_the_frame_budget() {
    let Some(gas) = slots_gas(12) else { return };
    let authority = alloy_primitives::address!("0000000000000000000000000000000000ab1018");
    let db = world(&[(CONTRACT, super::store_n_slots(10))]);
    let tx = crate::common::authorizing_call(
        super::CALLER,
        CONTRACT,
        U256::ZERO,
        gas,
        CHILD,
        &[(authority, 0)],
    );
    let limits = EvmTxRuntimeLimits::default().with_tx_kv_update_limit(10);
    let outcome = execute_under(db, tx, limits);
    assert_eq!(super::reverted_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 9 });
    assert_eq!(super::nonce_of(&outcome, authority), 1, "the admitted authorization stays");
}

/// A fresh `SSTORE` whose regular charge crosses detention, and whose completed write would have
/// crossed the state-growth frame budget. The charge is made before the write is counted. Kind
/// 2. The crossing slot is not kept. The parent's earlier slots stay: the stop is the child's.
#[test]
fn test_a_charge_that_crosses_detention_stops_before_the_write_is_counted() {
    let child = BytecodeBuilder::default()
        .append(TIMESTAMP)
        .append(POP)
        .sstore(U256::from(1), U256::from(1))
        .stop()
        .build();
    let parent = extend(
        store_n_prefix(TX_STATE_GROWTH_LIMIT as u16, 1, false),
        BytecodeBuilder::default().call(CHILD, U256::ZERO).return_returndata(),
    );
    let db = world(&[(CONTRACT, parent), (CHILD, child)]);
    let Some(gas) = slots_gas(TX_STATE_GROWTH_LIMIT + 1) else { return };
    // 50 leaves the opcodes between the read and `SSTORE` inside the cap, and `SSTORE`'s charge
    // past it. Zero would be a real cap the read itself could not move past.
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(50);
    let outcome = execute_under(db, call_tx(gas), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let stop = returned_stop(&outcome);
    assert_eq!(stop.kind, 2, "the regular charge stops the opcode before the write is counted");
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO, "the crossing slot is not kept");
    assert_eq!(
        slot_value(&outcome, CONTRACT, 1),
        U256::from(1),
        "the parent frame is outside the stop"
    );
}

/// KV at depth 2 and 3 under a transaction limit of 100: the child's budget is 98, the
/// grandchild's 96 and the next frame's `floor(96 × 98 / 100)` = 94. The 95th new record there
/// stops that frame, kind 1, `limit` 94.
#[test]
fn test_kv_at_depth_2_and_3_stops_at_94() {
    assert_eq!(super::frame_share(100), 98);
    assert_eq!(super::frame_share(98), 96);
    assert_eq!(super::frame_share(96), 94);
    let Some(gas) = slots_gas(95) else { return };
    let db = world(&[
        (CONTRACT, super::forward_to(CHILD)),
        (CHILD, super::forward_to(super::GRAND)),
        (super::GRAND, super::forward_to(super::NEXT)),
        (super::NEXT, super::store_n_slots(95)),
    ]);
    let limits = EvmTxRuntimeLimits::default().with_tx_kv_update_limit(100);
    let outcome = execute_under(db, call_tx(gas), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 94 });
    assert_eq!(slot_value(&outcome, super::NEXT, 1), U256::ZERO);
}

/// The caller catches a kind-3 stop, continues, and is stopped in turn. The child's stop is not
/// the transaction's result. `limit` is the caller's frame budget. The top frame catches that
/// stop in turn and the transaction succeeds.
///
/// The same shape for kind 1, under a transaction KV limit of 100.
#[test]
fn test_a_caller_that_catches_a_stop_is_stopped_by_its_own_next_record() {
    let child = super::store_n_slots(1);
    let mid = extend(
        store_n_prefix(980, 1, false),
        BytecodeBuilder::default()
            .call(CHILD, U256::ZERO)
            .append(POP)
            .sstore(U256::from(981), U256::from(1)),
    );
    let parent =
        BytecodeBuilder::default().call(super::GRAND, U256::ZERO).return_returndata().build();
    // GRAND is the frame whose budget is 980. CHILD is the frame whose budget is 0.
    let db = world(&[(CONTRACT, parent.clone()), (super::GRAND, mid), (CHILD, child)]);
    let Some(gas) = slots_gas(1_000) else { return };
    let outcome = execute_under(db, call_tx(gas), EvmTxRuntimeLimits::default());
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(outcome.limit_exceeded.is_none(), "the top frame caught the stop");
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 3, limit: 980 });
    assert_eq!(
        slot_value(&outcome, super::GRAND, 1),
        U256::ZERO,
        "the stopped caller keeps nothing"
    );
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO);

    {
        // The caller's KV budget is `floor(100 × 98 / 100)` = 98, its child's 0.
        let mid = extend(
            store_n_prefix(98, 1, false),
            BytecodeBuilder::default()
                .call(CHILD, U256::ZERO)
                .append(POP)
                .sstore(U256::from(99), U256::from(1)),
        );
        let db =
            world(&[(CONTRACT, parent), (super::GRAND, mid), (CHILD, super::store_n_slots(1))]);
        let limits = EvmTxRuntimeLimits::default().with_tx_kv_update_limit(100);
        let outcome = execute_under(db, call_tx(gas), limits);
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert!(outcome.limit_exceeded.is_none(), "the top frame caught the stop");
        assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 98 });
        assert_eq!(slot_value(&outcome, super::GRAND, 1), U256::ZERO);
    }
}

/// Under a transaction KV limit of 10 the creator, a child of the top frame, has a KV budget of
/// 9. It keeps 9 records and then creates: the creation's budget is the share of the nothing the
/// creator has left, 0, and its own record stops it at its start. The start bumped the creator's
/// nonce, whose record is counted on the creator when the creation returns, before it runs on:
/// the creator stops, kind 1, `limit` 9, its frame budget.
///
/// With one record of room the creation's budget is still `floor(1 × 98 / 100)` = 0, so the
/// creation is stopped at its start, kind 1, `limit` 0, and the creator keeps the bump and its
/// record in the one it has: its nonce is 1.
#[test]
fn test_a_creator_nonce_record_stops_the_creator_at_its_kv_budget() {
    let Some(gas) = slots_gas(12) else { return };
    let limits = EvmTxRuntimeLimits::default().with_tx_kv_update_limit(10);
    let creator = |records: u16| {
        let mut code = store_n_prefix(records, 1, false);
        let init = BytecodeBuilder::default().return_with_data([0x00]).build();
        code.extend_from_slice(
            &BytecodeBuilder::default()
                .create(U256::ZERO, init)
                .append(POP)
                .return_returndata()
                .build(),
        );
        Bytes::from(code)
    };

    let db = world(&[(CONTRACT, super::forward_to(CHILD)), (CHILD, creator(9))]);
    let outcome = execute_under(db, call_tx(gas), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 9 });
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO, "the stopped creator keeps nothing");

    let db = world(&[(CONTRACT, super::forward_to(CHILD)), (CHILD, creator(8))]);
    let outcome = execute_under(db, call_tx(gas), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 0 });
    assert_eq!(super::nonce_of(&outcome, CHILD), 1, "a stopped creation keeps the creator's bump");
    assert_eq!(slot_value(&outcome, CHILD, 8), U256::from(1));
}

/// A value `CALL` to the BN254 pairing precompile with no input, whose price, 45,000, is past
/// what a read of the block environment under a cap of 20,000 leaves once the caller's own
/// `CALL` is paid, and within the forward. Gas detention answers it from its price before revm
/// runs it, so the value never moves: the price is the stop, kind 2, and nothing is moved, logged
/// or recorded, the new account's state gas coming back. A value the caller cannot fund runs no
/// precompile, counts nothing and is stopped by no limit.
///
/// The answer moved nothing, so it is held to the price alone: under a transaction KV limit of 2,
/// where the started frame's KV share, `floor(2 × 98 / 100)` = 1, could not hold the two records
/// its value transfer would make, the stop is still kind 2. A precompile the node cannot price
/// needs a node precompile and is not run.
#[test]
fn test_a_precompile_answered_from_its_price_is_held_to_the_price_alone() {
    const PAIRING: alloy_primitives::Address =
        alloy_primitives::address!("0000000000000000000000000000000000000008");
    let paying = |value: u64| {
        BytecodeBuilder::default()
            .append_many([TIMESTAMP, POP])
            .call(PAIRING, U256::from(value))
            .append(POP)
            .return_returndata()
            .build()
    };
    let gas = super::EXECUTION_CAP + crate::common::value_call_room();
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(20_000);

    let outcome = execute_under(world(&[(CONTRACT, paying(1))]), call_tx(gas), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome).kind, 2, "the price is the stop");
    assert_eq!(super::balance_of(&outcome, PAIRING), U256::ZERO, "the value never moved");
    assert!(outcome.result.logs().is_empty(), "no transfer log");
    assert_eq!(outcome.usage.write_records, 0, "no record");
    assert_eq!(outcome.usage.data_size, super::BODY_BYTES);
    assert_eq!(outcome.gas.state, 0, "the new account's charge comes back");

    let outcome = execute_under(world(&[(CONTRACT, paying(5))]), call_tx(gas), limits);
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("an unfunded value call fails and its caller runs on: {:?}", outcome.result);
    };
    assert!(output.data().is_empty(), "no precompile ran and no stop was returned");
    assert!(outcome.limit_exceeded.is_none());
    assert_eq!(outcome.usage.write_records, 0);
    assert_eq!(outcome.usage.data_size, super::BODY_BYTES);
    assert_eq!(outcome.gas.state, 0, "the unfunded start is charged nothing");

    {
        let limits = limits.with_tx_kv_update_limit(2);
        let outcome = execute_under(world(&[(CONTRACT, paying(1))]), call_tx(gas), limits);
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(returned_stop(&outcome).kind, 2, "the price alone, not the records");
        assert_eq!(super::balance_of(&outcome, PAIRING), U256::ZERO);
        assert_eq!(outcome.usage.write_records, 0);
    }
}

/// A precompile the node installed, which runs out of the allowance gas detention leaves it.
///
/// A precompile the engine cannot price runs on what the compute limit leaves its frame: a value
/// `CALL` to a node's own precompile, installed through `with_dyn_precompiles`, whose price,
/// 100,000, is past what a read of the block environment under a cap of 30,000 leaves once the
/// caller's own `CALL` is paid, and within the forward. It runs out of that allowance, and that is
/// the stop, kind 2; revm took the value and the transfer log back with the precompile's failure,
/// so the start kept nothing and is held to nothing. Under a transaction KV limit of 2, where the
/// started frame's KV share, `floor(2 × 98 / 100)` = 1, could not hold the two records its value
/// transfer would make, the stop is still kind 2: the same as for a precompile the engine prices.
#[test]
fn test_an_unpriced_precompile_out_of_its_allowance_is_held_to_the_allowance_alone() {
    use alloy_primitives::map::HashMap;
    use mega_evm::MegaEvm;
    use revm::bytecode::opcode::{CALL, GAS, PUSH0};

    use crate::withheld_gas::{priced, Runs, PRICED};

    const PRICE: u64 = 100_000;
    let code = BytecodeBuilder::default()
        .append_many([TIMESTAMP, POP])
        .mstore(0, U256::from(PRICE).to_be_bytes::<32>())
        .append_many([PUSH0, PUSH0])
        .push_number(32_u8)
        .append(PUSH0)
        .push_u256(U256::from(1))
        .push_address(PRICED)
        .append_many([GAS, CALL, POP])
        .return_returndata()
        .build();
    let gas = super::EXECUTION_CAP + crate::common::value_call_room();
    let run = |limits: EvmTxRuntimeLimits| {
        let runs = Runs::default();
        let context = crate::common::context(world(&[(CONTRACT, code.clone())]))
            .with_tx_runtime_limits(limits);
        let outcome = MegaEvm::new(context)
            .with_dyn_precompiles(HashMap::from_iter([(PRICED, priced(&runs))]))
            .execute_transaction(call_tx(gas))
            .expect("the transaction is valid");
        let ran = runs.lock().unwrap().clone();
        (outcome, ran)
    };
    let limits = EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(30_000);

    let (outcome, ran) = run(limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome).kind, 2, "the allowance is the stop");
    assert!(
        matches!(ran.as_slice(), [(allowance, false)] if *allowance < 30_000),
        "it ran once, on the allowance, short of its price: {ran:?}"
    );
    assert_eq!(super::balance_of(&outcome, PRICED), U256::ZERO, "the value is taken back");
    assert_eq!(outcome.usage.write_records, 0, "the records are taken back");
    assert_eq!(outcome.usage.data_size, super::BODY_BYTES);

    let (outcome, _) = run(limits.with_tx_kv_update_limit(2));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome).kind, 2, "the allowance alone, not the records");
    assert_eq!(super::balance_of(&outcome, PRICED), U256::ZERO);
    assert_eq!(outcome.usage.write_records, 0);
    assert_eq!(outcome.usage.data_size, super::BODY_BYTES);
}
