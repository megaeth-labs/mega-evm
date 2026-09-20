//! SALT pricing across an EIP-7702 delegation, and across the call kinds that do not add state.
//!
//! Two things a state gas charge must get right when the target of a call carries a delegation
//! designator:
//!
//! - the target is an account that *exists*, so reaching it with value adds no leaf and costs no
//!   state gas, however crowded its bucket is; and
//! - deciding that must not follow the designator further than the one hop EIP-7702 allows. A
//!   designator can point at itself, or two of them at each other; resolving such a chain to a
//!   fixed point does not terminate. These probes reach a cycle through each call-family opcode and
//!   through both creation opcodes, and require the transaction to settle.
//!
//! `CALLCODE` is here for the same reason from the other side: it runs the callee's code in the
//! caller's own account and sends value to that account, so it adds no leaf either, whatever the
//! callee looks like.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaTransaction, MegaTransactionOutcome,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLCODE, CREATE, CREATE2, DELEGATECALL, GAS, PUSH0, RETURN, STATICCALL, STOP,
    },
    context_interface::cfg::GasId,
    database::AccountState,
    state::Bytecode,
};

use crate::salt::{
    account_bucket, call_contract, crowded_account, db, entry, minimal_envs, run, try_run,
    SaltEnvs, EMPTY,
};

/// An account whose code is a delegation designator pointing at itself.
const SELF_DELEGATING: Address = address!("0000000000000000000000000000000000c00010");
/// One half of a two-address delegation cycle.
const CYCLE_A: Address = address!("0000000000000000000000000000000000c00011");
/// The other half.
const CYCLE_B: Address = address!("0000000000000000000000000000000000c00012");

/// Writes the `0xef0100 || address` designator an applied EIP-7702 authorization leaves behind.
fn with_delegation(mut db: MemoryDatabase, address: Address, to: Address) -> MemoryDatabase {
    let bytecode = Bytecode::new_eip7702(to);
    let code_hash = bytecode.hash_slow();
    let account = db.load_account(address).expect("the account is in memory");
    account.info.code = Some(bytecode);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
    db
}

/// `CALL(gas, target, 0, 0, 0, 0, 0)` followed by `STOP`.
fn calls(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .append(PUSH0)
        .append(PUSH0)
        .append(PUSH0)
        .append(PUSH0)
        .append(PUSH0)
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(STOP)
        .build()
}

/// The three call kinds that take no value operand, plus `CALLCODE`, against `target`.
fn calls_with(opcode: u8, target: Address) -> Bytes {
    let mut code =
        BytecodeBuilder::default().append(PUSH0).append(PUSH0).append(PUSH0).append(PUSH0);
    if opcode == CALLCODE {
        code = code.append(PUSH0);
    }
    code.push_address(target).append(GAS).append(opcode).append(STOP).build()
}

/// `CREATE`, or `CREATE2` with salt zero, of an init code that deploys nothing.
fn creates(opcode: u8) -> Bytes {
    let init: [u8; 3] = [PUSH0, PUSH0, RETURN];
    let mut code = BytecodeBuilder::default().mstore(0, init);
    if opcode == CREATE2 {
        code = code.push_number(0u64);
    }
    code.push_number(init.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(opcode)
        .append(STOP)
        .build()
}

/// Requires the probe to settle into a receipt at all. A delegation chain resolved to a fixed
/// point would not return here — the test would not fail, it would never finish — so what this
/// asserts is that the engine followed the designator the one hop EIP-7702 allows and no further.
fn assert_settles(what: &str, db: MemoryDatabase, tx: MegaTransaction) -> MegaTransactionOutcome {
    let outcome = try_run(db, minimal_envs(), tx).unwrap_or_else(|e| panic!("{what}: {e:?}"));
    assert!(outcome.gas.gas_used > 0, "{what}: the transaction produced no receipt");
    outcome
}

/// The outer frame of a probe stops after the call it makes, so the transaction succeeds
/// whatever the frame it called did.
fn assert_outer_frame_succeeds(what: &str, db: MemoryDatabase, tx: MegaTransaction) {
    let outcome = assert_settles(what, db, tx);
    assert!(outcome.result.is_success(), "{what}: {:?}", outcome.result);
}

/* Delegation cycles reached through each opcode that prices a state charge. */

#[test]
fn test_a_call_to_a_self_delegating_account_settles() {
    let db = with_delegation(db(calls(SELF_DELEGATING)), SELF_DELEGATING, SELF_DELEGATING);
    assert_outer_frame_succeeds("CALL", db, call_contract());
}

#[test]
fn test_a_call_into_a_two_address_delegation_cycle_settles() {
    let db = with_delegation(db(calls(CYCLE_A)), CYCLE_A, CYCLE_B);
    let db = with_delegation(db, CYCLE_B, CYCLE_A);
    assert_outer_frame_succeeds("CALL into a cycle", db, call_contract());
}

#[test]
fn test_a_staticcall_to_a_self_delegating_account_settles() {
    let db = with_delegation(
        db(calls_with(STATICCALL, SELF_DELEGATING)),
        SELF_DELEGATING,
        SELF_DELEGATING,
    );
    assert_outer_frame_succeeds("STATICCALL", db, call_contract());
}

#[test]
fn test_a_delegatecall_into_a_two_address_delegation_cycle_settles() {
    let db = with_delegation(db(calls_with(DELEGATECALL, CYCLE_A)), CYCLE_A, CYCLE_B);
    let db = with_delegation(db, CYCLE_B, CYCLE_A);
    assert_outer_frame_succeeds("DELEGATECALL into a cycle", db, call_contract());
}

#[test]
fn test_a_callcode_to_a_self_delegating_account_settles() {
    let db = with_delegation(
        db(calls_with(CALLCODE, SELF_DELEGATING)),
        SELF_DELEGATING,
        SELF_DELEGATING,
    );
    assert_outer_frame_succeeds("CALLCODE", db, call_contract());
}

/// The top level resolves a delegation with the same one-hop rule, so a transaction sent
/// straight at a self-delegating address is the control arm of the five above. One hop lands
/// back on the designator, whose own bytes are then what runs — which is not a program, so the
/// transaction settles without succeeding.
#[test]
fn test_a_transaction_to_a_self_delegating_account_settles() {
    let db = with_delegation(db(Bytes::new()), SELF_DELEGATING, SELF_DELEGATING);
    let outcome = assert_settles(
        "a direct transaction",
        db,
        crate::salt::tx(revm::primitives::TxKind::Call(SELF_DELEGATING), Bytes::new(), U256::ZERO),
    );
    assert!(!outcome.result.is_success(), "the designator's own bytes are not a program");
}

/// A `CREATE` runs inside a delegated frame: the creator whose nonce derives the address, and
/// whose bucket prices the charge, is the delegating account, reached through the designator.
#[test]
fn test_a_create_from_a_delegated_frame_settles() {
    let db = db(calls(CYCLE_A)).account_code(CYCLE_B, creates(CREATE));
    let db = with_delegation(db, CYCLE_A, CYCLE_B);
    assert_outer_frame_succeeds("CREATE", db, call_contract());
}

/// The same through `CREATE2`, which derives its address by hashing rather than by nonce.
#[test]
fn test_a_create2_from_a_delegated_frame_settles() {
    let db = db(calls(CYCLE_A)).account_code(CYCLE_B, creates(CREATE2));
    let db = with_delegation(db, CYCLE_A, CYCLE_B);
    assert_outer_frame_succeeds("CREATE2", db, call_contract());
}

/* What a delegation designator means for the price. */

/// An account carrying a delegation designator exists, so value reaching it adds no leaf and
/// costs no state gas however crowded its bucket is.
#[test]
fn test_a_value_call_to_a_delegated_account_charges_no_state_gas() {
    let target = SELF_DELEGATING;
    for m in [1, 8] {
        let db = with_delegation(db(crate::salt::value_call(target).stop().build()), target, EMPTY);
        let envs = crowded_account(minimal_envs(), target, m);
        assert_eq!(run(db, envs, call_contract()).gas.state, 0, "at m = {m}");
    }
}

/// The control arm: the same call to an address that really is empty pays for the leaf it adds,
/// at the crowded price.
#[test]
fn test_a_value_call_to_a_truly_empty_address_pays_the_crowded_price() {
    let envs = crowded_account(minimal_envs(), EMPTY, 8);
    let outcome = run(db(crate::salt::value_call(EMPTY).stop().build()), envs, call_contract());
    assert_eq!(outcome.gas.state, entry(GasId::new_account_state_gas()) * 8);
}

/// A `CREATE` executed inside a delegated frame deploys from the delegating account, so the
/// charge lands in the bucket of the address *that* account's nonce derives — not the delegate's.
#[test]
fn test_a_create_inside_a_delegated_frame_is_priced_at_the_authority_s_address() {
    const AUTHORITY: Address = address!("0000000000000000000000000000000000c00013");
    const CODE: Address = address!("0000000000000000000000000000000000c00014");
    let created = AUTHORITY.create(0);
    let delegate_created = CODE.create(0);

    let db = db(calls(AUTHORITY)).account_code(CODE, creates(CREATE));
    let db = with_delegation(db, AUTHORITY, CODE);

    let envs = crowded_account(minimal_envs(), created, 8);
    let outcome = run(db.clone(), envs.clone(), call_contract());
    assert_eq!(
        outcome.gas.state,
        entry(GasId::create_state_gas()) * 8,
        "the authority's own address is what was priced",
    );
    assert_eq!(envs.bucket_queries(account_bucket(created)), 1);

    // Crowding the address the delegate's own nonce would derive changes nothing.
    let envs = crowded_account(minimal_envs(), delegate_created, 8);
    let outcome = run(db, envs, call_contract());
    assert_eq!(outcome.gas.state, entry(GasId::create_state_gas()));
}

/* CALLCODE: value that never leaves the caller's account. */

/// `CALLCODE` sends value to the caller's own account, which exists, so it adds no leaf — even
/// when the code it borrows lives at an address that does not exist.
#[test]
fn test_a_callcode_to_an_empty_account_charges_no_state_gas() {
    for m in [1, 8] {
        let code = callcode_with_value(EMPTY);
        let envs = crowded_account(minimal_envs(), EMPTY, m);
        assert_eq!(run(db(code), envs, call_contract()).gas.state, 0, "at m = {m}");
    }
}

/// The same, with the borrowed code at an account carrying a delegation designator.
#[test]
fn test_a_callcode_to_a_delegated_account_charges_no_state_gas() {
    let db = with_delegation(db(callcode_with_value(SELF_DELEGATING)), SELF_DELEGATING, EMPTY);
    let envs = crowded_account(minimal_envs(), SELF_DELEGATING, 8);
    assert_eq!(run(db, envs, call_contract()).gas.state, 0);
}

/// And the control arm again: a `CALL` to the same empty address does add a leaf, and pays for
/// it. Without this the two above could be passing because nothing was charged anywhere.
#[test]
fn test_a_call_to_the_same_empty_account_still_pays() {
    let envs = crowded_account(minimal_envs(), EMPTY, 8);
    let outcome = run(db(crate::salt::value_call(EMPTY).stop().build()), envs, call_contract());
    assert_eq!(outcome.gas.state, entry(GasId::new_account_state_gas()) * 8);
}

/// `CALLCODE(gas, target, 1, 0, 0, 0, 0)` followed by `STOP`.
fn callcode_with_value(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(1u64)
        .push_address(target)
        .push_number(1_000_000u64)
        .append(CALLCODE)
        .append(STOP)
        .build()
}

/// A broken SALT environment does not fail a program built only of calls that add no state:
/// nothing on that path asks the pricing hook for a number.
#[test]
fn test_a_callcode_does_not_consult_the_pricing_hook() {
    let envs: SaltEnvs =
        minimal_envs().with_failing_bucket(account_bucket(EMPTY), "unreachable".into());
    let outcome =
        try_run(db(callcode_with_value(EMPTY)), envs, call_contract()).expect("nothing was priced");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0);
}
