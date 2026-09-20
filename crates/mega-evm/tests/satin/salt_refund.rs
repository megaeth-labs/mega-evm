//! SALT pricing: giving a state gas charge back at the price it was made at.
//!
//! A state charge is undone at three kinds of moment, and all three re-price the charge that was
//! made rather than looking the number up again from somewhere else:
//!
//! - the same opcode takes its own write back (`SSTORE` 0 -> x -> 0),
//! - a frame that made a charge, or a frame whose child did not create the leaf the charge paid
//!   for, does not survive, and
//! - the transaction itself reverts.
//!
//! Because the site is re-priced through the one hook, and because the multiplier of a bucket is
//! read once per transaction, the refill is the charge — a bucket cannot be read at one capacity
//! for the charge and another for the refill. These tests pin that: what is fully undone nets
//! zero at every multiplier, what is kept scales, and the environment is asked once.
//!
//! Against that, an applied EIP-7702 authorization is *not* undone by a frame that reverts
//! later: the delegation it wrote survives the revert, so the state gas it paid must survive it
//! too.

use alloy_primitives::{Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaEvm, MegaTransaction, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{CALL, PUSH0, RETURN, REVERT},
    context_interface::cfg::GasId,
};

use crate::salt::{
    authorization_tx, call_contract, capacity, create_with, crowded_account, crowded_slot, db,
    entry, minimal_envs, run, salt_context, slot_bucket, try_run, tx, SaltEnvs, AUTHORITY, CALLER,
    CONTRACT, EMPTY, GAS_LIMIT,
};

/// The contract a probe's inner frame runs in.
const SUB: Address = alloy_primitives::address!("0000000000000000000000000000000000c00005");

/// The slot every probe that writes storage uses.
const SLOT: u64 = 7;

/// Runs `probe` at each multiplier and returns the state gas it netted, requiring the regular
/// ledger to be the same at all of them.
fn net_state_at(
    ms: [u64; 3],
    crowd: impl Fn(SaltEnvs, u64) -> SaltEnvs,
    probe: impl Fn() -> (MemoryDatabase, MegaTransaction),
) -> [u64; 3] {
    let mut states = [0; 3];
    let mut regular = None;
    for (i, m) in ms.into_iter().enumerate() {
        let (db, tx) = probe();
        let outcome = try_run(db, crowd(minimal_envs(), m), tx).expect("the probe is valid");
        states[i] = outcome.gas.state;
        let previous = *regular.get_or_insert(outcome.gas.regular);
        assert_eq!(outcome.gas.regular, previous, "regular gas must not move with m");
    }
    states
}

/// `SSTORE` onto a fresh slot and straight back to zero: the restore refills exactly what the
/// set charged, so the transaction nets nothing on the state ledger however crowded the bucket
/// is — and the bucket is read once for both.
#[test]
fn test_a_slot_written_and_restored_nets_nothing_at_every_multiplier() {
    let code = || {
        BytecodeBuilder::default()
            .sstore(U256::from(SLOT), U256::from(1))
            .sstore(U256::from(SLOT), U256::ZERO)
            .stop()
            .build()
    };
    let crowd = |envs, m| crowded_slot(envs, CONTRACT, U256::from(SLOT), m);

    assert_eq!(
        net_state_at([1, 2, 8], crowd, || (db(code()), call_contract())),
        [0, 0, 0],
        "the restore gives back what the set charged",
    );

    let envs = crowd(minimal_envs(), 8);
    let outcome = run(db(code()), envs.clone(), call_contract());
    assert_eq!(outcome.gas.state, 0);
    assert_eq!(
        envs.bucket_queries(slot_bucket(CONTRACT, U256::from(SLOT))),
        1,
        "one capacity read priced both the charge and the refill",
    );
}

/// A creation whose init code reverts: the `CREATE` opcode charged its caller for the account
/// leaf upfront, and the frame that did not create it gives the charge back at the same price.
#[test]
fn test_a_reverting_creation_gives_its_upfront_charge_back() {
    let created = CONTRACT.create(0);
    let probe = || {
        // Init code `PUSH0 PUSH0 REVERT`: the creation fails, so no account leaf is added.
        let code = create_with(&[PUSH0, PUSH0, REVERT]).stop().build();
        (db(code), call_contract())
    };

    assert_eq!(
        net_state_at([1, 2, 8], move |envs, m| crowded_account(envs, created, m), probe),
        [0, 0, 0],
        "a creation that deployed nothing pays no state gas",
    );
}

/// A `CALL` that charged for a new account leaf and then failed gives the charge back: the
/// caller cannot afford the transfer, so no account is created.
#[test]
fn test_a_failed_value_call_gives_its_new_account_charge_back() {
    let probe = || {
        let code = BytecodeBuilder::default()
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(1u64)
            .push_address(EMPTY)
            .push_number(1_000_000u64)
            .append(CALL)
            .stop()
            .build();
        // The contract holds nothing, so the transfer the CALL attempts cannot be made.
        let db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10u64.pow(18)))
            .account_code(CONTRACT, code);
        (db, call_contract())
    };

    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_account(envs, EMPTY, m), probe),
        [0, 0, 0],
        "no account was created, so the upfront charge comes back",
    );
}

/// A charge made two frames deep and rolled back with its frame comes back whole: the inner
/// frame wrote a slot in a crowded bucket and reverted.
#[test]
fn test_a_charge_in_a_reverting_inner_frame_comes_back_whole() {
    let probe = || {
        let sub = BytecodeBuilder::default()
            .sstore(U256::from(SLOT), U256::from(1))
            .revert_with_data([0xaa])
            .build();
        let code = BytecodeBuilder::default()
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_address(SUB)
            .push_number(2_000_000u64)
            .append(CALL)
            .stop()
            .build();
        (db(code).account_code(SUB, sub), call_contract())
    };

    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_slot(envs, SUB, U256::from(SLOT), m), probe),
        [0, 0, 0],
        "the frame's rollback takes its state charge with it",
    );
}

/// What an inner frame keeps still scales: the same program with the inner frame succeeding
/// pays the crowded price, so the arms above are not passing because nothing was charged.
#[test]
fn test_a_charge_an_inner_frame_keeps_scales_with_its_bucket() {
    let probe = || {
        let sub = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
        let code = BytecodeBuilder::default()
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_number(0u64)
            .push_address(SUB)
            .push_number(2_000_000u64)
            .append(CALL)
            .stop()
            .build();
        (db(code).account_code(SUB, sub), call_contract())
    };

    let set = entry(GasId::sstore_set_state_gas());
    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_slot(envs, SUB, U256::from(SLOT), m), probe),
        [set, set * 2, set * 8],
    );
}

/// The whole transaction reverting is the same story one level up: the outermost frame's charge
/// goes back with it.
#[test]
fn test_a_transaction_that_reverts_pays_no_state_gas() {
    let probe = || {
        let code = BytecodeBuilder::default()
            .sstore(U256::from(SLOT), U256::from(1))
            .revert_with_data([0xaa])
            .build();
        (db(code), call_contract())
    };

    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_slot(envs, CONTRACT, U256::from(SLOT), m), probe),
        [0, 0, 0],
    );
}

/// An applied EIP-7702 authorization is not a frame's doing: the delegation it wrote outlives a
/// frame that reverts afterwards, and so does the state gas it paid.
#[test]
fn test_an_applied_authorization_keeps_its_charge_through_a_revert() {
    let authority_charges =
        entry(GasId::new_account_state_gas()) + entry(GasId::tx_eip7702_state_gas_bytecode());
    let probe = || {
        let code = BytecodeBuilder::default().revert_with_data([0xaa]).build();
        (db(code), authorization_tx(CONTRACT))
    };

    assert_eq!(
        net_state_at([1, 2, 8], |envs, m| crowded_account(envs, AUTHORITY, m), probe),
        [authority_charges, authority_charges * 2, authority_charges * 8],
        "the authority's leaf and its delegation bytes are still there after the revert",
    );

    // And the delegation really did survive: the authority carries the designation.
    let outcome = try_run(
        db(BytecodeBuilder::default().revert_with_data([0xaa]).build()),
        crowded_account(minimal_envs(), AUTHORITY, 8),
        authorization_tx(CONTRACT),
    )
    .expect("the transaction is valid");
    assert!(
        outcome.state.get(&AUTHORITY).is_some_and(|account| account
            .info
            .code
            .as_ref()
            .is_some_and(|code| !code.is_empty())),
        "the authority kept its delegation",
    );
}

/// A creation whose deposit is refused gives back everything it charged: the account leaf it
/// paid for upfront and the bytes it never deposited.
#[test]
fn test_a_refused_code_deposit_pays_no_state_gas() {
    let created = CALLER.create(0);
    let probe = || {
        // Init code returning a single `0xEF` byte, which EIP-3541 refuses to deploy.
        let init = BytecodeBuilder::default()
            .mstore(0, [0xefu8])
            .push_number(1u64)
            .push_number(0u64)
            .append(RETURN)
            .build();
        (db(Bytes::new()), tx(TxKind::Create, init, U256::ZERO))
    };

    assert_eq!(
        net_state_at([1, 2, 8], move |envs, m| crowded_account(envs, created, m), probe),
        [0, 0, 0],
        "nothing was deployed, so nothing is owed",
    );
}

/// A charge kept beside one given back: the kept one scales, the other nets out, and the
/// transaction pays exactly the difference.
#[test]
fn test_a_kept_charge_and_a_restored_one_settle_independently() {
    const KEPT: u64 = 3;
    let probe = || {
        let code = BytecodeBuilder::default()
            .sstore(U256::from(KEPT), U256::from(1))
            .sstore(U256::from(SLOT), U256::from(1))
            .sstore(U256::from(SLOT), U256::ZERO)
            .stop()
            .build();
        (db(code), call_contract())
    };
    let crowd = |envs: SaltEnvs, m: u64| {
        let envs = crowded_slot(envs, CONTRACT, U256::from(KEPT), m);
        crowded_slot(envs, CONTRACT, U256::from(SLOT), m)
    };

    let set = entry(GasId::sstore_set_state_gas());
    assert_eq!(net_state_at([1, 2, 8], crowd, probe), [set, set * 2, set * 8]);
}

/// A crowded bucket cannot make a transaction owe more than it charged: the refill of a charge
/// is the charge, so a restore never leaves the state ledger negative or the reservoir richer
/// than the sender's budget.
#[test]
fn test_a_restore_never_gives_back_more_than_it_charged() {
    let code = BytecodeBuilder::default()
        .sstore(U256::from(SLOT), U256::from(1))
        .sstore(U256::from(SLOT), U256::ZERO)
        .stop()
        .build();
    let envs = crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8);

    let outcome: MegaTransactionOutcome =
        MegaEvm::new(salt_context(db(code), envs)).execute_transaction(call_contract()).unwrap();

    assert_eq!(outcome.gas.state, 0, "the state ledger nets to zero, not below it");
    assert!(
        outcome.gas.reservoir_remaining <= GAS_LIMIT,
        "a refill cannot put more in the reservoir than the transaction brought",
    );
    assert!(outcome.gas.gas_used > 0 && outcome.gas.gas_used <= GAS_LIMIT);
}

/// The whole matrix at one multiplier, side by side, so the three refund classes are readable in
/// one place: a rollback gives the charge back, an authorization keeps it, and a bucket is read
/// once whatever happens to the charge afterwards.
#[test]
fn test_the_refund_classes_side_by_side() {
    let set = entry(GasId::sstore_set_state_gas());
    let authority_charges =
        entry(GasId::new_account_state_gas()) + entry(GasId::tx_eip7702_state_gas_bytecode());

    let kept = run(
        db(BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build()),
        crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8),
        call_contract(),
    );
    assert_eq!(kept.gas.state, set * 8, "a write that stays pays the crowded price");

    let rolled_back = try_run(
        db(BytecodeBuilder::default()
            .sstore(U256::from(SLOT), U256::from(1))
            .revert_with_data([0xaa])
            .build()),
        crowded_slot(minimal_envs(), CONTRACT, U256::from(SLOT), 8),
        call_contract(),
    )
    .expect("the transaction is valid");
    assert_eq!(rolled_back.gas.state, 0, "a write that is rolled back pays nothing");

    let authorized = try_run(
        db(BytecodeBuilder::default().revert_with_data([0xaa]).build()),
        crowded_account(minimal_envs(), AUTHORITY, 8),
        authorization_tx(CONTRACT),
    )
    .expect("the transaction is valid");
    assert_eq!(
        authorized.gas.state,
        authority_charges * 8,
        "an applied authorization pays even though the frame reverted",
    );
}

/// The capacity a bucket is read at is fixed for the transaction: one read serves the charge and
/// the refill, so the two cannot disagree even if the environment's answer would.
#[test]
fn test_the_capacity_behind_a_charge_and_its_refill_is_read_once() {
    let bucket = slot_bucket(CONTRACT, U256::from(SLOT));
    let envs = minimal_envs().with_bucket_capacity(bucket, capacity(8));
    let code = BytecodeBuilder::default()
        .sstore(U256::from(SLOT), U256::from(1))
        .sstore(U256::from(SLOT), U256::ZERO)
        .sstore(U256::from(SLOT), U256::from(2))
        .sstore(U256::from(SLOT), U256::ZERO)
        .stop()
        .build();

    let outcome = run(db(code), envs.clone(), call_contract());

    assert_eq!(outcome.gas.state, 0, "every set was restored");
    assert_eq!(envs.bucket_queries(bucket), 1, "two charges and two refills, one capacity read");
}
