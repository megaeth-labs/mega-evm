//! SALT pricing: what happens when a state gas charge cannot be priced at all.
//!
//! The pricing hook reports a failed capacity lookup the way `Host::sload` reports a failed
//! database read: it returns nothing and records the cause in the context error. Every charge
//! site turns that into a bail-out, and the transaction surfaces the recorded cause as an error
//! rather than as a halt — a transaction that could not afford its state gas and one whose price
//! could not be looked up must not look alike, and neither may be settled at a price nobody
//! chose.

use alloy_primitives::{Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaEvm, MegaTransaction,
};
use revm::{
    bytecode::opcode::{PUSH0, RETURN},
    context::result::EVMError,
    context_interface::cfg::GasId,
};

use crate::salt::{
    account_bucket, authorization_tx, call_contract, create_empty_contract, crowded_account, db,
    entry, minimal_envs, run, salt_context, slot_bucket, try_run, tx, value_call, SaltEnvs,
    AUTHORITY, CALLER, CONTRACT, EMPTY,
};

/// What a broken SALT backend reports.
const UNREACHABLE: &str = "salt backend unreachable";

/// A slot every probe that writes storage uses.
const SLOT: u64 = 7;

/// `envs` with the capacity query of `account`'s own bucket failing.
fn failing_account(envs: SaltEnvs, account: Address) -> SaltEnvs {
    envs.with_failing_bucket(account_bucket(account), UNREACHABLE.into())
}

/// `envs` with the capacity query of the slot `key` of `address` failing.
fn failing_slot(envs: SaltEnvs, address: Address, key: U256) -> SaltEnvs {
    envs.with_failing_bucket(slot_bucket(address, key), UNREACHABLE.into())
}

/// Runs the probe and requires it to fail with the cause the SALT environment reported.
fn assert_fails(site: &str, db: MemoryDatabase, envs: SaltEnvs, tx: MegaTransaction) {
    match try_run(db, envs, tx) {
        Err(EVMError::Custom(message)) => assert!(
            message.contains(UNREACHABLE),
            "{site}: the recorded cause must reach the caller, got {message:?}",
        ),
        Err(other) => panic!("{site}: expected the recorded cause, got {other:?}"),
        Ok(outcome) => panic!("{site}: expected an error, got {:?}", outcome.result),
    }
}

/// An `SSTORE` onto a slot whose bucket cannot be read fails the transaction.
#[test]
fn test_an_unpriceable_sstore_set_fails_the_transaction() {
    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    assert_fails(
        "SSTORE set",
        db(code),
        failing_slot(minimal_envs(), CONTRACT, U256::from(SLOT)),
        call_contract(),
    );
}

/// So does a `CALL` carrying value to an account whose bucket cannot be read.
#[test]
fn test_an_unpriceable_new_account_call_fails_the_transaction() {
    assert_fails(
        "CALL to a new account",
        db(value_call(EMPTY).stop().build()),
        failing_account(minimal_envs(), EMPTY),
        call_contract(),
    );
}

/// And a `CREATE` whose deployment address falls in a bucket that cannot be read.
#[test]
fn test_an_unpriceable_create_fails_the_transaction() {
    assert_fails(
        "CREATE",
        db(create_empty_contract().stop().build()),
        failing_account(minimal_envs(), CONTRACT.create(0)),
        call_contract(),
    );
}

/// A charge the EIP-2780 runtime phase makes before the first frame fails the transaction too:
/// that phase reports a real shortfall as an out-of-gas halt, and an unpriceable charge must not
/// be dressed up as one.
#[test]
fn test_an_unpriceable_top_level_recipient_fails_the_transaction() {
    assert_fails(
        "the transaction's recipient",
        db(Bytes::new()),
        failing_account(minimal_envs(), EMPTY),
        tx(TxKind::Call(EMPTY), Bytes::new(), U256::from(1)),
    );
}

/// The create target of a creation transaction is charged in the same phase.
#[test]
fn test_an_unpriceable_create_transaction_target_fails_the_transaction() {
    assert_fails(
        "a creation transaction's target",
        db(Bytes::new()),
        failing_account(minimal_envs(), CALLER.create(0)),
        tx(TxKind::Create, Bytes::from_static(&[PUSH0, PUSH0, RETURN]), U256::ZERO),
    );
}

/// An EIP-7702 authorization is applied before any frame runs, and its charges go through the
/// same hook.
#[test]
fn test_an_unpriceable_eip7702_authority_fails_the_transaction() {
    assert_fails(
        "an EIP-7702 authority",
        db(Bytes::new()),
        failing_account(minimal_envs(), AUTHORITY),
        authorization_tx(CONTRACT),
    );
}

/// The code deposit is priced through the hook as well. The creation itself pays nothing here —
/// the deployment address already holds a balance, so it is not a new account leaf — which
/// leaves the deposit as the only charge in that bucket.
#[test]
fn test_an_unpriceable_code_deposit_fails_the_transaction() {
    let created = CALLER.create(0);
    let db = db(Bytes::new()).account_balance(created, U256::from(1));
    let init = BytecodeBuilder::default().push_number(32u64).append_many([PUSH0, RETURN]).build();

    // The creation charge is skipped for an address that already exists, so a run with the
    // bucket readable must still draw the per-byte deposit charge — otherwise the failure below
    // would be pinning nothing.
    let deposited = run(
        db.clone(),
        crowded_account(minimal_envs(), created, 1),
        tx(TxKind::Create, init.clone(), U256::ZERO),
    );
    assert_eq!(
        deposited.gas.state,
        entry(GasId::code_deposit_state_gas()) * 32,
        "the deposit is the only state charge of this probe",
    );

    assert_fails(
        "a code deposit",
        db,
        failing_account(minimal_envs(), created),
        tx(TxKind::Create, init, U256::ZERO),
    );
}

/// A program that never charges state gas never asks for a price, so a SALT environment that
/// cannot answer does not fail it.
#[test]
fn test_a_program_that_charges_no_state_gas_is_unaffected() {
    let envs = failing_slot(minimal_envs(), CONTRACT, U256::from(SLOT));
    let envs = failing_account(envs, CONTRACT);
    let envs = failing_account(envs, CALLER);
    let code = BytecodeBuilder::default().stop().build();

    let outcome = try_run(db(code), envs, call_contract()).expect("nothing was priced");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0);
}

/// A failed lookup inside a frame that goes on to revert still fails the transaction: the cause
/// is recorded on the context, which the frame's own rollback does not undo.
#[test]
fn test_an_unpriceable_charge_in_a_reverting_frame_still_fails_the_transaction() {
    let code = BytecodeBuilder::default()
        .sstore(U256::from(SLOT), U256::from(1))
        .revert_with_data([0xaa])
        .build();
    assert_fails(
        "a reverting frame",
        db(code),
        failing_slot(minimal_envs(), CONTRACT, U256::from(SLOT)),
        call_contract(),
    );
}

/// A system-originated transaction is priced at the minimum bucket without reading SALT, so a
/// SALT environment that cannot answer does not stop the protocol's own work.
#[test]
fn test_a_system_call_is_unaffected_by_an_unreadable_bucket() {
    use alloy_eips::eip4788::SYSTEM_ADDRESS;
    use alloy_evm::Evm;

    let code = BytecodeBuilder::default().sstore(U256::from(SLOT), U256::from(1)).stop().build();
    let envs = failing_slot(minimal_envs(), CONTRACT, U256::from(SLOT));

    let result = MegaEvm::new(salt_context(db(code), envs))
        .transact_system_call(SYSTEM_ADDRESS, CONTRACT, Bytes::new())
        .expect("a system call does not read SALT");
    assert!(result.result.is_success(), "{:?}", result.result);
}
