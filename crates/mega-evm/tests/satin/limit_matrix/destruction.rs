//! The same-transaction destruction table, and the state growth a `SELFDESTRUCT` adds.
//!
//! A scenario run through [`plain_and_probed`] reads its figures off a plain run and the
//! state-growth net it kept off the probe's budget. The figures are the page's at `m` = 1 and the
//! spec's byte prices: a test that pins them returns early at other prices. Kind 1 runs under a
//! reduced transaction KV limit.

use alloy_op_evm::OpTx;
use alloy_primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    satin_gas_params,
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaLimitExceeded, MegaTransaction, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{
        ADDRESS, CALL, CALLDATALOAD, CALLDATASIZE, CALLVALUE, GAS, JUMPDEST, JUMPI, LT, MLOAD, POP,
        PUSH0, SELFDESTRUCT, SLOAD, SSTORE, STOP,
    },
    context::{result::ExecutionResult, TxEnv},
    interpreter::{
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, Jumps},
        Interpreter,
    },
    Database, Inspector,
};

use super::{
    assert_growth, at_spec_prices, balance_of, call_tx, call_with_gas, create_tx, delegatecall,
    execute, has_code, is_destroyed, plain_and_probed, probe_gas, returned_stop, slot_value,
    slots_gas, store_n_prefix, store_n_prefix_at, warm_call, world, ACCOUNT_LEAF_STATE_GAS_M1,
    BENEFICIARY, BODY_BYTES, BODY_HISTORY, CALLER, CHILD, CODE_DEPOSIT_STATE_PER_BYTE, CONTRACT,
    EXECUTION_CAP, EXISTING, GRAND, HISTORY_GAS_PER_BYTE, NEXT, RECORD_HISTORY, SLOT_STATE_GAS_M1,
    TRANSFER_LOG_SIZE, TX_STATE_GROWTH_LIMIT, WRITE_RECORD_SIZE,
};
use crate::common::{self, context, state_is_free};

/// Whether the page's state and history figures hold at the prices in effect.
fn pins_state() -> bool {
    at_spec_prices() && !state_is_free() && !common::history_is_free()
}

/// History gas of `bytes` at the spec's byte price.
const fn history(bytes: u64) -> u64 {
    bytes * HISTORY_GAS_PER_BYTE
}

/// State gas of deploying `code` at `m` = 1.
fn code_state(code: &[u8]) -> u64 {
    code.len() as u64 * CODE_DEPOSIT_STATE_PER_BYTE
}

fn runtime_to(beneficiary: Address) -> Bytes {
    BytecodeBuilder::default().selfdestruct(beneficiary).build()
}

fn init_returning(prefix: BytecodeBuilder, runtime: &[u8]) -> Bytes {
    prefix.return_with_data(runtime).build()
}

fn created_at(init: &[u8]) -> Address {
    CONTRACT.create2(B256::ZERO, keccak256(init))
}

/// `CREATE2` of `init` with salt 0 carrying `endowment`, its address popped, then `tail`. The tail
/// has no absolute jump: a loop in it is placed with `store_n_prefix_at`.
fn after_create(init: Bytes, endowment: U256, tail: BytecodeBuilder) -> Vec<u8> {
    let mut code = BytecodeBuilder::default()
        .create2(endowment, init, U256::ZERO)
        .append(POP)
        .build()
        .to_vec();
    code.extend_from_slice(&tail.build());
    code
}

fn funded_contract(code: impl Into<Bytes>) -> MemoryDatabase {
    let mut db = world(&[(CONTRACT, code.into()), (EXISTING, Bytes::from_static(&[STOP]))]);
    db.set_account_balance(CONTRACT, U256::from(10u128.pow(24)));
    db
}

/// [`plain_and_probed`] on a funded contract. `setup` adds accounts to both databases.
fn both(
    scenario: Vec<u8>,
    setup: impl Fn(&mut MemoryDatabase),
) -> Option<(MegaTransactionOutcome, MegaTransactionOutcome)> {
    plain_and_probed(&scenario, |code| {
        let mut db = funded_contract(code);
        setup(&mut db);
        db
    })
}

/// Runtime that stores its 32-byte calldata into slot 1, or with no calldata self-destructs to
/// `beneficiary`.
fn store_or_destroy(beneficiary: Address) -> Bytes {
    let destroy = BytecodeBuilder::default().selfdestruct(beneficiary).build();
    // `CALLDATASIZE`, `PUSH1 store`, `JUMPI` come first.
    let store = 4 + destroy.len();
    let mut code = BytecodeBuilder::default()
        .append(CALLDATASIZE)
        .push_number(store as u8)
        .append(JUMPI)
        .build()
        .to_vec();
    code.extend_from_slice(&destroy);
    code.extend_from_slice(
        &BytecodeBuilder::default()
            .append_many([JUMPDEST, PUSH0, CALLDATALOAD])
            .push_number(1_u8)
            .append_many([SSTORE, STOP])
            .build(),
    );
    Bytes::from(code)
}

/// `CALL` of `target` with all the gas left and one 32-byte word of calldata, `word`.
fn call_with_word(code: BytecodeBuilder, target: Address, word: u64) -> BytecodeBuilder {
    code.mstore(0x200, U256::from(word).to_be_bytes::<32>())
        .append_many([PUSH0, PUSH0])
        .push_number(32_u8)
        .push_number(0x200_u16)
        .append(PUSH0)
        .push_address(target)
        .append_many([GAS, CALL])
}

/// Create an account that did not exist, deposit its code, store one fresh slot, and
/// `SELFDESTRUCT` it to a new beneficiary, moving a non-zero balance. The leaf, the code and the
/// slot are given back at the prices charged, and their records and the code's data size and
/// history go with them. The logs, here the two transfer logs, stay counted. The beneficiary is
/// +1 and its leaf, 183,600 at `m` = 1, stays charged: the kept net is 1.
///
/// The beneficiary is held after the give-back: a destroying frame exactly at its budget, here 0,
/// subtracts the absent leaf and its new slot, `1 + N` = 2, and the beneficiary fits.
#[test]
fn test_a_kept_destruction_gives_back_the_leaf_the_code_and_the_slot() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(BENEFICIARY);
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init.clone(),
        U256::from(1),
        BytecodeBuilder::default().call(created, U256::ZERO).append(POP),
    );
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created), "the account is removed");
    assert_eq!(balance_of(&outcome, BENEFICIARY), U256::from(1));
    assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1, "only the new beneficiary stays");
    // Kept: the creator's record and the beneficiary's. The endowment's and the move's transfer
    // logs stay counted and pay no history.
    assert_eq!(outcome.usage.write_records, 2);
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 2 * WRITE_RECORD_SIZE + 2 * TRANSFER_LOG_SIZE);
    assert_eq!(outcome.gas.history, history(BODY_BYTES + 2 * WRITE_RECORD_SIZE));
    assert_eq!(outcome.gas.history_bytes, BODY_BYTES + 2 * WRITE_RECORD_SIZE);
    assert_eq!(outcome.result.logs().len(), 2, "the two transfer logs stay");
    assert_growth(&probed, 1);

    // At the budget: the creation is +2 on the creator, which then stores 998 slots, so the
    // destroying frame's budget is `floor(0 × 98 / 100)` = 0.
    let mut code = after_create(init, U256::from(1), BytecodeBuilder::default());
    code.extend_from_slice(&store_n_prefix_at(code.len(), 998, 1, false));
    code.extend_from_slice(&BytecodeBuilder::default().call(created, U256::ZERO).stop().build());
    let Some(gas) = slots_gas(1_010) else { return };
    let outcome = execute(funded_contract(code), call_tx(gas));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created), "the destruction at the budget fits");
    assert_eq!(balance_of(&outcome, BENEFICIARY), U256::from(1), "the beneficiary fits");
    assert_eq!(slot_value(&outcome, CONTRACT, 998), U256::from(1));
}

/// The top frame creates the account. An inner frame calls it, it self-destructs, and the inner
/// frame then reverts. The creation sits outside that revert: the mark, the falls, the give-back
/// and the record takeback are discarded, so the leaf and the code stay charged, the creator's
/// and the created account's records stand, and the kept net is the creation's +1.
#[test]
fn test_a_reverted_inner_frame_discards_the_destruction() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(EXISTING);
    let init = init_returning(BytecodeBuilder::default(), &runtime);
    let created = created_at(&init);
    let parent = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default().call(CHILD, U256::ZERO).append(POP),
    );
    let child = BytecodeBuilder::default().call(created, U256::ZERO).revert().build();
    let Some((outcome, probed)) = both(parent, |db| db.set_account_code(CHILD, child.clone()))
    else {
        return;
    };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(!is_destroyed(&outcome, created), "the revert discards the destruction");
    assert!(has_code(&outcome, created));
    assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1 + code_state(&runtime));
    assert_eq!(outcome.usage.write_records, 2, "the records stand again");
    assert_growth(&probed, 1);
}

/// A pre-existing contract, a child of the top frame with a state-growth budget of 980, stores
/// 980 slots and self-destructs to a new beneficiary. It was not created in the transaction, so
/// there is no fall and no give-back. The beneficiary's +1 crosses: the stop names the frame
/// budget, kind 3, `limit` 980, and the operation keeps nothing.
#[test]
fn test_a_new_beneficiary_at_a_full_budget_stops_the_frame() {
    let mut pre = store_n_prefix(980, 1, false);
    pre.extend_from_slice(&runtime_to(BENEFICIARY));
    let mut db = world(&[(CONTRACT, super::forward_to(CHILD)), (CHILD, Bytes::from(pre))]);
    db.set_account_balance(CHILD, U256::from(10));
    let Some(gas) = slots_gas(1_000) else { return };
    let outcome = execute(db, call_tx(gas));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 3, limit: 980 });
    assert_eq!(balance_of(&outcome, BENEFICIARY), U256::ZERO, "the operation keeps nothing");
    assert_eq!(balance_of(&outcome, CHILD), U256::from(10));
    assert_eq!(slot_value(&outcome, CHILD, 1), U256::ZERO);
}

/// Create an account with one fresh slot outside an enclosing frame. The enclosing frame destroys
/// it, stores the slot to another non-zero value, stores it to zero, and destroys it again.
///
/// When the enclosing frame is kept, the first destruction gives the leaf, the code and the slot
/// back and takes their records back; the later stores do not rise, fall, charge, refill or make
/// a record; the second destruction does nothing. When it reverts, all of that is discarded: the
/// creation's charges and its three records stand, and the kept net is 2.
#[test]
fn test_stores_after_the_mark_and_a_reverted_enclosing_frame() {
    if !pins_state() {
        return;
    }
    let runtime = store_or_destroy(EXISTING);
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let parent = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default().call(CHILD, U256::ZERO).append(POP),
    );
    for kept in [true, false] {
        let enclosing = BytecodeBuilder::default().call(created, U256::ZERO).append(POP);
        let enclosing = call_with_word(enclosing, created, 2).append(POP);
        let enclosing = call_with_word(enclosing, created, 0).append(POP);
        let enclosing = enclosing.call(created, U256::ZERO).append(POP);
        let enclosing = if kept { enclosing.stop() } else { enclosing.revert() };
        let enclosing = enclosing.build();
        assert_eq!(
            sstore_state_gas(&parent, |db| db.set_account_code(CHILD, enclosing.clone()), created),
            [SLOT_STATE_GAS_M1 as i64, 0, 0],
            "kept {kept}: the stores after the mark move no state gas"
        );
        let Some((outcome, probed)) =
            both(parent.clone(), |db| db.set_account_code(CHILD, enclosing.clone()))
        else {
            return;
        };
        assert!(outcome.result.is_success(), "kept {kept}: {:?}", outcome.result);
        if kept {
            assert!(is_destroyed(&outcome, created));
            assert_eq!(outcome.gas.state, 0, "given back once, and nothing after the mark");
            assert_eq!(outcome.usage.write_records, 1, "only the creator's record");
            assert_eq!(outcome.usage.data_size, BODY_BYTES + WRITE_RECORD_SIZE);
            assert_growth(&probed, 0);
        } else {
            assert!(!is_destroyed(&outcome, created));
            assert_eq!(slot_value(&outcome, created, 1), U256::from(1));
            assert_eq!(
                outcome.gas.state,
                ACCOUNT_LEAF_STATE_GAS_M1 + SLOT_STATE_GAS_M1 + code_state(&runtime)
            );
            assert_eq!(outcome.usage.write_records, 3, "the records stand again");
            assert_eq!(
                outcome.usage.data_size,
                BODY_BYTES + 3 * WRITE_RECORD_SIZE + runtime.len() as u64
            );
            assert_growth(&probed, 2);
        }
    }
}

/// Runtime that accepts value and stops, or with no value emits one `LOG3` of a word and
/// self-destructs to `beneficiary`.
fn log_and_destroy(beneficiary: Address) -> Bytes {
    let body = BytecodeBuilder::default().log3_word().selfdestruct(beneficiary).build();
    let accept = 4 + body.len();
    let mut code = BytecodeBuilder::default()
        .append(CALLVALUE)
        .push_number(accept as u8)
        .append(JUMPI)
        .build()
        .to_vec();
    code.extend_from_slice(&body);
    code.extend_from_slice(&[JUMPDEST, STOP]);
    Bytes::from(code)
}

/// A kept same-transaction destruction of an account absent at the start, with one fresh slot,
/// deposited code, one `LOG3` of a word and a non-zero balance moved to an existing account. The
/// account's and the slot's records are taken back, and the code's data size and history with
/// them; state gas comes back whole. The `LOG3` stays counted, 160 bytes, and its history stays
/// charged; the transfer logs stay counted and pay none. A later value transfer to the marked
/// account makes no record and is not a rise.
#[test]
fn test_a_destruction_gives_back_record_history_and_keeps_the_logs() {
    if !pins_state() {
        return;
    }
    let runtime = log_and_destroy(EXISTING);
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::from(1),
        BytecodeBuilder::default()
            .call(created, U256::ZERO)
            .append(POP)
            .call(created, U256::from(1))
            .append(POP),
    );
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert_eq!(outcome.gas.state, 0, "the leaf, the slot and the code come back");
    // Kept records: the creator and the existing beneficiary. Transfer logs: the endowment, the
    // move, and the later value.
    const LOG3_WORD: u64 = 32 + 3 * 32 + 32;
    assert_eq!(outcome.usage.write_records, 2);
    assert_eq!(
        outcome.usage.data_size,
        BODY_BYTES + 2 * WRITE_RECORD_SIZE + LOG3_WORD + 3 * TRANSFER_LOG_SIZE
    );
    assert_eq!(outcome.gas.history, history(BODY_BYTES + 2 * WRITE_RECORD_SIZE + LOG3_WORD));
    assert_eq!(outcome.gas.history_bytes, BODY_BYTES + 2 * WRITE_RECORD_SIZE + LOG3_WORD);
    assert_eq!(outcome.result.logs().len(), 4, "the LOG3 and the three transfer logs");
    assert_growth(&probed, 0);
}

/// The creation is +2 on the top frame, whose remaining becomes 998, so the enclosing frame it
/// calls gets `floor(998 × 98 / 100)` = 978. The enclosing frame calls the account, which
/// destroys itself and returns a net of −2. The enclosing frame's remaining stays at its budget,
/// 978, not 980, so the child it then starts gets `floor(978 × 98 / 100)` = 958.
#[test]
fn test_a_destroying_childs_negative_net_does_not_raise_the_parents_budget() {
    assert_eq!(super::frame_share(998), 978);
    assert_eq!(super::frame_share(978), 958);
    let runtime = runtime_to(EXISTING);
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let parent = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default().call(CHILD, U256::ZERO).return_returndata(),
    );
    let enclosing = BytecodeBuilder::default()
        .call(created, U256::ZERO)
        .append(POP)
        .call(NEXT, U256::ZERO)
        .return_returndata()
        .build();
    let mut db = funded_contract(parent);
    db.set_account_code(CHILD, enclosing);
    db.set_account_code(NEXT, super::probe_code());
    let Some(gas) = probe_gas(2) else { return };
    let outcome = execute(db, call_tx(gas));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 3, limit: 958 });
    assert_eq!(slot_value(&outcome, NEXT, 1), U256::ZERO);
}

/// The enclosing frame, given 2,000,000 gas, halts out of gas after a successful destroying
/// child. The mark, the falls, the give-back and the record takeback are discarded: the leaf, the
/// slot and the code stay charged, the three records stand, and the kept net is 2.
#[test]
fn test_a_halt_after_a_destroying_child_discards_the_mark() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(EXISTING);
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let parent = after_create(
        init,
        U256::ZERO,
        call_with_gas(BytecodeBuilder::default(), CHILD, U256::ZERO, 2_000_000).append(POP),
    );
    // A load at offset 2^32 − 1 cannot pay its memory: an out-of-gas halt.
    let enclosing = BytecodeBuilder::default()
        .call(created, U256::ZERO)
        .append(POP)
        .push_number(u32::MAX)
        .append_many([MLOAD, STOP])
        .build();
    let Some((outcome, probed)) = both(parent, |db| db.set_account_code(CHILD, enclosing.clone()))
    else {
        return;
    };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(!is_destroyed(&outcome, created), "the halt discards the destruction");
    assert_eq!(slot_value(&outcome, created, 1), U256::from(1));
    assert_eq!(
        outcome.gas.state,
        ACCOUNT_LEAF_STATE_GAS_M1 + SLOT_STATE_GAS_M1 + code_state(&runtime)
    );
    assert_eq!(outcome.usage.write_records, 3, "the records stand again");
    assert_growth(&probed, 2);
}

/// A second `SELFDESTRUCT` of an account created in the transaction, with no balance moved and no
/// funding between, subtracts nothing, gives nothing back, takes no record back and creates no
/// beneficiary: the beneficiary does not exist and a zero balance does not create it.
#[test]
fn test_a_second_destruction_with_no_balance_subtracts_nothing() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(BENEFICIARY);
    let init = init_returning(BytecodeBuilder::default(), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default()
            .call(created, U256::ZERO)
            .append(POP)
            .call(created, U256::ZERO)
            .append(POP),
    );
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert_eq!(outcome.gas.state, 0, "the first destruction gives the leaf and the code back once");
    assert_eq!(outcome.usage.write_records, 1, "only the creator's record");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + WRITE_RECORD_SIZE);
    assert_eq!(balance_of(&outcome, BENEFICIARY), U256::ZERO, "no beneficiary");
    assert_growth(&probed, 0);
}

/// The same repeated destruction after the account was funded again, moving that non-zero
/// balance to an account that does not exist. The second destruction still subtracts nothing and
/// gives nothing back; the value sent to the marked account makes no record. The beneficiary is a
/// rise, 183,600 at `m` = 1, and its record is kept.
#[test]
fn test_a_second_destruction_after_refunding_charges_only_the_beneficiary() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(BENEFICIARY);
    let init = init_returning(BytecodeBuilder::default(), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default()
            .call(created, U256::ZERO)
            .append(POP)
            .call(created, U256::from(1))
            .append(POP),
    );
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1,
        "the second destruction only adds the beneficiary"
    );
    assert_eq!(balance_of(&outcome, BENEFICIARY), U256::from(1));
    assert_eq!(outcome.usage.write_records, 2, "the creator and the beneficiary");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 2 * WRITE_RECORD_SIZE + 2 * TRANSFER_LOG_SIZE);
    assert_growth(&probed, 1);
}

/// Value to the counterfactual address A, then `CREATE2` onto A, then A self-destructs to an
/// existing account. The value call is the rise and the leaf's charge; the creation onto a
/// funded address adds neither. A did not exist at the start, so the destruction is the absent
/// row: it gives back the value call's leaf charge and the code, and takes A's record back.
#[test]
fn test_value_then_create2_then_destruct_gives_the_value_calls_leaf_back() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(EXISTING);
    let init = init_returning(BytecodeBuilder::default(), &runtime);
    let created = created_at(&init);
    let code = BytecodeBuilder::default()
        .call(created, U256::from(1))
        .append(POP)
        .create2(U256::ZERO, init, U256::ZERO)
        .append(POP)
        .call(created, U256::ZERO)
        .append(POP)
        .build()
        .to_vec();
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert_eq!(outcome.gas.state, 0, "the value call's leaf is given back to the destroying frame");
    // Kept: the creator's record and the existing beneficiary's. Transfer logs: the value call and
    // the move.
    assert_eq!(outcome.usage.write_records, 2);
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 2 * WRITE_RECORD_SIZE + 2 * TRANSFER_LOG_SIZE);
    assert_growth(&probed, 0);
}

/// The reservoir is empty. A constructor stores three fresh slots, burns its gas down to about
/// 100,000 left after `SELFDESTRUCT`'s static and cold costs, and self-destructs to a new
/// beneficiary, moving its endowment. The destruction would give back 477,360 at `m` = 1, the
/// leaf's 183,600 and three slots of 97,920, but the beneficiary's new-account charge, 183,600,
/// is made first and cannot be paid: the constructor halts out of gas, the give-back is not
/// credited, and the creation fails with no beneficiary.
#[test]
fn test_a_beneficiary_charge_the_frame_cannot_pay_halts_before_the_give_back() {
    if !pins_state() {
        return;
    }
    assert_eq!(ACCOUNT_LEAF_STATE_GAS_M1 + 3 * SLOT_STATE_GAS_M1, 477_360);
    // `GAS`, `PUSH3`, `LT`, `PUSH1`, `JUMPI`, `JUMPDEST`: 22 gas a turn, 19 after the last `GAS`.
    // Then `PUSH20` and `SELFDESTRUCT`'s static 5,000 and the cold beneficiary.
    let after_loop = 19 + 3 + 5_000 + satin_gas_params().selfdestruct_cold_cost();
    let target = u32::try_from(100_000 + 22 + after_loop).expect("fits a PUSH3");
    let slots = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(1))
        .sstore(U256::from(3), U256::from(1))
        .build();
    let entry = slots.len();
    let mut init = slots.to_vec();
    init.extend_from_slice(
        &BytecodeBuilder::default()
            .append_many([JUMPDEST, GAS])
            .push_bytes(&target.to_be_bytes()[1..])
            .append(LT)
            .push_number(entry as u8)
            .append(JUMPI)
            .selfdestruct(BENEFICIARY)
            .build(),
    );
    let code = BytecodeBuilder::default().create(U256::from(1), init).return_top().build();
    let outcome = execute(funded_contract(code), call_tx(5_000_000));
    let ExecutionResult::Success { output, .. } = &outcome.result else {
        panic!("the creator runs on: {:?}", outcome.result);
    };
    assert_eq!(output.data().as_ref(), &[0_u8; 32], "the creation failed");
    assert_eq!(balance_of(&outcome, BENEFICIARY), U256::ZERO, "no beneficiary is kept");
    assert_eq!(outcome.gas.state, 0, "nothing of the failed creation stays charged");
    assert_eq!(outcome.gas.reservoir_remaining, 0);
}

/// After the mark, an `SSTORE` of the destroyed account's own slot, `(original 0, present
/// non-zero, new 0)`, on the slot the destruction already gave back. No rise, no fall, no
/// fresh-slot charge and no refill: the state gas is still given back exactly once, and the store
/// makes no record. The EIP-3529 refund counter is not pinned here.
#[test]
fn test_a_cleared_slot_after_the_mark_does_not_refill_again() {
    if !pins_state() {
        return;
    }
    let runtime = store_or_destroy(EXISTING);
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        call_with_word(
            BytecodeBuilder::default().call(created, U256::ZERO).append(POP),
            created,
            0,
        )
        .append(POP),
    );
    let fresh = SLOT_STATE_GAS_M1 as i64;
    assert_eq!(
        sstore_state_gas(&code, |_| {}, created),
        [fresh, 0],
        "the constructor's store is charged; the clear after the mark moves no state gas"
    );
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert_eq!(outcome.gas.state, 0, "given back once by the table and not refilled again");
    assert_eq!(outcome.usage.write_records, 1, "the store after the mark makes no record");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + WRITE_RECORD_SIZE);
    assert_growth(&probed, 0);
}

/// A pre-existing contract, unchanged so far and holding a balance, self-destructs to an account
/// the transaction already changed. Under a transaction KV limit of 3, the top frame's two records
/// for the earlier transfer leave it 1, so the destroying frame's KV budget is
/// `floor(1 × 98 / 100)` = 0. The cleared account's one record crosses it, kind 1, `limit` 0; the
/// beneficiary adds no record; the operation keeps nothing. Under a limit of 4 the budget is 1
/// and the same operation fits.
#[test]
fn test_a_cleared_account_with_no_kv_room_stops_at_the_frame_budget() {
    let code = BytecodeBuilder::default()
        .call(EXISTING, U256::from(1))
        .append(POP)
        .call(CHILD, U256::ZERO)
        .return_returndata()
        .build();
    let Some(gas) = slots_gas(2) else { return };
    for (limit, fits) in [(3, false), (4, true)] {
        let mut db = funded_contract(code.clone());
        db.set_account_code(CHILD, runtime_to(EXISTING));
        db.set_account_balance(CHILD, U256::from(10));
        let limits = mega_evm::EvmTxRuntimeLimits::default().with_tx_kv_update_limit(limit);
        let outcome = super::execute_under(db, call_tx(gas), limits);
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        if fits {
            assert_eq!(balance_of(&outcome, CHILD), U256::ZERO);
            assert_eq!(balance_of(&outcome, EXISTING), U256::from(12));
        } else {
            assert_eq!(returned_stop(&outcome), MegaLimitExceeded { kind: 1, limit: 0 });
            assert_eq!(balance_of(&outcome, CHILD), U256::from(10), "the operation keeps nothing");
            assert_eq!(balance_of(&outcome, EXISTING), U256::from(2));
        }
    }
}

/// A destruction inside a frame that reverts, then a later destruction of the same account in a
/// frame that is kept. The first mark does not stand, so the later destruction is the first
/// effective one: it gives the leaf, the slot and the code back once and takes their records back
/// once.
#[test]
fn test_only_the_kept_destruction_gives_the_charges_back() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(EXISTING);
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let parent = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default()
            .call(CHILD, U256::ZERO)
            .append(POP)
            .call(created, U256::ZERO)
            .append(POP),
    );
    let child = BytecodeBuilder::default().call(created, U256::ZERO).revert().build();
    let Some((outcome, probed)) = both(parent, |db| db.set_account_code(CHILD, child.clone()))
    else {
        return;
    };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert_eq!(outcome.gas.state, 0, "the leaf, the code and the slot come back once");
    assert_eq!(outcome.usage.write_records, 1, "their records are taken back once");
    assert_eq!(outcome.gas.history, history(BODY_BYTES + WRITE_RECORD_SIZE));
    assert_growth(&probed, 0);
}

/// A new account destroyed by its constructor, before any code is deposited and before any slot
/// is written. The leaf is refilled and falls, and the account's record is taken back. Nothing
/// else was charged.
#[test]
fn test_a_new_account_destroyed_before_code_or_slots_refills_the_leaf() {
    if !pins_state() {
        return;
    }
    let init = runtime_to(EXISTING);
    let created = created_at(&init);
    let code = after_create(init, U256::ZERO, BytecodeBuilder::default());
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert_eq!(outcome.gas.state, 0, "the leaf is refilled and nothing else was charged");
    assert_eq!(outcome.usage.write_records, 1, "only the creator's record");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + WRITE_RECORD_SIZE);
    assert_growth(&probed, 0);
}

/// A pre-funded address is created onto, and its constructor destroys it to itself, so no balance
/// moves to another account. The leaf was present: no charge, no refill, no fall, and the record
/// the nonce bump made stands. No beneficiary and no transfer log.
#[test]
fn test_a_prefunded_destruction_that_moves_no_balance_keeps_the_nonce_record() {
    if !pins_state() {
        return;
    }
    let init = BytecodeBuilder::default().append_many([ADDRESS, SELFDESTRUCT]).build();
    let created = created_at(&init);
    let code = after_create(init, U256::ZERO, BytecodeBuilder::default());
    let Some((outcome, probed)) = both(code, |db| db.set_account_balance(created, U256::from(1)))
    else {
        return;
    };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0, "a present leaf is neither charged nor refilled");
    assert_eq!(outcome.usage.write_records, 2, "the creator's and the nonce bump's records");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 2 * WRITE_RECORD_SIZE);
    assert_growth(&probed, 0);
}

/// Code deposited, then the account is destroyed in a later call. The account was absent at the
/// start. The code deposit's state gas is refilled, and the code's data size and history are taken
/// back; the code is no record and no unit of state growth.
#[test]
fn test_destroyed_code_refills_its_deposit_and_its_history() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(EXISTING);
    let init = init_returning(BytecodeBuilder::default(), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default().call(created, U256::ZERO).append(POP),
    );
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert!(code_state(&runtime) > 0, "the runtime deposits code");
    assert_eq!(outcome.gas.state, 0, "the code deposit and the leaf are refilled");
    // The call carries no calldata: the body is 310 bytes, and the creator's record is kept.
    assert_eq!(outcome.gas.history, history(BODY_BYTES + WRITE_RECORD_SIZE));
    assert_eq!(outcome.gas.history_bytes, BODY_BYTES + WRITE_RECORD_SIZE);
    assert_eq!(outcome.usage.data_size, BODY_BYTES + WRITE_RECORD_SIZE);
    assert_eq!(outcome.usage.write_records, 1);
    assert_growth(&probed, 0);
}

/// Two slots, each original zero and present non-zero, one of them restored to zero before the
/// destruction. The non-zero slot is the zero, non-zero, deleted row: refilled, −1, record back.
/// The restored slot is the zero, zero, deleted row and is not counted again.
#[test]
fn test_slots_deleted_with_the_account_refill_once() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(EXISTING);
    let init = init_returning(
        BytecodeBuilder::default()
            .sstore(U256::from(1), U256::from(1))
            .sstore(U256::from(2), U256::from(1))
            .sstore(U256::from(1), U256::ZERO),
        &runtime,
    );
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default().call(created, U256::ZERO).append(POP),
    );
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert_eq!(outcome.gas.state, 0, "each slot refills once");
    assert_eq!(outcome.usage.write_records, 1, "only the creator's record");
    assert_growth(&probed, 0);
}

/// A creation onto an address that held nonce 0, balance 0, empty code and slot 1 = 5 at the
/// transaction's start. `CREATE2` does not collide on storage. The address is empty under
/// EIP-161, so the creation charges a new account, 183,600 at `m` = 1, and counts a rise; the leaf
/// is the present row, so the destruction neither refills that charge nor subtracts the rise, and
/// the account's record stands. The code was absent and is given back.
///
/// The runtime's `SLOAD` of slot 1 reads zero, not the 5 the database holds: a slot of an account
/// created in the transaction that the transaction first accesses after the creation reads zero,
/// so its original is zero, and it is the zero, zero, deleted row, which makes no record. Only a
/// slot accessed before the creation keeps the value the database holds (the record unit test).
#[test]
fn test_a_creation_onto_an_empty_address_with_storage_keeps_the_leaf_charge() {
    if !pins_state() {
        return;
    }
    let runtime = BytecodeBuilder::default()
        .push_number(1_u8)
        .append_many([SLOAD, POP])
        .selfdestruct(EXISTING)
        .build();
    let init = init_returning(BytecodeBuilder::default(), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default().call(created, U256::ZERO).append(POP),
    );
    let Some((outcome, probed)) =
        both(code, |db| db.set_account_storage(created, U256::from(1), U256::from(5)))
    else {
        return;
    };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created), "the creation did not collide");
    let slot = &outcome.state[&created].storage[&U256::from(1)];
    assert_eq!(
        (slot.original_value, slot.present_value),
        (U256::ZERO, U256::ZERO),
        "the slot loads as zero for an account created in the transaction"
    );
    assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1, "the leaf's charge stands");
    // The creator's record and the created account's, which the present leaf keeps.
    assert_eq!(outcome.usage.write_records, 2);
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 2 * WRITE_RECORD_SIZE);
    assert_growth(&probed, 1);
}

/// Runs `txs` in order over `db` twice — each committed before the next runs, and all on one
/// journal that runs them without finalizing — and returns the last one's outcome on the committed
/// path, having held the other path's to it: the same result, gas, usage and limit stop.
fn the_same_on_one_journal(
    db: MemoryDatabase,
    txs: &[MegaTransaction],
    what: &str,
) -> MegaTransactionOutcome {
    use revm::{DatabaseCommit, ExecuteEvm};
    let (last, earlier) = txs.split_last().expect("a transaction");
    let mut committed = db.clone();
    for tx in earlier {
        let outcome = execute(committed.clone(), tx.clone());
        assert!(outcome.result.is_success(), "{what}: {:?}", outcome.result);
        committed.commit(outcome.result_and_state.state);
    }
    let control = execute(committed, last.clone());

    let mut evm = mega_evm::MegaEvm::new(context(db));
    for tx in earlier {
        let result = ExecuteEvm::transact_one(&mut evm, tx.clone()).expect("valid");
        assert!(result.is_success(), "{what}: {result:?}");
    }
    let outcome = evm.execute_transaction(last.clone()).expect("valid");
    assert_eq!(outcome.result_and_state.result, control.result_and_state.result, "{what}");
    assert_eq!(outcome.gas, control.gas, "{what}");
    assert_eq!(outcome.usage, control.usage, "{what}");
    assert_eq!(outcome.limit_exceeded, control.limit_exceeded, "{what}");
    control
}

/// A leaf an earlier transaction destroyed is absent at the next transaction's start, whether the
/// two run on one journal, as a block builder runs them, or the first is committed before the
/// second runs. The address held an account the first transaction found: a balance, or nothing but
/// storage. The first transaction creates there and destroys the account, the present row, which
/// gives no leaf back; the second does the same onto an address that now holds nothing, the absent
/// row, whose leaf comes back with its state gas, its unit of state growth and its record. The
/// second transaction's outcome is the same on either path, at any byte price.
#[test]
fn test_a_leaf_an_earlier_transaction_destroyed_is_absent_at_the_next_start() {
    let init = runtime_to(EXISTING);
    let created = created_at(&init);
    let code = after_create(init, U256::ZERO, BytecodeBuilder::default().append(STOP));
    let tx = |nonce| {
        let mut tx = call_tx(BELOW_THE_CAP);
        tx.0.base.nonce = nonce;
        tx
    };
    for (held, what) in [(true, "a balance"), (false, "storage alone")] {
        let mut db = funded_contract(code.clone());
        if held {
            db.set_account_balance(created, U256::from(1));
        } else {
            db.set_account_storage(created, U256::from(1), U256::from(5));
        }

        let first = execute(db.clone(), tx(0));
        assert!(first.result.is_success(), "{what}: {:?}", first.result);
        assert!(is_destroyed(&first, created), "{what}: the first transaction destroyed it");
        let charged = if held { 0 } else { common::account_state_gas() };
        assert_eq!(first.gas.state, charged, "{what}: the present row gives no leaf back");

        let second = the_same_on_one_journal(db, &[tx(0), tx(1)], what);
        assert!(second.result.is_success(), "{what}: {:?}", second.result);
        assert_eq!(second.gas.state, 0, "{what}: the absent row gives the leaf back");
        assert_eq!(second.usage.state_growth, 0, "{what}");
        assert_eq!(second.usage.write_records, 1, "{what}: the factory's nonce alone");
        assert_eq!(second.usage.data_size, BODY_BYTES + WRITE_RECORD_SIZE, "{what}");
    }
}

/// A creation at the address an earlier transaction funded, destroyed to an existing account,
/// whose leaf the transaction found present: the creation charged no account, so the destruction
/// gives no leaf back, and the created account's record stands beside the factory's and the
/// beneficiary's, with the transfer log of the balance it moved.
fn assert_the_present_row(outcome: &MegaTransactionOutcome, created: Address, what: &str) {
    assert!(outcome.result.is_success(), "{what}: {:?}", outcome.result);
    assert!(is_destroyed(outcome, created), "{what}: the creation did not collide");
    assert_eq!(outcome.gas.state, 0, "{what}: no leaf was charged or given back");
    assert_eq!(outcome.usage.state_growth, 0, "{what}");
    assert_eq!(outcome.usage.write_records, 3, "{what}: the factory, the account, the beneficiary");
    assert_eq!(
        outcome.usage.data_size,
        BODY_BYTES + 3 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE,
        "{what}"
    );
}

/// A leaf an earlier transaction funded is present at the next transaction's start, whether the
/// two run on one journal or the first is committed before the second runs. The address held no
/// account until the first transaction moved 1 wei to it; the second creates there and destroys
/// the account to an existing one. On one journal revm's flag of the account the database answered
/// none for still stands from the first transaction's load, but the leaf held a balance when the
/// second started, so it is the present row, as on the committed path.
#[test]
fn test_a_leaf_an_earlier_transaction_funded_is_present_at_the_next_start() {
    let init = runtime_to(EXISTING);
    let created = created_at(&init);
    let code = after_create(init, U256::ZERO, BytecodeBuilder::default().append(STOP));
    let mut create = call_tx(BELOW_THE_CAP);
    create.0.base.nonce = 1;
    let fund = common::call(CALLER, created, U256::from(1), BELOW_THE_CAP);
    let outcome = the_same_on_one_journal(funded_contract(code), &[fund, create], "funded");
    assert_the_present_row(&outcome, created, "funded");
}

/// A leaf funded after an earlier transaction destroyed the account there is present at the next
/// transaction's start, on one journal as on the committed path. The first transaction creates at
/// the address and destroys the account; the second moves 1 wei to it; the third creates there
/// again and destroys the account to an existing one. On one journal revm's entry names the third
/// destruction a first one in its transaction of an account an earlier transaction destroyed, but
/// the leaf held a balance when the third started, so it is the present row. The address held no
/// account at first, or a balance the database answered.
#[test]
fn test_a_leaf_funded_after_its_destruction_is_present_at_the_next_start() {
    let init = runtime_to(EXISTING);
    let created = created_at(&init);
    let code = after_create(init, U256::ZERO, BytecodeBuilder::default().append(STOP));
    let create = |nonce| {
        let mut tx = call_tx(BELOW_THE_CAP);
        tx.0.base.nonce = nonce;
        tx
    };
    let mut fund = common::call(CALLER, created, U256::from(1), BELOW_THE_CAP);
    fund.0.base.nonce = 1;
    for (held, what) in [(false, "no account"), (true, "a balance")] {
        let mut db = funded_contract(code.clone());
        if held {
            db.set_account_balance(created, U256::from(1));
        }
        let outcome = the_same_on_one_journal(db, &[create(0), fund.clone(), create(2)], what);
        assert_the_present_row(&outcome, created, what);
    }
}

/// After the mark, an `SSTORE` of a slot the destroyed account never wrote, `(original 0, present
/// 0, new non-zero)`: the write happens, but it does not rise, keeps no fresh-slot state gas — the
/// pricing hook prices it at nothing, so the store neither charges nor refills it — and makes no
/// record.
#[test]
fn test_a_fresh_slot_written_after_the_mark_is_neither_charged_nor_counted() {
    if !pins_state() {
        return;
    }
    let runtime = store_or_destroy(EXISTING);
    let init = init_returning(BytecodeBuilder::default(), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        call_with_word(
            BytecodeBuilder::default().call(created, U256::ZERO).append(POP),
            created,
            7,
        )
        .append(POP),
    );
    assert_eq!(sstore_state_gas(&code, |_| {}, created), [0], "the store moves no state gas");
    let Some((outcome, probed)) = both(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created));
    assert_eq!(slot_value(&outcome, created, 1), U256::from(7), "the store after the mark ran");
    assert_eq!(outcome.gas.state, 0, "the slot keeps no state gas");
    assert_eq!(outcome.usage.write_records, 1, "only the creator's record");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + WRITE_RECORD_SIZE);
    assert_growth(&probed, 0);
}

/// A created account whose runtime reads the block's timestamp and then self-destructs, under a
/// compute cap its `SELFDESTRUCT` crosses: at the opcode's own gas, before the destruction is
/// written, or at its cold access to the beneficiary, after it. Either way the crossing stops the
/// destroying frame before the opcode completes, so the operation applies none of the table: the
/// account is not destroyed, its leaf, code and slot stay charged and recorded, and its caller
/// resumes and returns the stop. Under a cap that also leaves the caller's own work after the call
/// its room, the destruction completes and gives them back.
#[test]
fn test_a_detention_crossing_at_the_destruction_applies_none_of_the_table() {
    use revm::bytecode::opcode::TIMESTAMP;
    if !pins_state() {
        return;
    }
    let mut runtime = vec![TIMESTAMP, POP];
    runtime.extend_from_slice(&runtime_to(EXISTING));
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default().call(created, U256::ZERO).append(POP).return_returndata(),
    );
    // After the read: `POP`, `PUSH20`, then the opcode's 5,000 and its cold access.
    let cold = satin_gas_params().selfdestruct_cold_cost();
    for cap in [2 + 3 + 5_000 - 1, 2 + 3 + 5_000 + cold - 1] {
        let limits =
            mega_evm::EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);
        let outcome =
            super::execute_under(funded_contract(code.clone()), call_tx(BELOW_THE_CAP), limits);
        assert!(outcome.result.is_success(), "cap {cap}: {:?}", outcome.result);
        assert_eq!(returned_stop(&outcome).kind, 2, "cap {cap}: the destroying frame is stopped");
        assert!(!is_destroyed(&outcome, created), "cap {cap}");
        assert_eq!(
            outcome.gas.state,
            ACCOUNT_LEAF_STATE_GAS_M1 + SLOT_STATE_GAS_M1 + code_state(&runtime),
            "cap {cap}: nothing is given back"
        );
        assert_eq!(outcome.usage.write_records, 3, "cap {cap}: the records stand");
    }
    let limits = mega_evm::EvmTxRuntimeLimits::default()
        .with_block_env_access_compute_gas_limit(2 + 3 + 5_000 + cold + 100);
    let outcome = super::execute_under(funded_contract(code), call_tx(BELOW_THE_CAP), limits);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(is_destroyed(&outcome, created), "within the cap the destruction completes");
    assert_eq!(outcome.gas.state, 0, "and gives the account's state gas back");
}

/// A transaction gas limit below the execution cap: no reservoir, so state gas spills onto
/// regular gas and a frame's regular gas is all that pays it.
const BELOW_THE_CAP: u64 = 50_000_000;

/// `CALL` of `target` with `gas` and one 32-byte word of calldata, `word`.
fn call_with_word_and_gas(
    code: BytecodeBuilder,
    target: Address,
    word: u64,
    gas: u32,
) -> BytecodeBuilder {
    code.mstore(0x200, U256::from(word).to_be_bytes::<32>())
        .append_many([PUSH0, PUSH0])
        .push_number(32_u8)
        .push_number(0x200_u16)
        .append(PUSH0)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

/// After the mark, a fresh `SSTORE` on 60,000 gas, below the transaction's execution cap where
/// no reservoir pays: the store is charged no state gas, so it completes, where the same store of
/// the account before any destruction needs the slot's 97,920 of state gas and runs out of gas.
#[test]
fn test_a_store_after_the_mark_is_not_charged_what_its_frame_cannot_pay() {
    if !pins_state() {
        return;
    }
    const GAS: u32 = 60_000;
    assert!(u64::from(GAS) < SLOT_STATE_GAS_M1);
    let runtime = store_or_destroy(EXISTING);
    let init = init_returning(BytecodeBuilder::default(), &runtime);
    let created = created_at(&init);
    for destroyed in [true, false] {
        let first = if destroyed {
            BytecodeBuilder::default().call(created, U256::ZERO).append(POP)
        } else {
            BytecodeBuilder::default()
        };
        let tail = call_with_word_and_gas(first, created, 7, GAS).append(POP).stop();
        let code = after_create(init.clone(), U256::ZERO, tail);
        let outcome = execute(funded_contract(code), call_tx(BELOW_THE_CAP));
        assert!(outcome.result.is_success(), "destroyed {destroyed}: {:?}", outcome.result);
        let stored = if destroyed { U256::from(7) } else { U256::ZERO };
        assert_eq!(slot_value(&outcome, created, 1), stored, "destroyed {destroyed}");
    }
}

/// Code deposited after its constructor destroyed the account, on gas that cannot pay the
/// deposit's state gas: the address held a balance before the transaction, so its leaf was
/// present and the destruction refills nothing that could pay for the deposit. The deposit is
/// charged no state gas, so the creation succeeds; the same constructor without the destruction
/// runs out of gas on the deposit, and the creation fails.
///
/// The transaction carries a reservoir of 10,000 above the cap and the body: the creating frame's
/// share of it pays the history of the `CREATE2`'s two records, 7,040, which the 2 per cent of its
/// regular gas it keeps could not, and leaves the constructor a share of about 2,700. The creating
/// frame is called with 40,000 of regular gas: once the `CREATE2` took its own, the constructor's
/// share covers the destruction and the deposit's hash, and falls short of the deposit's state gas.
/// The destroying constructor deploys from 30,000 up, and the other fails up to 45,000.
#[test]
fn test_a_deposit_after_the_mark_is_not_charged_what_its_frame_cannot_pay() {
    if !pins_state() {
        return;
    }
    const CREATOR_GAS: u32 = 40_000;
    let runtime = runtime_to(EXISTING);
    assert!(code_state(&runtime) > 30_000);
    let gas = EXECUTION_CAP + BODY_HISTORY + 10_000;
    for destroyed in [true, false] {
        let mut init =
            if destroyed { delegatecall(GRAND).append(POP).build().to_vec() } else { vec![] };
        init.extend_from_slice(&BytecodeBuilder::default().return_with_data(&runtime).build());
        let init = Bytes::from(init);
        let created = CHILD.create2(B256::ZERO, keccak256(&init));
        let creator =
            BytecodeBuilder::default().create2(U256::ZERO, &init, U256::ZERO).return_top().build();
        let code = call_with_gas(BytecodeBuilder::default(), CHILD, U256::ZERO, CREATOR_GAS)
            .append(POP)
            .return_returndata()
            .build()
            .to_vec();
        let mut db = funded_contract(code);
        db.set_account_code(CHILD, creator);
        db.set_account_code(GRAND, runtime.clone());
        db.set_account_balance(created, U256::from(1));
        let outcome = execute(db, call_tx(gas));
        let ExecutionResult::Success { output, .. } = &outcome.result else {
            panic!("destroyed {destroyed}: {:?}", outcome.result);
        };
        let deployed =
            U256::from_be_slice(output.data()) == U256::from_be_slice(created.as_slice());
        assert_eq!(deployed, destroyed, "destroyed {destroyed}: the creation succeeds");
        if destroyed {
            assert!(is_destroyed(&outcome, created));
            assert!(has_code(&outcome, created), "the code was deposited");
            assert_eq!(outcome.gas.state, 0, "and charged no state gas");
        }
    }
}

/// A constructor that destroys its own account through a `DELEGATECALL` to code that
/// self-destructs, and then returns its runtime. The destruction gives the leaf back; the code is
/// deposited after the mark, so its state gas is not kept, while its data size and history are
/// charged as any deposited code's. A later call that runs the runtime, which self-destructs
/// again, takes nothing back.
#[test]
fn test_code_deposited_after_its_constructor_destroyed_the_account_keeps_no_state_gas() {
    if !pins_state() {
        return;
    }
    let runtime = runtime_to(EXISTING);
    let mut prefix = delegatecall(CHILD).append(POP).build().to_vec();
    prefix.extend_from_slice(&BytecodeBuilder::default().return_with_data(&runtime).build());
    let init = Bytes::from(prefix);
    let created = created_at(&init);
    for again in [false, true] {
        let tail = if again {
            BytecodeBuilder::default().call(created, U256::ZERO).append(POP)
        } else {
            BytecodeBuilder::default()
        };
        let code = after_create(init.clone(), U256::ZERO, tail);
        let Some((outcome, probed)) = both(code, |db| db.set_account_code(CHILD, runtime.clone()))
        else {
            return;
        };
        assert!(outcome.result.is_success(), "again {again}: {:?}", outcome.result);
        assert!(is_destroyed(&outcome, created), "again {again}");
        assert!(has_code(&outcome, created), "again {again}: the code was deposited");
        assert_eq!(outcome.gas.state, 0, "again {again}: the leaf came back, the code kept none");
        let kept = BODY_BYTES + WRITE_RECORD_SIZE + runtime.len() as u64;
        assert_eq!(outcome.usage.data_size, kept, "again {again}: the code's bytes stay");
        assert_eq!(outcome.gas.history, history(kept), "again {again}: and their history");
        assert_eq!(outcome.usage.write_records, 1, "again {again}: only the creator's record");
        assert_growth(&probed, 0);
    }
}

/// A creation transaction carrying 1 wei whose constructor writes 999 fresh slots and then
/// self-destructs to an account that does not exist. The created account and its slots use the
/// whole state-growth limit of 1,000, which is the transaction's own frame's budget, and the
/// beneficiary would be a 1,001st unit: the destruction takes back the absent leaf and the 999
/// slots before the beneficiary is held, so the beneficiary fits and the transaction keeps one new
/// account. Under a limit one unit higher the transaction keeps the same: the limit stops nothing.
#[test]
fn test_a_creation_at_its_growth_limit_that_destroys_itself_to_a_new_account_succeeds() {
    let mut init = store_n_prefix(999, 1, false);
    init.extend_from_slice(&runtime_to(BENEFICIARY));
    let Some(gas) = slots_gas(1_000) else { return };
    let tx = || {
        OpTx(op_transaction(TxEnv {
            caller: CALLER,
            kind: TxKind::Create,
            data: Bytes::from(init.clone()),
            value: U256::from(1),
            gas_limit: gas,
            ..Default::default()
        }))
    };
    let created = CALLER.create(0);
    for limit in [TX_STATE_GROWTH_LIMIT, TX_STATE_GROWTH_LIMIT + 1] {
        let limits = mega_evm::EvmTxRuntimeLimits::default().with_tx_state_growth_limit(limit);
        let outcome = super::execute_under(world(&[]), tx(), limits);
        assert!(outcome.result.is_success(), "limit {limit}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "limit {limit}");
        assert!(is_destroyed(&outcome, created), "limit {limit}");
        assert_eq!(balance_of(&outcome, BENEFICIARY), U256::from(1), "limit {limit}");
        assert_eq!(outcome.usage.state_growth, 1, "limit {limit}: the beneficiary alone");
        // The beneficiary's record; the created account's and its slots' went with it.
        assert_eq!(outcome.usage.write_records, 1, "limit {limit}");
        if pins_state() {
            assert_eq!(outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1, "limit {limit}");
        }
    }
}

/// The state gas each `SSTORE` an account ran moved on its frame's net, in execution order.
#[derive(Clone, Debug, Default)]
struct SstoreStateGas {
    armed: Option<(Address, i64)>,
    seen: Vec<(Address, i64)>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for SstoreStateGas {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        if interp.bytecode.opcode() == SSTORE {
            self.armed = Some((interp.input.target_address(), interp.gas.state_gas_spent()));
        }
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<DB>,
    ) {
        if let Some((account, before)) = self.armed.take() {
            self.seen.push((account, interp.gas.state_gas_spent() - before));
        }
    }
}

/// What each `SSTORE` that ran in `account` moved its frame's state gas by, in order, in the
/// plain run [`both`] makes of `scenario`.
fn sstore_state_gas(
    scenario: &[u8],
    setup: impl Fn(&mut MemoryDatabase),
    account: Address,
) -> Vec<i64> {
    let mut code = scenario.to_vec();
    code.push(STOP);
    let mut db = funded_contract(code);
    setup(&mut db);
    let gas = probe_gas(10).expect("the scenario fits a gas limit");
    let mut evm = mega_evm::MegaEvm::new(context(db)).with_inspector(SstoreStateGas::default());
    let outcome = evm.execute_transaction(call_tx(gas)).expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    evm.inspector().seen.iter().filter(|(at, _)| *at == account).map(|(_, moved)| *moved).collect()
}

/// What a frame's gas held just before and just after its `SELFDESTRUCT`.
#[derive(Clone, Debug, Default)]
struct DestructionGas {
    armed: bool,
    /// `(regular remaining, reservoir)` at the opcode, before its static gas.
    before: Option<(u64, u64)>,
    /// `(regular remaining, reservoir)` once the opcode completed.
    after: Option<(u64, u64)>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for DestructionGas {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        if interp.bytecode.opcode() == SELFDESTRUCT {
            self.armed = true;
            self.before = Some((interp.gas.remaining(), interp.gas.reservoir()));
        }
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<EthInterpreter>,
        _context: &mut MegaContext<DB>,
    ) {
        if self.armed {
            self.armed = false;
            self.after = Some((interp.gas.remaining(), interp.gas.reservoir()));
        }
    }
}

/// The transaction is below the execution cap, so the reservoir is empty. The destroying frame
/// stores two fresh slots of its own, which spill 195,840 of state gas and 7,040 of history onto
/// its regular gas, and self-destructs to a warm existing account. The table gives back the leaf
/// and its record, slot 1 and its record from the constructor, the code's state gas and history,
/// and the frame's own two slots and records. The frame's regular gas is credited first, up to
/// the 202,880 still spilled, and the rest goes to its reservoir. Read at the opcode with an
/// inspector. At the transaction's end nothing is left in the reservoir and no state gas stays.
#[test]
fn test_a_give_back_onto_an_empty_reservoir_credits_the_spill_first() {
    if !pins_state() {
        return;
    }
    let runtime = BytecodeBuilder::default()
        .sstore(U256::from(2), U256::from(1))
        .sstore(U256::from(3), U256::from(1))
        .selfdestruct(EXISTING)
        .build();
    let init =
        init_returning(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), &runtime);
    let created = created_at(&init);
    let code = after_create(
        init,
        U256::ZERO,
        BytecodeBuilder::default().call(created, U256::ZERO).append(POP).stop(),
    );
    let spilled = 2 * (SLOT_STATE_GAS_M1 + RECORD_HISTORY);
    assert_eq!(spilled, 202_880);
    let rest = ACCOUNT_LEAF_STATE_GAS_M1 +
        RECORD_HISTORY +
        SLOT_STATE_GAS_M1 +
        RECORD_HISTORY +
        code_state(&runtime) +
        history(runtime.len() as u64);
    let mut evm = mega_evm::MegaEvm::new(context(funded_contract(code)))
        .with_inspector(DestructionGas::default());
    let outcome = evm
        .execute_transaction(warm_call(50_000_000, &[EXISTING]))
        .expect("the transaction is valid");
    let probe = evm.inspector().clone();
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let (before, reservoir_before) = probe.before.expect("the account self-destructed");
    let (after, reservoir_after) = probe.after.expect("the opcode completed");
    assert_eq!(reservoir_before, 0, "the reservoir is empty");
    assert_eq!(after + 5_000, before + spilled, "regular gas is credited up to the spill");
    assert_eq!(reservoir_after, rest, "the rest goes to the reservoir");
    assert_eq!(outcome.gas.state, 0);
    assert_eq!(outcome.gas.reservoir_remaining, 0, "the credit meets the callers' spill too");
}

/// A creation transaction whose constructor destroys its own account. On success the created
/// account's record is given back once, 3,520 at the spec's byte price, and not a second time.
/// When that first frame fails, the table's give-back is discarded and the settlement gives the
/// charge back once.
#[test]
fn test_a_creation_that_destroys_itself_refunds_its_record_once() {
    if !pins_state() {
        return;
    }
    let mut prefix = delegatecall(CHILD).append(POP).build().to_vec();
    prefix.extend_from_slice(&BytecodeBuilder::default().revert().build());
    let init = Bytes::from(prefix);
    let db = world(&[(CHILD, runtime_to(EXISTING))]);
    let Some(gas) = slots_gas(1) else { return };
    let outcome = execute(db, create_tx(init.clone(), gas));
    assert!(
        matches!(outcome.result, ExecutionResult::Revert { .. }),
        "a failing constructor reverts the creation: {:?}",
        outcome.result
    );
    let body = (BODY_BYTES + init.len() as u64) * HISTORY_GAS_PER_BYTE;
    assert_eq!(outcome.gas.history, body, "a failed first frame gives the record back once");
    assert_eq!(outcome.gas.state, 0);

    let init = runtime_to(EXISTING);
    let Some(gas) = slots_gas(1) else { return };
    let db = world(&[(EXISTING, Bytes::from_static(&[0x00]))]);
    let outcome = execute(db, create_tx(init.clone(), gas));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let body = (BODY_BYTES + init.len() as u64) * HISTORY_GAS_PER_BYTE;
    assert_eq!(
        outcome.gas.history, body,
        "giving the created record back removes {RECORD_HISTORY}, once"
    );
    assert_eq!(outcome.gas.history, outcome.gas.history_bytes * HISTORY_GAS_PER_BYTE);
    assert_eq!(outcome.gas.state, 0, "the leaf is given back and the beneficiary already exists");
}
