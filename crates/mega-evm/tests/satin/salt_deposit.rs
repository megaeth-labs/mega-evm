//! SALT pricing: where a creation transaction's charge lands when the envelope nonce and the
//! sender's own nonce disagree.
//!
//! A creation transaction pays for the account leaf it adds before its first frame runs, and the
//! bucket that leaf lands in prices the charge. The leaf lands at the address frame creation
//! derives from the sender's account nonce, which is not in general the nonce the envelope
//! carries. An ordinary transaction cannot tell the two apart: its nonce is validated against
//! the account's, so the two numbers are one. A deposit can — op-revm returns from the account
//! load before the nonce check when the transaction is a deposit — so its envelope may say
//! anything while the contract still deploys at `sender.create(account nonce)`.
//!
//! Every test below runs that shape, and an engine pricing the charge at the envelope nonce's
//! address fails all of them but the last: it pays the base price where the leaf actually lands
//! and eight times the price where nothing is written, asks whether the wrong bucket can be
//! priced at all, and settles a reverted creation in a bucket the transaction never deploys
//! into. The last one is the control the two agree on, because equal nonces leave one address
//! to derive.

use alloy_op_evm::OpTx;
use alloy_primitives::{Address, Bytes, TxKind, B256};
use mega_evm::{
    test_utils::{op_transaction, MemoryDatabase},
    MegaTransaction,
};
use revm::{
    bytecode::opcode::{PUSH0, RETURN, REVERT},
    context::{result::ExecutionResult, TxEnv},
    context_interface::cfg::GasId,
};

use crate::{
    common::state_is_free,
    salt::{
        account_bucket, crowded_account, db, entry, minimal_envs, run, try_run, SaltEnvs, CALLER,
        GAS_LIMIT,
    },
};

/// The sender's own nonce. The contract deploys at `CALLER.create(STATE_NONCE)`.
const STATE_NONCE: u64 = 5;

/// The nonce the deposit's envelope carries, which op-revm never compares to the account's.
const ENVELOPE_NONCE: u64 = 0;

/// The capacity, in minimum buckets, every crowded arm puts its bucket at.
const M: u64 = 8;

/// Init code deploying zero bytes of runtime code, so the creation's only state charge is the
/// account leaf itself.
const EMPTY_INIT: &[u8] = &[PUSH0, PUSH0, RETURN];

/// Init code that reverts, so the creation adds no leaf and its upfront charge comes back.
const REVERTING_INIT: &[u8] = &[PUSH0, PUSH0, REVERT];

/// The address the transaction deploys at, and the one its envelope nonce derives, with the
/// guard the tests rest on: the two fall in different buckets, so crowding one says nothing
/// about the other.
fn candidates() -> (Address, Address) {
    let deployed = CALLER.create(STATE_NONCE);
    let envelope = CALLER.create(ENVELOPE_NONCE);
    assert_ne!(
        account_bucket(deployed),
        account_bucket(envelope),
        "the probes tell the two sites apart by the bucket each reads",
    );
    (deployed, envelope)
}

/// A sender whose account nonce is `nonce`.
fn sender_at(nonce: u64) -> MemoryDatabase {
    db(Bytes::new()).account_nonce(CALLER, nonce)
}

/// A creation transaction from `CALLER` carrying `nonce` in its envelope, deploying `init_code`.
///
/// `deposit` gives the transaction a source hash, which is what makes op-revm classify it as a
/// deposit and skip the nonce check; it is not the sequencer's own system transaction, so it is
/// priced like any transaction a user sends.
fn create_tx(nonce: u64, init_code: &[u8], deposit: bool) -> MegaTransaction {
    let mut tx = op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        nonce,
        data: Bytes::copy_from_slice(init_code),
        gas_limit: GAS_LIMIT,
        ..Default::default()
    });
    if deposit {
        tx.deposit.source_hash = B256::from([0x42; 32]);
        tx.deposit.is_system_transaction = false;
    }
    OpTx(tx)
}

/// What one account leaf costs at the minimum bucket.
fn leaf() -> u64 {
    entry(GasId::create_state_gas())
}

/// `envs` with the capacity query of `account`'s own bucket failing.
fn failing_account(envs: SaltEnvs, account: Address) -> SaltEnvs {
    envs.with_failing_bucket(account_bucket(account), "salt backend unreachable".into())
}

/// A deposit creation is charged in the bucket of the address it deploys at, and the address its
/// envelope nonce derives is never priced, never written and never read.
#[test]
fn test_a_deposit_creation_is_priced_where_it_deploys() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let (deployed, envelope) = candidates();
    let envs = crowded_account(minimal_envs(), deployed, M);

    let outcome =
        run(sender_at(STATE_NONCE), envs.clone(), create_tx(ENVELOPE_NONCE, EMPTY_INIT, true));

    assert_eq!(outcome.gas.state, leaf() * M, "the deployed address's bucket prices the leaf");
    assert_eq!(envs.bucket_queries(account_bucket(deployed)), 1, "read once, for that charge");
    assert_eq!(
        envs.bucket_queries(account_bucket(envelope)),
        0,
        "the envelope nonce's address is nobody's charge site",
    );

    // The state says the same thing the price does: the leaf is at the deployed address, the
    // sender's nonce moved off the value that derived it, and the envelope's address is absent.
    assert!(outcome.state.contains_key(&deployed), "the created account is at {deployed}");
    assert!(!outcome.state.contains_key(&envelope), "nothing was written at {envelope}");
    let sender = outcome.state.get(&CALLER).expect("the sender is in the state");
    assert_eq!(sender.info.nonce, STATE_NONCE + 1, "the creation bumped the account's own nonce");
}

/// Crowding the address the envelope nonce derives changes nothing: the transaction pays the
/// base price, and that bucket's capacity is never asked for.
#[test]
fn test_crowding_the_envelope_nonce_s_address_changes_no_price() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let (deployed, envelope) = candidates();
    let envs = crowded_account(minimal_envs(), envelope, M);

    let outcome =
        run(sender_at(STATE_NONCE), envs.clone(), create_tx(ENVELOPE_NONCE, EMPTY_INIT, true));

    assert_eq!(outcome.gas.state, leaf(), "the crowd sits where the transaction writes nothing");
    assert_eq!(envs.bucket_queries(account_bucket(envelope)), 0, "so it is never read");
    assert_eq!(envs.bucket_queries(account_bucket(deployed)), 1, "the leaf's own bucket is");
}

/// A capacity the deployed address's bucket cannot report fails the transaction with the cause
/// the SALT environment gave, like any other charge that cannot be priced.
#[test]
fn test_an_unpriceable_deposit_creation_fails_with_its_cause() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let (deployed, _) = candidates();
    let envs = failing_account(minimal_envs(), deployed);

    match try_run(sender_at(STATE_NONCE), envs, create_tx(ENVELOPE_NONCE, EMPTY_INIT, true)) {
        Err(revm::context::result::EVMError::Custom(message)) => {
            assert!(message.contains("salt backend unreachable"), "got {message:?}");
        }
        other => panic!("expected the recorded cause, got {other:?}"),
    }
}

/// The same failure at the envelope nonce's address does not reach the transaction at all: a
/// bucket nothing is charged in is a bucket nothing asks about.
#[test]
fn test_a_failure_at_the_envelope_nonce_s_address_is_never_asked_for() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let (deployed, envelope) = candidates();
    let envs = failing_account(minimal_envs(), envelope);

    let outcome =
        run(sender_at(STATE_NONCE), envs.clone(), create_tx(ENVELOPE_NONCE, EMPTY_INIT, true));

    assert_eq!(outcome.gas.state, leaf(), "the transaction is priced by its own site");
    assert_eq!(envs.bucket_queries(account_bucket(envelope)), 0);
    assert_eq!(envs.bucket_queries(account_bucket(deployed)), 1);
}

/// A deposit creation whose init code reverts adds no leaf, and the refund is made at the site
/// the charge was: the transaction nets nothing on the state ledger even where that site is
/// eight times the minimum, and one capacity read priced both halves.
#[test]
fn test_a_reverting_deposit_creation_gives_the_charge_back_where_it_was_made() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    let (deployed, envelope) = candidates();
    let envs = crowded_account(minimal_envs(), deployed, M);

    let outcome = try_run(
        sender_at(STATE_NONCE),
        envs.clone(),
        create_tx(ENVELOPE_NONCE, REVERTING_INIT, true),
    )
    .expect("the probe is a valid transaction");

    assert!(
        matches!(outcome.result, ExecutionResult::Revert { .. }),
        "the init code reverts: {:?}",
        outcome.result,
    );
    assert_eq!(outcome.gas.state, 0, "a creation that deployed nothing pays no state gas");
    assert_eq!(envs.bucket_queries(account_bucket(deployed)), 1, "charge and refill, one read");
    assert_eq!(envs.bucket_queries(account_bucket(envelope)), 0);
    assert!(!outcome.state.contains_key(&envelope), "nothing was written at {envelope}");
}

/// The control: with the two nonces equal there is one address to derive, and a deposit and an
/// ordinary transaction are priced alike there. This is every other creation transaction in the
/// suite, and the site it is charged at did not move.
#[test]
fn test_at_equal_nonces_a_deposit_and_an_ordinary_creation_agree() {
    // A charge that costs nothing costs nothing at any capacity, and the engine asks for none.
    if state_is_free() {
        return;
    }
    // Both nonces are `STATE_NONCE` here, so the two candidate addresses of the tests above
    // are this one address, and no arrangement of buckets can tell them apart.
    let deployed = CALLER.create(STATE_NONCE);

    let mut ledgers = Vec::new();
    for deposit in [false, true] {
        let envs = crowded_account(minimal_envs(), deployed, M);
        let outcome =
            run(sender_at(STATE_NONCE), envs.clone(), create_tx(STATE_NONCE, EMPTY_INIT, deposit));

        assert_eq!(outcome.gas.state, leaf() * M, "deposit = {deposit}: the crowded leaf");
        assert_eq!(envs.bucket_queries(account_bucket(deployed)), 1, "deposit = {deposit}");
        assert!(outcome.state.contains_key(&deployed), "deposit = {deposit}: the leaf is here");
        ledgers.push(outcome.gas.state);
    }
    assert_eq!(ledgers[0], ledgers[1], "the envelope's shape prices nothing on its own");
}
