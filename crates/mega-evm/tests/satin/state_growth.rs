//! The state a transaction adds, which no limit holds: EIP-8037 charges state gas for exactly that
//! state, and the transaction's own gas pays for it.
//!
//! What each site charges — a fresh slot, an account a value transfer, a creation or a destruction
//! adds, an applied EIP-7702 authority — is the schedule's state-gas entry, at the minimum bucket,
//! where the test database puts every state, net of what a write back or a failed frame gives back.
//! A charge the transaction cannot pay is an out-of-gas. What was applied before the first frame —
//! an admitted authority, a deposit's created caller — outlives a stop, as through any failed first
//! frame.
//!
//! No figure is read off a run. They move with the byte prices a measurement build sets; where a
//! state byte costs nothing, which only a measurement build arranges, no transaction adds state
//! gas, so the cases that need a state charge return early.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaEvm, MegaTransaction, MegaTransactionOutcome,
    AUTHORIZATION_SIZE, TRANSFER_LOG_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::bytecode::opcode::{
    CALL, CREATE, CREATE2, DELEGATECALL, GAS, INVALID, POP, PUSH0, PUSH1, REVERT, SELFDESTRUCT,
    STOP,
};

use crate::common::{
    account_state_gas, authorizing_call, body_history, call, context, history, slot_state_gas,
    state_is_free,
};

const CALLER: Address = address!("0000000000000000000000000000000000600000");
const A: Address = address!("0000000000000000000000000000000000600001");
const B: Address = address!("0000000000000000000000000000000000600002");
const BURNER: Address = address!("0000000000000000000000000000000000600004");
/// An account that does not exist, so reaching it with value creates it.
const EMPTY: Address = address!("0000000000000000000000000000000000600005");
const DELEGATE: Address = address!("0000000000000000000000000000000000600006");
const AUTHORITY_1: Address = address!("0000000000000000000000000000000000600007");
const AUTHORITY_2: Address = address!("0000000000000000000000000000000000600008");

/// Below the execution cap: the reservoir is empty and every state charge spills onto regular gas.
const BELOW_CAP: u64 = 50_000_000;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(A, U256::from(1_000))
        .account_code(BURNER, Bytes::from_static(&[INVALID]))
}

fn run_under(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits)).execute_transaction(tx).unwrap()
}

/// Runs `tx` over `db` under no limit.
fn run(db: MemoryDatabase, tx: MegaTransaction) -> MegaTransactionOutcome {
    run_under(db, tx, EvmTxRuntimeLimits::no_limits())
}

/// Runs `tx` over `db` under a transaction data-size limit of `limit`.
fn run_under_data_size(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limit: u64,
) -> MegaTransactionOutcome {
    run_under(db, tx, EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit))
}

/// The outcome was stopped by the transaction's data-size limit `limit`, crossed at `used`: a
/// revert carrying `MegaLimitExceeded(0, limit)`, never a halt.
fn assert_data_size_stopped(what: &str, outcome: &MegaTransactionOutcome, limit: u64, used: u64) {
    assert!(
        !outcome.result.is_success() && !outcome.result.is_halt(),
        "{what}: {:?}",
        outcome.result
    );
    let stop =
        LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, limit, used, frame_local: false };
    assert_eq!(outcome.limit_exceeded, Some(stop), "{what}");
    assert_eq!(outcome.result.output().unwrap(), &stop.revert_data(), "{what}");
}

/// Appends writes of the fresh slots `from..to`.
fn write_slots(mut builder: BytecodeBuilder, from: u64, to: u64) -> BytecodeBuilder {
    for slot in from..to {
        builder = builder.sstore(U256::from(slot), U256::from(slot + 1));
    }
    builder
}

/// Appends a `CALL` to `target` with all the gas and `value`, dropping its success flag.
fn then_call(builder: BytecodeBuilder, target: Address, value: u8) -> BytecodeBuilder {
    builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(PUSH1)
        .append(value)
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(POP)
}

/// Appends a `CREATE` of `init` carrying no value, or a `CREATE2` with salt zero, dropping the
/// address.
fn then_create(builder: BytecodeBuilder, init: &Bytes, create2: bool) -> BytecodeBuilder {
    let builder = builder.mstore(0, init);
    // CREATE2 takes a salt below the size, the offset and the value.
    let builder = if create2 { builder.push_number(0_u64) } else { builder };
    builder
        .push_number(init.len() as u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(if create2 { CREATE2 } else { CREATE })
        .append(POP)
}

/// The schedule's state gas for the account a creation adds.
fn created_account_state_gas() -> u64 {
    crate::salt::entry(revm::context_interface::cfg::GasId::create_state_gas())
}

/// The state gas a transaction reports is net: a slot written back gives its state gas back, and so
/// do a failed child's writes, a creation that failed, and a slot a delegate writes back before
/// writing another. Each case is charged two slots' worth over its run, or a created account and a
/// slot, and keeps one slot.
///
/// Independence: constants — the slot is the schedule's entry.
#[test]
fn test_the_state_gas_is_net_of_what_is_given_back() {
    let slot = slot_state_gas();
    let reverting = Bytes::from_static(&[PUSH0, PUSH0, REVERT]);
    let restores_then_writes = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::ZERO)
        .sstore(U256::from(1), U256::from(1))
        .stop()
        .build();
    let delegate_to_b = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(1))
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .append(GAS)
        .append(DELEGATECALL)
        .append(POP)
        .stop()
        .build();
    let cases: [(&str, MemoryDatabase); 3] = [
        (
            "a slot written back, then another",
            funded().account_code(
                A,
                BytecodeBuilder::default()
                    .sstore(U256::ZERO, U256::from(1))
                    .sstore(U256::ZERO, U256::ZERO)
                    .sstore(U256::from(1), U256::from(1))
                    .stop()
                    .build(),
            ),
        ),
        (
            "a child's slot rolled back with its revert, then a slot of the caller's",
            funded()
                .account_code(
                    A,
                    then_call(BytecodeBuilder::default(), B, 0)
                        .sstore(U256::ZERO, U256::from(1))
                        .stop()
                        .build(),
                )
                .account_code(
                    B,
                    BytecodeBuilder::default()
                        .sstore(U256::ZERO, U256::from(1))
                        .append_many([PUSH0, PUSH0, REVERT])
                        .build(),
                ),
        ),
        (
            "a delegate that writes its caller's slot back, then another",
            funded().account_code(A, delegate_to_b).account_code(B, restores_then_writes),
        ),
    ];
    for (name, db) in cases {
        let outcome = run(db, call(CALLER, A, U256::ZERO, BELOW_CAP));
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.gas.state, slot, "{name}");
    }

    // A creation that fails gives its account back, and its creator keeps the slot it writes
    // after it.
    let db = funded().account_code(
        A,
        then_create(BytecodeBuilder::default(), &reverting, false)
            .sstore(U256::from(9), U256::from(1))
            .stop()
            .build(),
    );
    let outcome = run(db, call(CALLER, A, U256::ZERO, BELOW_CAP));
    assert!(outcome.result.is_success(), "a failed creation: {:?}", outcome.result);
    assert_eq!(outcome.gas.state, slot);
}

/// One authorization: its authority — `None` for a signature that recovers to nobody — its chain
/// id and its nonce.
type Authorization = (Option<Address>, u64, u64);

/// A type-4 call from `CALLER` to `to` carrying `value` and `authorizations`, each delegating its
/// authority to `DELEGATE`.
fn authorizing(to: Address, value: U256, authorizations: &[Authorization]) -> MegaTransaction {
    use revm::context_interface::{
        either::Either,
        transaction::{RecoveredAuthority, RecoveredAuthorization},
    };
    let mut tx = authorizing_call(CALLER, to, value, BELOW_CAP, DELEGATE, &[]);
    tx.0.base.authorization_list = authorizations
        .iter()
        .map(|(authority, chain_id, nonce)| {
            Either::Right(RecoveredAuthorization::new_unchecked(
                revm::context_interface::transaction::Authorization {
                    chain_id: U256::from(*chain_id),
                    address: DELEGATE,
                    nonce: *nonce,
                },
                authority.map_or(RecoveredAuthority::Invalid, RecoveredAuthority::Valid),
            ))
        })
        .collect();
    tx
}

/// The schedule's state gas for a new account and for a delegation's bytes.
fn account_and_delegation() -> (u64, u64) {
    use crate::salt::entry;
    use revm::context_interface::cfg::GasId;
    (entry(GasId::new_account_state_gas()), entry(GasId::tx_eip7702_state_gas_bytecode()))
}

/// An authorization adds state only when it applies: a new account for an authority that did not
/// exist, and the delegation's bytes once for an authority that was not delegated. Each authority
/// it applies to is one write record, however many of its authorizations applied, except the
/// sender, whose account is the body's. An authorization that does not apply — a wrong nonce, a
/// wrong chain, a nonce that cannot be bumped, an account with code, a signature that recovers to
/// nobody — adds nothing and leaves its authority as it was.
#[test]
fn test_an_authorization_adds_state_only_when_it_applies() {
    if state_is_free() {
        return;
    }
    let (account, delegation) = account_and_delegation();
    let a1 = Some(AUTHORITY_1);
    let a2 = Some(AUTHORITY_2);
    let with_code = || funded().account_code(AUTHORITY_1, Bytes::from_static(&[STOP]));
    // (case, database, authorizations, state gas, records, AUTHORITY_1's nonce after)
    type Case = (&'static str, MemoryDatabase, Vec<Authorization>, u64, u64, u64);
    let cases: Vec<Case> = vec![
        ("a new authority", funded(), vec![(a1, 0, 0)], account + delegation, 1, 1),
        (
            "two new authorities",
            funded(),
            vec![(a1, 0, 0), (a2, 0, 0)],
            2 * (account + delegation),
            2,
            1,
        ),
        (
            "one new authority authorized twice at its first nonce",
            funded(),
            vec![(a1, 0, 0), (a1, 0, 0)],
            account + delegation,
            1,
            1,
        ),
        (
            "one new authority authorized twice in sequence",
            funded(),
            vec![(a1, 0, 0), (a1, 0, 1)],
            account + delegation,
            1,
            2,
        ),
        (
            "an authority that exists",
            funded().account_balance(AUTHORITY_1, U256::from(1)),
            vec![(a1, 0, 0)],
            delegation,
            1,
            1,
        ),
        (
            "an authority on the transaction's chain",
            funded(),
            vec![(a1, 1, 0)],
            account + delegation,
            1,
            1,
        ),
        ("an authority on another chain", funded(), vec![(a1, 999, 0)], 0, 0, 0),
        ("a wrong nonce", funded(), vec![(a1, 0, 1)], 0, 0, 0),
        ("a nonce that cannot be bumped", funded(), vec![(a1, 0, u64::MAX)], 0, 0, 0),
        ("an authority with code", with_code(), vec![(a1, 0, 0)], 0, 0, 0),
        ("a signature that recovers to nobody", funded(), vec![(None, 0, 0)], 0, 0, 0),
        (
            "the sender, at the nonce its transaction leaves it",
            funded(),
            vec![(Some(CALLER), 0, 1)],
            delegation,
            0,
            0,
        ),
        (
            "the sender, at its transaction's own nonce",
            funded(),
            vec![(Some(CALLER), 0, 0)],
            0,
            0,
            0,
        ),
    ];
    for (name, db, authorizations, state_gas, records, nonce) in cases {
        let tuples = authorizations.len() as u64 * mega_evm::AUTHORIZATION_SIZE;
        let outcome = run(
            db.account_code(A, Bytes::from_static(&[STOP])),
            authorizing(A, U256::ZERO, &authorizations),
        );
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.gas.state, state_gas, "{name}: the state gas");
        assert_eq!(outcome.usage.write_records, records, "{name}: the records");
        assert_eq!(
            outcome.usage.data_size,
            TX_BODY_SIZE + tuples + records * WRITE_RECORD_SIZE,
            "{name}: the data size",
        );
        let authority = outcome.state.get(&AUTHORITY_1);
        assert_eq!(authority.map_or(0, |a| a.info.nonce), nonce, "{name}: the authority's nonce");
        let delegated =
            outcome.state.values().any(|a| a.info.code.as_ref().is_some_and(|c| c.is_eip7702()));
        assert_eq!(delegated, state_gas >= delegation, "{name}: whether anything was delegated");
    }
}

/// An authority in a crowded bucket costs the bucket's multiple of its new account and its
/// delegation, on the state ledger alone: its record and the data size do not move.
#[test]
fn test_a_crowded_authority_moves_the_state_ledger_alone() {
    use crate::salt::{crowded_account, minimal_envs, salt_context};
    let (account, delegation) = account_and_delegation();
    // Room below the execution cap for the authority at the crowded price.
    let gas_limit = BELOW_CAP + 100 * (account + delegation);
    let run = |envs| {
        let mut tx = authorizing(A, U256::ZERO, &[(Some(AUTHORITY_1), 0, 0)]);
        tx.0.base.gas_limit = gas_limit;
        MegaEvm::new(salt_context(funded().account_code(A, Bytes::from_static(&[STOP])), envs))
            .execute_transaction(tx)
            .unwrap()
    };
    let minimal = run(minimal_envs());
    let crowded = run(crowded_account(minimal_envs(), AUTHORITY_1, 100));
    assert!(minimal.result.is_success() && crowded.result.is_success());
    assert_eq!(minimal.gas.state, account + delegation);
    assert_eq!(crowded.gas.state, 100 * (account + delegation));
    assert_eq!(crowded.gas.regular, minimal.gas.regular, "the regular ledger does not move");
    assert_eq!(crowded.usage, minimal.usage, "the record and the data size do not move");
}

/// The state gas an authority costs is charged before the first frame, from the transaction's
/// gas. A gas limit that covers it in a minimal bucket but not in a crowded one is not refused at
/// validation: the transaction runs out of gas before its first frame, and the out-of-gas takes
/// the authorization back.
#[test]
fn test_an_authority_the_transaction_cannot_pay_for_is_not_applied() {
    if state_is_free() {
        return;
    }
    use crate::salt::{crowded_account, minimal_envs, salt_context};
    let tx = |gas_limit| {
        let mut tx = authorizing(A, U256::ZERO, &[(Some(AUTHORITY_1), 0, 0)]);
        tx.0.base.gas_limit = gas_limit;
        tx
    };
    let run = |envs, gas_limit| {
        MegaEvm::new(salt_context(funded().account_code(A, Bytes::from_static(&[STOP])), envs))
            .execute_transaction(tx(gas_limit))
            .expect("the transaction is valid")
    };
    let minimal = run(minimal_envs(), BELOW_CAP);
    assert!(minimal.result.is_success());
    let budget = minimal.gas.gas_used;

    let crowded = run(crowded_account(minimal_envs(), AUTHORITY_1, 100), budget);
    assert!(crowded.result.is_halt(), "an out-of-gas, not a refusal: {:?}", crowded.result);
    let authority = crowded.state.get(&AUTHORITY_1);
    assert!(
        authority.is_none_or(|a| a.info.nonce == 0 && a.info.is_empty_code_hash()),
        "the authorization was taken back: {authority:?}"
    );
    // And a gas limit that covers the authority at the crowded price applies it.
    let (account, delegation) = account_and_delegation();
    let roomy = BELOW_CAP + 100 * (account + delegation);
    assert!(run(crowded_account(minimal_envs(), AUTHORITY_1, 100), roomy).result.is_success());
}

/// A value transaction to an authority it creates pays for one new account, not two: by the time
/// EIP-2780 looks at the recipient the authorization has created it. It is one record too.
#[test]
fn test_an_authority_that_is_the_recipient_pays_for_one_account() {
    let (account, delegation) = account_and_delegation();
    let outcome =
        run(funded(), authorizing(AUTHORITY_1, U256::from(1), &[(Some(AUTHORITY_1), 0, 0)]));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, account + delegation);
    assert_eq!(outcome.usage.write_records, 1);

    let unauthorized = run(funded(), call(CALLER, AUTHORITY_1, U256::from(1), BELOW_CAP));
    assert_eq!(unauthorized.gas.state, account, "the same transfer without it pays the account");
}

/// Authorities whose records cross the data-size limit are not applied, whether they are new or
/// exist already: the limit is enforced before the writes it guards, the gas they charged — their
/// state gas included — is taken back, and the transaction is stopped at its first frame. Taking
/// them back forgoes the refund an authority that existed would have earned: two transactions that
/// differ in that alone spend the same gas.
#[test]
fn test_authorities_crossing_the_data_size_limit_are_not_applied() {
    let both = [(Some(AUTHORITY_1), 0, 0), (Some(AUTHORITY_2), 0, 0)];
    let fresh = || funded().account_code(A, Bytes::from_static(&[STOP]));
    let existing = || {
        fresh()
            .account_balance(AUTHORITY_1, U256::from(1))
            .account_balance(AUTHORITY_2, U256::from(1))
    };
    let body = TX_BODY_SIZE + 2 * AUTHORIZATION_SIZE;
    // (case, database, the data-size limit their two records cross)
    let cases: [(&str, MemoryDatabase, u64); 2] = [
        ("two existing authorities", existing(), body + WRITE_RECORD_SIZE),
        ("two new authorities", fresh(), body + 2 * WRITE_RECORD_SIZE - 1),
    ];
    for (name, db, limit) in cases {
        let stopped = run_under_data_size(db, authorizing(A, U256::ZERO, &both), limit);
        assert_data_size_stopped(name, &stopped, limit, body + 2 * WRITE_RECORD_SIZE);
        assert_eq!(stopped.gas.state, 0, "{name}: their state gas was taken back");
        assert_eq!(stopped.usage.write_records, 0, "{name}: no record of them is kept");
        for authority in [AUTHORITY_1, AUTHORITY_2] {
            let account = stopped.state.get(&authority);
            assert!(
                account.is_none_or(|a| a.info.nonce == 0 && a.info.is_empty_code_hash()),
                "{name}: {authority} was not delegated: {account:?}"
            );
        }
    }

    let spent = |db: MemoryDatabase| {
        run_under_data_size(db, authorizing(A, U256::ZERO, &both), body).gas.gas_used
    };
    assert_eq!(spent(existing()), spent(fresh()), "no refund for an authority never applied");

    let applied = run(fresh(), authorizing(A, U256::ZERO, &both));
    assert!(applied.result.is_success(), "without the limit they apply: {:?}", applied.result);
    assert_eq!(applied.usage.write_records, 2);
}

/// A stop takes back what the frames did and keeps what was applied before the first frame. An
/// admitted authority keeps its nonce, its delegation, its state gas, its write record and the
/// history that record cost, as it does when its frame does nothing; the value the first frame
/// carried and that frame's own record come back with the frame. The stop is the data-size limit,
/// crossed by the record of the slot the frame writes after the authority's record and, with value,
/// after its own start's record and transfer log.
///
/// Rules: [S11.8], [S11.14]. Independence: constants — the limit is the byte table's sizes; what
/// the authority keeps is compared with a run whose frame does nothing.
#[test]
fn test_a_stop_keeps_what_was_applied_before_the_first_frame() {
    let delegation = |outcome: &MegaTransactionOutcome| {
        outcome.state[&AUTHORITY_1].info.code.as_ref().map(|code| code.original_bytes())
    };
    let (account, delegation_state_gas) = account_and_delegation();
    let authority = account + delegation_state_gas;
    for value in [U256::ZERO, U256::from(1)] {
        let writes =
            funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
        let idle = funded().account_code(A, BytecodeBuilder::default().stop().build());
        let tx = authorizing_call(CALLER, A, value, BELOW_CAP, DELEGATE, &[(AUTHORITY_1, 0)]);
        let start = if value.is_zero() { 0 } else { WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE };
        let limit = TX_BODY_SIZE + AUTHORIZATION_SIZE + WRITE_RECORD_SIZE + start;
        let stopped = run_under_data_size(writes, tx.clone(), limit);
        assert_data_size_stopped("the slot", &stopped, limit, limit + WRITE_RECORD_SIZE);
        let applied = run(idle, tx);
        assert!(applied.result.is_success(), "{:?}", applied.result);

        assert_eq!(stopped.state[&AUTHORITY_1].info.nonce, 1, "value {value}");
        assert!(delegation(&stopped).is_some_and(|code| !code.is_empty()), "value {value}");
        assert_eq!(delegation(&stopped), delegation(&applied), "value {value}");
        assert_eq!(stopped.gas.state, authority, "value {value}: the authority's state gas");
        assert_eq!(stopped.gas.state, applied.gas.state, "value {value}");
        assert_eq!(stopped.usage.write_records, 1, "value {value}: the authority's record alone");
        assert_eq!(
            stopped.gas.history_bytes,
            TX_BODY_SIZE + AUTHORIZATION_SIZE + WRITE_RECORD_SIZE,
            "value {value}: the body, its authorization and the authority's record"
        );
        if value.is_zero() {
            assert_eq!(stopped.gas.history, applied.gas.history);
        } else {
            assert_eq!(
                applied.gas.history_bytes - stopped.gas.history_bytes,
                WRITE_RECORD_SIZE,
                "the recipient's record comes back with the frame"
            );
        }
        assert_eq!(stopped.state[&A].info.balance, U256::from(1_000), "value {value}");
    }
}

/// A deposit-like transaction's caller, created before the first frame, outlives a stop with the
/// deposit's mint and the state gas charged for the account. The stop is the data-size limit,
/// crossed by the record of the slot the deposit's frame writes.
///
/// Rules: [S11.13]. Independence: constants — the limit is the byte table's sizes, the caller's
/// account the schedule's entry.
#[test]
fn test_a_stopped_deposit_keeps_the_caller_it_created() {
    let fresh = address!("00000000000000000000000000000000006000ff");
    let deposit = |code: Bytes| {
        let mut tx = call(fresh, A, U256::ZERO, BELOW_CAP);
        tx.0.deposit.source_hash = B256::repeat_byte(0x11);
        tx.0.deposit.mint = Some(5);
        tx.0.base.gas_price = 0;
        (funded().account_code(A, code), tx)
    };
    let (writes, tx) = deposit(write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
    let stopped = run_under_data_size(writes, tx, TX_BODY_SIZE);
    assert_data_size_stopped("the slot", &stopped, TX_BODY_SIZE, TX_BODY_SIZE + WRITE_RECORD_SIZE);
    let (idle, tx) = deposit(BytecodeBuilder::default().stop().build());
    let created = run(idle, tx);
    assert!(created.result.is_success(), "{:?}", created.result);

    let caller = &stopped.state[&fresh].info;
    assert_eq!((caller.balance, caller.nonce), (U256::from(5), 1), "the mint and the nonce stay");
    assert_eq!(stopped.gas.state, created.gas.state, "the caller's state gas stays with it");
    assert_eq!(stopped.gas.state, account_state_gas(), "the caller's account");
}

/// A `SELFDESTRUCT` grows the state only when the value it moves creates its beneficiary: one new
/// account, which is also one write record. Moving value to an account that exists is a record
/// and no state; moving nothing, or to itself, is neither.
#[test]
fn test_a_destruction_grows_state_only_when_it_creates_its_beneficiary() {
    let (account, _) = account_and_delegation();
    let destroys_to = |beneficiary| {
        BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build()
    };
    let cases: [(&str, MemoryDatabase, u64, u64); 4] = [
        ("value to a new beneficiary", funded().account_code(A, destroys_to(EMPTY)), account, 1),
        ("value to a beneficiary that exists", funded().account_code(A, destroys_to(BURNER)), 0, 1),
        (
            "nothing to move",
            funded().account_balance(A, U256::ZERO).account_code(A, destroys_to(EMPTY)),
            0,
            0,
        ),
        ("value to itself", funded().account_code(A, destroys_to(A)), 0, 0),
    ];
    for (name, db, state_gas, records) in cases {
        let outcome = run(db, call(CALLER, A, U256::ZERO, BELOW_CAP));
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.gas.state, state_gas, "{name}: the state gas");
        assert_eq!(outcome.usage.write_records, records, "{name}: the records");
    }
}

/// A state charge the frame cannot pay is an out-of-gas, which no limit turns into a stop.
///
/// Each case is a transaction below the execution cap, so there is no reservoir, whose gas limit
/// pays everything before its state charge and leaves it one gas short of that charge: a fresh
/// slot's `SSTORE`, a `SELFDESTRUCT` that moves a balance to an account that does not exist, a
/// value transaction to an account that does not exist, whose start EIP-2780 charges for the new
/// account, and, from `A`, a value `CALL` to an account that does not exist and a `CREATE`, whose
/// opcodes charge the account the frame they start adds. It halts out of gas, consumes its whole
/// gas limit, keeps no state gas, and reports no stop: not a stop that would give the unspent gas
/// back.
///
/// Once the state charge is paid, a frame start still charges the caller the history of the two
/// records the frame makes, after the gas it forwards is computed [S7.23]; one gas short of what
/// the start costs in all is an out-of-gas too, and the frame does not start [S7.24]. A `CREATE`
/// charges the created account before its 63/64 split, as EIP-8037 has it, so the records come out
/// of the 64th the creator keeps, and the creator must hold 64 times their price after the state
/// charge. EIP-8037 states that order for a creation alone — of a value `CALL` it says only that
/// the charge comes right before the child frame is entered — so, though the engine charges a
/// `CALL`'s new account before its split too, that order is not pinned here: the `CALL` forwards
/// an explicit nothing — the same forward in any order — and its rows hold whatever the order: a
/// gas limit short of the state charge, and one gas short of the whole start, runs out of gas.
///
/// Every case runs under a data-size limit of the body alone, which the record made right after
/// the state charge crosses — the slot's, the beneficiary's, the new account's — and which a charge
/// the transaction cannot pay never reaches. With exactly what the transaction needs to the end of
/// its state charge — and of the start's records — it is that limit that stops the transaction,
/// which pins each gas limit as one gas short of the charge, or of the start, and nothing else.
///
/// Independence: constants. Each gas limit is written out from the spec's numbers — the EIP-2780
/// base of 12,000, 3,000 for the recipient and 6,000 for its value, two gas per `PUSH0` and three
/// per other push, 5,000 for `SELFDESTRUCT` — and the schedule's entries for the opcodes' regular
/// gas, the state charges and the history price; none is read off a run.
#[test]
fn test_a_state_charge_the_frame_cannot_pay_runs_out_of_gas_whatever_the_limit() {
    use mega_evm::{satin_gas_params, MegaHaltReason};
    use revm::{
        context::result::{ExecutionResult, HaltReason},
        context_interface::cfg::GasId,
    };

    // Where a state byte is free there is no state charge to fall short of.
    if state_is_free() {
        return;
    }
    let params = satin_gas_params();
    let entry = |id: GasId| params.get(id);
    // A call to an account that exists, before its code runs: the EIP-2780 base and the
    // recipient, then the body's history.
    let call_intrinsic = 12_000 + 3_000 + body_history(0);
    // The history of the two records a frame start makes: a value transfer's sender and
    // recipient, or a creation's creator nonce and created account.
    let start_records = history(2 * WRITE_RECORD_SIZE);
    // (case, database, the transaction at a gas limit, what it pays before the state charge, the
    // state charge, what it pays after the state charge for the stop to be reached)
    type Case = (&'static str, MemoryDatabase, fn(u64) -> MegaTransaction, u64, u64, u64);
    let to_a = |gas_limit| call(CALLER, A, U256::ZERO, gas_limit);
    let cases: [Case; 5] = [
        (
            "a fresh slot",
            funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 1).stop().build()),
            to_a,
            // Two pushes, then the store's static, cold and set gas.
            call_intrinsic +
                2 * 3 +
                entry(GasId::sstore_static()) +
                entry(GasId::cold_storage_cost()) +
                entry(GasId::sstore_set_without_load_cost()),
            slot_state_gas(),
            // The slot's record is held before its history is charged.
            0,
        ),
        (
            "a SELFDESTRUCT that creates its beneficiary",
            funded().account_code(
                A,
                BytecodeBuilder::default().push_address(EMPTY).append(SELFDESTRUCT).build(),
            ),
            to_a,
            // The push, the opcode's 5,000, its new account's regular gas, and the cold access to
            // the beneficiary.
            call_intrinsic +
                3 +
                5_000 +
                entry(GasId::new_account_cost_for_selfdestruct()) +
                entry(GasId::cold_account_additional_cost()) +
                entry(GasId::warm_storage_read_cost()),
            account_state_gas(),
            // The beneficiary's record is held before its history is charged.
            0,
        ),
        (
            "a value transaction to an account that does not exist",
            funded(),
            |gas_limit| call(CALLER, EMPTY, U256::from(1), gas_limit),
            // The base, the recipient and its value, the body's history, then the history of the
            // recipient's write record, which is charged before the account.
            12_000 + 3_000 + 6_000 + body_history(0) + history(WRITE_RECORD_SIZE),
            account_state_gas(),
            0,
        ),
        (
            "a value CALL to an account that does not exist",
            funded().account_code(
                A,
                BytecodeBuilder::default()
                    .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
                    .push_number(1_u8)
                    .push_address(EMPTY)
                    .append(PUSH0)
                    .append(CALL)
                    .stop()
                    .build(),
            ),
            to_a,
            // Four `PUSH0` for the empty input and output, the value's and the address's pushes,
            // a `PUSH0` for a forward of nothing, then the call's warm access, the cold surcharge
            // on the recipient, the value transfer and the new account's regular gas.
            call_intrinsic +
                4 * 2 +
                3 +
                3 +
                2 +
                entry(GasId::warm_storage_read_cost()) +
                entry(GasId::cold_account_additional_cost()) +
                entry(GasId::transfer_value_cost()) +
                entry(GasId::new_account_cost()),
            account_state_gas(),
            // The two records, which a forward of nothing leaves to the caller.
            start_records,
        ),
        (
            "a CREATE",
            funded().account_code(
                A,
                BytecodeBuilder::default()
                    .append_many([PUSH0, PUSH0, PUSH0, CREATE])
                    .stop()
                    .build(),
            ),
            to_a,
            // Three `PUSH0` for an empty init code carrying no value, then the creation's 32,000.
            call_intrinsic + 3 * 2 + entry(GasId::create()),
            created_account_state_gas(),
            // The two records, out of the 64th the creator keeps after its forward.
            64 * start_records,
        ),
    ];
    for (name, db, tx, before, charge, then) in cases {
        let short = before + charge - 1;
        let start = before + charge + then;
        let mut rows = vec![(format!("{name}, one gas short of the state charge"), short)];
        if then > 0 {
            rows.push((format!("{name}, one gas short of the frame start"), start - 1));
        }
        for (row, gas_limit) in rows {
            let ran_out = run_under_data_size(db.clone(), tx(gas_limit), TX_BODY_SIZE);
            assert!(
                matches!(
                    &ran_out.result,
                    ExecutionResult::Halt {
                        reason: MegaHaltReason::Base(HaltReason::OutOfGas(_)),
                        ..
                    }
                ),
                "{row}: an out-of-gas: {:?}",
                ran_out.result
            );
            assert_eq!(ran_out.limit_exceeded, None, "{row}: no stop");
            assert_eq!(ran_out.gas.gas_used, gas_limit, "{row}: the whole gas limit is consumed");
            assert_eq!(ran_out.gas.state, 0, "{row}: no state gas is kept");
        }

        let paid = run_under_data_size(db.clone(), tx(start), TX_BODY_SIZE);
        assert!(
            matches!(
                paid.limit_exceeded,
                Some(LimitCheck::ExceedsLimit {
                    kind: LimitKind::DataSize,
                    limit: TX_BODY_SIZE,
                    frame_local: false,
                    ..
                })
            ),
            "{name}, everything paid: the record after the charge is the stop, {:?}",
            paid.limit_exceeded
        );
    }
}
