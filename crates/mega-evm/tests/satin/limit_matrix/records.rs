//! What one slot or one account keeps.
//!
//! A figure pinned below is the page's, at the minimum bucket. Each scenario runs through
//! [`plain_and_probed`]: its figures come from the plain run, and the state-growth net it kept
//! from the probe's budget, which is how a row about the state-growth count observes it.

use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use mega_evm::{test_utils::BytecodeBuilder, MegaTransactionOutcome};
use revm::bytecode::opcode::POP;

use super::{
    assert_growth, at_spec_prices, balance_of, call_with_gas, has_code, nonce_of, plain_and_probed,
    slot_value, world, ACCOUNT_LEAF_STATE_GAS_M1, BODY_BYTES, BODY_HISTORY, CHILD, CONTRACT,
    EXISTING, SLOT_STATE_GAS_M1, TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE,
};
use crate::common::state_is_free;

/// An address no test funds.
const NOBODY: Address = address!("0000000000000000000000000000000000ab102b");

/// Whether a test that pins a spec state-gas figure can run.
fn pins_state() -> bool {
    at_spec_prices() && !state_is_free()
}

/// [`plain_and_probed`] on `world`, with [`CONTRACT`] running `scenario` and `setup` applied.
fn run(
    scenario: Bytes,
    setup: impl Fn(&mut mega_evm::test_utils::MemoryDatabase),
) -> Option<(MegaTransactionOutcome, MegaTransactionOutcome)> {
    plain_and_probed(&scenario, |code| {
        let mut db = world(&[(CONTRACT, code), (EXISTING, Bytes::from_static(&[0x00]))]);
        setup(&mut db);
        db
    })
}

/// Slot `0 → x → 0` in one frame. The second `SSTORE` is the fall row: state gas is refilled,
/// the record is taken back, and the slot's net state growth is 0.
#[test]
fn test_a_slot_set_and_cleared_refills_its_state_gas() {
    if !pins_state() {
        return;
    }
    let code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(1), U256::ZERO)
        .build();
    let Some((outcome, probed)) = run(code, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0, "the fall refills the fresh-slot charge");
    assert_eq!(
        outcome.gas.history, BODY_HISTORY,
        "the taken-back record leaves the body's history"
    );
    assert_eq!(slot_value(&outcome, CONTRACT, 1), U256::ZERO);
    assert_eq!(outcome.usage.write_records, 0, "the record is taken back with the value");
    assert_growth(&probed, 0);
}

/// `CREATE2` onto an address that held a balance before the transaction, nonce 0, no code, and
/// that this transaction has not changed. The creation is valid and the nonce becomes 1. No
/// state-growth rise and no new-account charge. One record for the target, because its nonce
/// differs from what the transaction loaded, and the creator's nonce record apart from it.
///
/// Under a transaction KV limit of 1 the creation's KV budget is `floor(1 × 98 / 100)` = 0 and the
/// target's record stops it, kind 1, `limit` 0, while the creator's bump stands, and the creator
/// keeps its record in the 1 it has once the creation returned.
#[test]
fn test_create2_onto_a_prefunded_address_is_not_a_rise() {
    let init = BytecodeBuilder::default().stop().build();
    let created = CONTRACT.create2(B256::ZERO, keccak256(init.as_ref()));
    let code = || {
        BytecodeBuilder::default().create2(U256::ZERO, init.clone(), U256::ZERO).append(POP).build()
    };
    let setup = |db: &mut mega_evm::test_utils::MemoryDatabase| {
        db.set_account_balance(created, U256::from(1));
    };
    let Some((outcome, probed)) = run(code(), setup) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(nonce_of(&outcome, created), 1, "the creation bumps the nonce");
    assert_eq!(outcome.gas.state, 0, "a funded address is not empty: no new-account charge");
    assert_eq!(outcome.usage.write_records, 2, "the target's record and the creator's");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 2 * WRITE_RECORD_SIZE);
    assert_growth(&probed, 0);

    {
        let mut code = code().to_vec();
        code.extend_from_slice(&BytecodeBuilder::default().return_returndata().build());
        let mut db = world(&[(CONTRACT, Bytes::from(code))]);
        setup(&mut db);
        let limits = mega_evm::EvmTxRuntimeLimits::default().with_tx_kv_update_limit(1);
        let Some(gas) = super::slots_gas(2) else { return };
        let outcome = super::execute_under(db, super::call_tx(gas), limits);
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(
            super::returned_stop(&outcome),
            mega_evm::MegaLimitExceeded { kind: 1, limit: 0 }
        );
        assert_eq!(nonce_of(&outcome, created), 0, "the stopped creation is taken back");
        assert_eq!(nonce_of(&outcome, CONTRACT), 1, "the creator's bump stands");
        assert_eq!(outcome.usage.write_records, 1, "the creator's record");
    }
}

/// `CREATE2` onto an address this transaction funded earlier. The value call created the leaf:
/// +1 and 183,600 at `m` = 1. The address already has its write record, so the creation adds no
/// record, no rise and no second account charge.
#[test]
fn test_create2_onto_an_address_funded_in_this_transaction_charges_one_leaf() {
    if !pins_state() {
        return;
    }
    let init = BytecodeBuilder::default().stop().build();
    let funded = CONTRACT.create2(B256::ZERO, keccak256(init.as_ref()));
    let code = BytecodeBuilder::default()
        .call(funded, U256::from(1))
        .append(POP)
        .create2(U256::ZERO, init, U256::ZERO)
        .append(POP)
        .build();
    let setup = |db: &mut mega_evm::test_utils::MemoryDatabase| {
        db.set_account_balance(CONTRACT, U256::from(10u64.pow(18)));
    };
    let Some((outcome, probed)) = run(code, setup) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.gas.state, ACCOUNT_LEAF_STATE_GAS_M1,
        "the value call is the one leaf; the creation adds none"
    );
    assert_eq!(nonce_of(&outcome, funded), 1);
    assert_eq!(outcome.usage.write_records, 2, "the creator's record and the funded address's");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + 2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE);
    assert_growth(&probed, 1);
}

/// `CREATE2` onto an address that holds code: a collision. No target-side start is kept: the
/// target account, the endowment and its transfer log are not kept. The creator's nonce bump
/// and its record are retained. The collision adds no state growth.
///
/// A collision consumes the regular gas it was forwarded, all but 2 per cent of its creator's,
/// so the creator is a child given [`CREATOR_GAS`]: the transaction's own frame keeps the gas the
/// probe after it runs on.
#[test]
fn test_create2_onto_code_keeps_the_creator_nonce_and_not_the_target() {
    /// Regular gas for the creator: the `CREATE2` and what it forwards to the collision. The
    /// history of the start's records draws the reservoir the creator is given.
    const CREATOR_GAS: u32 = 200_000;
    let init = BytecodeBuilder::default().stop().build();
    let target = CHILD.create2(B256::ZERO, keccak256(init.as_ref()));
    let creator =
        BytecodeBuilder::default().create2(U256::from(1), init, U256::ZERO).append(POP).build();
    let code = call_with_gas(BytecodeBuilder::default(), CHILD, U256::ZERO, CREATOR_GAS)
        .append(POP)
        .build();
    let before = U256::from(7);
    let setup = |db: &mut mega_evm::test_utils::MemoryDatabase| {
        db.set_account_code(CHILD, creator.clone());
        db.set_account_balance(CHILD, U256::from(10u64.pow(18)));
        db.set_account_code(target, Bytes::from_static(&[0x00]));
        db.set_account_balance(target, before);
    };
    let Some((outcome, probed)) = run(code, setup) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(nonce_of(&outcome, CHILD), 1, "the creator nonce bump stands");
    assert_eq!(balance_of(&outcome, target), before, "the target and its endowment are not kept");
    assert!(has_code(&outcome, target), "the target code stays");
    assert_eq!(outcome.gas.state, 0, "a collision is not a rise");
    assert_eq!(outcome.usage.write_records, 1, "the creator's nonce record");
    assert_eq!(outcome.usage.data_size, BODY_BYTES + WRITE_RECORD_SIZE, "no transfer log");
    assert!(outcome.result.logs().is_empty());
    assert_growth(&probed, 0);
}

/// A value `CALL` the caller cannot fund, to an account that does not exist, counts nothing: no
/// record, no rise, no charge. A creation whose creator nonce cannot be bumped counts nothing
/// either, though the address it would create is empty. No limit stops either.
#[test]
fn test_an_unfunded_value_call_and_a_stuck_nonce_count_nothing() {
    let call = BytecodeBuilder::default().call(NOBODY, U256::from(5)).append(POP).build();
    let Some((outcome, probed)) = run(call, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 0, "an unfunded value call writes nothing");
    assert_eq!(outcome.usage.data_size, BODY_BYTES);
    assert_eq!(outcome.gas.state, 0, "and is charged nothing");
    assert_eq!(balance_of(&outcome, NOBODY), U256::ZERO);
    assert!(outcome.limit_exceeded.is_none());
    assert_growth(&probed, 0);

    let init = BytecodeBuilder::default().stop().build();
    let create = BytecodeBuilder::default().create(U256::ZERO, init).append(POP).build();
    let setup = |db: &mut mega_evm::test_utils::MemoryDatabase| {
        db.set_account_nonce(CONTRACT, u64::MAX);
    };
    let Some((outcome, probed)) = run(create, setup) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 0, "a nonce that cannot move writes nothing");
    assert_eq!(outcome.usage.data_size, BODY_BYTES);
    assert_eq!(outcome.gas.state, 0);
    assert_eq!(nonce_of(&outcome, CONTRACT), u64::MAX);
    assert!(outcome.limit_exceeded.is_none());
    assert_growth(&probed, 0);
}

/// `(original 0, present non-zero, new non-zero)` neither rises nor falls: the slot rose once, at
/// its first write. `(original non-zero, present 0, new non-zero)` neither rises nor falls, and
/// charges no fresh-slot state gas and refills none.
#[test]
fn test_a_second_nonzero_write_neither_rises_nor_falls() {
    if !pins_state() {
        return;
    }
    let twice = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(1), U256::from(2))
        .build();
    let Some((outcome, probed)) = run(twice, |_| {}) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, SLOT_STATE_GAS_M1, "0 → x → y rose once");
    assert_growth(&probed, 1);

    let restored = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::ZERO)
        .sstore(U256::from(1), U256::from(1))
        .build();
    let setup = |db: &mut mega_evm::test_utils::MemoryDatabase| {
        db.set_account_storage(CONTRACT, U256::from(1), U256::from(9));
    };
    let Some((outcome, probed)) = run(restored, setup) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0, "a slot that started non-zero charges and refills nothing");
    assert_growth(&probed, 0);
}

/// `0 → 0` changes nothing. `0 → x → x` and `0 → x → y` rose once. `0 → x → 0 → y` rose, fell,
/// and rose again, net +1. A slot whose original value is non-zero changes nothing on every write.
#[test]
fn test_slot_sequences_in_one_frame_rise_once() {
    if !pins_state() {
        return;
    }
    let writes = |values: &[u64]| {
        values
            .iter()
            .fold(BytecodeBuilder::default(), |code, value| {
                code.sstore(U256::from(1), U256::from(*value))
            })
            .build()
    };
    for (values, rises) in [(&[0][..], 0), (&[1, 1][..], 1), (&[1, 2][..], 1), (&[1, 0, 3][..], 1)]
    {
        let Some((outcome, probed)) = run(writes(values), |_| {}) else { return };
        assert!(outcome.result.is_success(), "{values:?}: {:?}", outcome.result);
        assert_eq!(outcome.gas.state, rises * SLOT_STATE_GAS_M1, "{values:?}");
        assert_eq!(outcome.usage.write_records, rises, "{values:?}");
        assert_growth(&probed, rises);
    }

    let setup = |db: &mut mega_evm::test_utils::MemoryDatabase| {
        db.set_account_storage(CONTRACT, U256::from(1), U256::from(8));
    };
    let Some((outcome, probed)) = run(writes(&[4, 0, 5]), setup) else { return };
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0, "an originally non-zero slot changes nothing");
    assert_growth(&probed, 0);
}
