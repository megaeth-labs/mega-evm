//! The state a transaction adds: EIP-8037 charges state gas for exactly that state, which the
//! transaction's own gas pays for, and the state-growth limit holds its count — the new accounts
//! and new slots it keeps, net — per transaction and per frame.
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
    constants::{TX_GAS_LIMIT_CAP, TX_STATE_GROWTH_LIMIT},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaEvm, MegaSpecId, MegaTransaction,
    MegaTransactionOutcome, ProtocolLimits, AUTHORIZATION_SIZE, TRANSFER_LOG_SIZE,
    TX_BODY_OVER_DATA_SIZE_LIMIT, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, DELEGATECALL, GAS, INVALID, POP, PUSH0, SELFDESTRUCT, STOP},
    context::result::ExecutionResult,
};

use crate::common::{
    account_state_gas, authorizing_call, body_history, call, context, execute_under, history,
    share, slot_state_gas, state_is_free,
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

/// Runs `tx` over `db` under no limit.
fn run(db: MemoryDatabase, tx: MegaTransaction) -> MegaTransactionOutcome {
    execute_under(db, tx, EvmTxRuntimeLimits::no_limits())
}

/// Runs `tx` over `db` under a transaction data-size limit of `limit`.
fn run_under_data_size(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limit: u64,
) -> MegaTransactionOutcome {
    execute_under(db, tx, EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit))
}

/// The transaction's own frame was stopped crossing the transaction's data-size limit `limit` at
/// `used`, the transaction having counted `before` before the frame: the frame's budget is what the
/// limit leaves, which the crossing crosses too, so the outcome is a revert carrying
/// `MegaLimitExceeded(0, budget)`, never a halt.
fn assert_data_size_stopped(
    what: &str,
    outcome: &MegaTransactionOutcome,
    before: u64,
    limit: u64,
    used: u64,
) {
    assert!(
        !outcome.result.is_success() && !outcome.result.is_halt(),
        "{what}: {:?}",
        outcome.result
    );
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit: limit - before,
        used: used - before,
        frame_local: true,
    };
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

/// The schedule's state gas for the account a creation adds.
fn created_account_state_gas() -> u64 {
    crate::salt::entry(revm::context_interface::cfg::GasId::create_state_gas())
}

/// A slot a delegate writes back, from a frame of its own, gives its state gas back, and the
/// transaction keeps the one slot the delegate writes after it. The other ways a charge is given
/// back — a slot written back in its own frame, a failed child, a failed creation — are held at
/// every multiplier with the SALT pricing.
///
/// Independence: constants — the slot is the schedule's entry.
#[test]
fn test_a_delegate_that_writes_a_slot_back_gives_its_state_gas_back() {
    let delegate_to_b = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(1))
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .append(GAS)
        .append(DELEGATECALL)
        .append(POP)
        .stop()
        .build();
    let restores_then_writes = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::ZERO)
        .sstore(U256::from(1), U256::from(1))
        .stop()
        .build();
    let db = funded().account_code(A, delegate_to_b).account_code(B, restores_then_writes);
    let outcome = run(db, call(CALLER, A, U256::ZERO, BELOW_CAP));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, slot_state_gas());
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

/// An authority an earlier transaction funded is no new account, whether the two transactions run
/// on one journal, as a block builder runs them, or the first is committed before the second runs:
/// the application charges it no account, and the transaction counts no rise for it.
#[test]
fn test_an_authority_an_earlier_transaction_funded_is_no_new_account() {
    use revm::{DatabaseCommit, ExecuteEvm};
    let fund = call(CALLER, AUTHORITY_1, U256::from(1), BELOW_CAP);
    let mut apply = authorizing(A, U256::ZERO, &[(Some(AUTHORITY_1), 0, 0)]);
    apply.0.base.nonce = 1;
    let db = funded().account_code(A, Bytes::from_static(&[STOP]));
    let first = run(db.clone(), fund.clone());
    assert!(first.result.is_success(), "{:?}", first.result);

    let mut committed = db.clone();
    committed.commit(first.result_and_state.state);
    let control = run(committed, apply.clone());
    assert!(control.result.is_success(), "{:?}", control.result);
    let (_, delegation) = account_and_delegation();
    assert_eq!(control.gas.state, delegation, "the delegation's bytes alone");
    assert_eq!(control.usage.state_growth, 0, "the authority existed");
    assert_eq!(control.usage.write_records, 1, "the authority's record");

    let context =
        crate::common::context(db).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    let mut evm = MegaEvm::new(context);
    assert!(ExecuteEvm::transact_one(&mut evm, fund).expect("valid").is_success());
    let second = evm.execute_transaction(apply).expect("valid");
    assert_eq!(second.result_and_state.result, control.result_and_state.result);
    assert_eq!(second.gas, control.gas);
    assert_eq!(second.usage, control.usage);
    assert_eq!(second.limit_exceeded, control.limit_exceeded);
}

/// An authority an earlier transaction called without value, which the database holds no account
/// for, is counted as the application charges it, whether the two transactions run on one journal
/// or the first is committed before the second runs: the transaction counts a rise for it exactly
/// when the application charged it a new account beside the delegation's bytes. On one journal the
/// call touched the empty account, and revm's flag of the touch outlives the call's transaction, so
/// the application finds the authority touched and charges it no account.
#[test]
fn test_an_authority_an_earlier_transaction_touched_is_counted_as_it_is_charged() {
    use revm::{DatabaseCommit, ExecuteEvm};
    if state_is_free() {
        return;
    }
    let (account, delegation) = account_and_delegation();
    let counted_as_charged = |outcome: &MegaTransactionOutcome, path: &str| {
        assert!(outcome.result.is_success(), "{path}: {:?}", outcome.result);
        let charged = outcome.gas.state == account + delegation;
        assert!(charged || outcome.gas.state == delegation, "{path}: {}", outcome.gas.state);
        assert_eq!(outcome.usage.state_growth, u64::from(charged), "{path}: the rise");
        assert_eq!(outcome.usage.write_records, 1, "{path}: the authority's record");
    };
    let touch = call(CALLER, AUTHORITY_1, U256::ZERO, BELOW_CAP);
    let mut apply = authorizing(A, U256::ZERO, &[(Some(AUTHORITY_1), 0, 0)]);
    apply.0.base.nonce = 1;
    let db = funded().account_code(A, Bytes::from_static(&[STOP]));
    let first = run(db.clone(), touch.clone());
    assert!(first.result.is_success(), "{:?}", first.result);

    let mut committed = db.clone();
    committed.commit(first.result_and_state.state);
    counted_as_charged(&run(committed, apply.clone()), "committed");

    let context =
        crate::common::context(db).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits());
    let mut evm = MegaEvm::new(context);
    assert!(ExecuteEvm::transact_one(&mut evm, touch).expect("valid").is_success());
    counted_as_charged(&evm.execute_transaction(apply).expect("valid"), "one journal");
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

/// A transaction whose authorities' records would cross the data-size limit is rejected at
/// validation, whether the authorities are new or exist already: validation holds the body and a
/// record for each entry of the authorization list to the limit, before anything is applied or
/// charged. At the limit, both apply.
#[test]
fn test_authorities_whose_records_would_cross_the_data_size_limit_reject_the_transaction() {
    let both = [(Some(AUTHORITY_1), 0, 0), (Some(AUTHORITY_2), 0, 0)];
    let fresh = || funded().account_code(A, Bytes::from_static(&[STOP]));
    let existing = || {
        fresh()
            .account_balance(AUTHORITY_1, U256::from(1))
            .account_balance(AUTHORITY_2, U256::from(1))
    };
    let body = TX_BODY_SIZE + 2 * AUTHORIZATION_SIZE;
    let at = body + 2 * WRITE_RECORD_SIZE;
    for (name, db) in [("two existing authorities", existing()), ("two new authorities", fresh())] {
        for limit in [body, at - 1] {
            let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit);
            let error = MegaEvm::new(context(db.clone()).with_tx_runtime_limits(limits))
                .execute_transaction(authorizing(A, U256::ZERO, &both))
                .expect_err("rejected at validation");
            let error = format!("{error:?}");
            assert!(error.contains(TX_BODY_OVER_DATA_SIZE_LIMIT), "{name} at {limit}: {error}");
        }
        let applied = run_under_data_size(db, authorizing(A, U256::ZERO, &both), at);
        assert!(applied.result.is_success(), "{name}: {:?}", applied.result);
        assert_eq!(applied.usage.write_records, 2, "{name}: both apply at the limit");
    }
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
        let before = TX_BODY_SIZE + AUTHORIZATION_SIZE + WRITE_RECORD_SIZE;
        assert_data_size_stopped("the slot", &stopped, before, limit, limit + WRITE_RECORD_SIZE);
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
    let (body, used) = (TX_BODY_SIZE, TX_BODY_SIZE + WRITE_RECORD_SIZE);
    assert_data_size_stopped("the slot", &stopped, body, body, used);
    let (idle, tx) = deposit(BytecodeBuilder::default().stop().build());
    let created = run(idle, tx);
    assert!(created.result.is_success(), "{:?}", created.result);

    let caller = &stopped.state[&fresh].info;
    assert_eq!((caller.balance, caller.nonce), (U256::from(5), 1), "the mint and the nonce stay");
    assert_eq!(stopped.gas.state, created.gas.state, "the caller's state gas stays with it");
    assert_eq!(stopped.gas.state, account_state_gas(), "the caller's account");
}

/// A `SELFDESTRUCT` of value to itself grows no state and records nothing. The destructions that
/// move value to another account are held with the SALT pricing and the write records.
#[test]
fn test_a_destruction_to_itself_grows_no_state() {
    let db = funded()
        .account_code(A, BytecodeBuilder::default().push_address(A).append(SELFDESTRUCT).build());
    let outcome = run(db, call(CALLER, A, U256::ZERO, BELOW_CAP));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0, "the state gas");
    assert_eq!(outcome.usage.write_records, 0, "the records");
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
/// charges the created account before its 98/100 split, as EIP-8037 has it, so the records come
/// out of the fiftieth the creator keeps, and the creator must hold 50 times their price after the
/// state charge. EIP-8037 states that order for a creation alone — of a value `CALL` it says only
/// that the charge comes right before the child frame is entered — so, though the engine charges a
/// `CALL`'s new account before its split too, that order is not pinned here: the `CALL` forwards
/// an explicit nothing — the same forward in any order — and its rows hold whatever the order: a
/// gas limit short of the state charge, and one gas short of the whole start, runs out of gas.
///
/// Every case runs under a data-size limit of the body alone, which the record made right after
/// the state charge crosses — the slot's, the beneficiary's, the new account's — and which a charge
/// the transaction cannot pay never reaches. With exactly what the transaction needs to the end of
/// its state charge — and of the start's records — it is that limit that stops the frame that
/// makes the record, which pins each gas limit as one gas short of the charge, or of the start,
/// and nothing else. A value `CALL`'s start is answered with the stop, which its caller catches
/// and ends on; a `CREATE`'s still bumps its creator's nonce, whose record then stops the creator,
/// the transaction's own frame.
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
    // state charge, what it pays after the state charge for the stop to be reached, whether the
    // transaction's own frame catches the stop)
    type Case = (&'static str, MemoryDatabase, fn(u64) -> MegaTransaction, u64, u64, u64, bool);
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
            false,
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
            // The destruction's records are held before their history is charged.
            0,
            false,
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
            false,
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
            true,
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
            // The two records, out of the fiftieth the creator keeps after its forward.
            50 * start_records,
            false,
        ),
    ];
    for (name, db, tx, before, charge, then, caught) in cases {
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
        if caught {
            assert!(paid.result.is_success(), "{name}: the caller catches the stop");
            assert_eq!(
                paid.limit_exceeded, None,
                "{name}: a stop below the transaction's own frame"
            );
            assert_eq!(paid.usage.data_size, TX_BODY_SIZE, "{name}: the start's records went");
            continue;
        }
        assert!(
            matches!(
                paid.limit_exceeded,
                Some(LimitCheck::ExceedsLimit {
                    kind: LimitKind::DataSize,
                    limit: 0,
                    frame_local: true,
                    ..
                })
            ),
            "{name}, everything paid: the record after the charge crosses the frame's budget of \
             nothing the body leaves, {:?}",
            paid.limit_exceeded
        );
    }
}

/* ---------- the state-growth limit ---------- */

// State growth is the count of the new accounts and new slots a transaction keeps, net, held per
// transaction at `TX_STATE_GROWTH_LIMIT` and per frame at 98/100 of what its parent has left,
// rounded down. A rise that crosses either stops the frame that makes it with
// `MegaLimitExceeded(3, limit)`, naming the frame's budget when the rise crosses it, and its caller
// resumes. Every figure below is hand arithmetic from the rises and falls the spec lists.

const C: Address = address!("0000000000000000000000000000000000600009");
const D: Address = address!("000000000000000000000000000000000060000a");

/// Gas for `slots` fresh slots and the calls around them at the prices in effect: the execution
/// cap's regular gas, and a reservoir paying every slot's state gas and its record's history, with
/// room for a few new accounts on top.
fn room(slots: u64) -> u64 {
    TX_GAS_LIMIT_CAP +
        slots * (slot_state_gas() + history(WRITE_RECORD_SIZE)) +
        16 * (account_state_gas() + history(WRITE_RECORD_SIZE)) +
        100_000_000
}

/// Limits holding the transaction to `units` of state growth, and nothing else.
fn growth(units: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::no_limits().with_tx_state_growth_limit(units)
}

/// The revert data of a state-growth stop naming `limit`.
fn growth_stop(limit: u64) -> Bytes {
    LimitCheck::ExceedsLimit { kind: LimitKind::StateGrowth, limit, used: 0, frame_local: true }
        .revert_data()
}

/// The stop of a state-growth crossing of the budget `limit` at `used`, by the frame that made it.
const fn budget_crossed(limit: u64, used: u64) -> Option<LimitCheck> {
    Some(LimitCheck::ExceedsLimit { kind: LimitKind::StateGrowth, limit, used, frame_local: true })
}

/// A call to `A` with gas for `slots` fresh slots, run over `db` under `limits`.
fn run_a(db: MemoryDatabase, slots: u64, limits: EvmTxRuntimeLimits) -> MegaTransactionOutcome {
    execute_under(db, call(CALLER, A, U256::ZERO, room(slots)), limits)
}

/// Appends a `CALL` of `to` with all the gas and no value, dropping its status.
fn call_dropping(code: BytecodeBuilder, to: Address) -> BytecodeBuilder {
    code.call(to, U256::ZERO).append(POP)
}

/// Appends a `CALL` of `to` with all the gas and no value, then returns what it returned or
/// reverted with: a frame that ends with its child's answer as its own output.
fn call_answering(code: BytecodeBuilder, to: Address, value: U256) -> Bytes {
    code.call(to, value).append(POP).return_returndata().build()
}

/// Appends a `DELEGATECALL` of `to` with all the gas, dropping its status: `to`'s code on the
/// caller's own storage.
fn delegatecall_dropping(code: BytecodeBuilder, to: Address) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0]).push_address(to).append_many([
        GAS,
        DELEGATECALL,
        POP,
    ])
}

/// Appends writes of zero to the slots `from..to`: a fall of each slot an earlier write in the
/// transaction made non-zero from an original of zero.
fn clear_slots(mut builder: BytecodeBuilder, from: u64, to: u64) -> BytecodeBuilder {
    for slot in from..to {
        builder = builder.sstore(U256::from(slot), U256::ZERO);
    }
    builder
}

/// The output of `outcome`'s transaction, the revert data of a stop a frame answered with.
fn output(outcome: &MegaTransactionOutcome) -> Bytes {
    outcome.result.output().cloned().unwrap_or_default()
}

/// The spec's numbers: 1,000 new accounts and slots per transaction, kind 3, every frame budgeted
/// at the same 98/100 share as the data size and the KV count; no limit at all leaves it unlimited.
#[test]
fn test_the_state_growth_limit_is_the_specs() {
    assert_eq!(TX_STATE_GROWTH_LIMIT, 1_000);
    let limits = ProtocolLimits::for_spec(MegaSpecId::SATIN).tx_runtime_limits;
    assert_eq!(limits.tx_state_growth_limit, TX_STATE_GROWTH_LIMIT);
    assert_eq!(EvmTxRuntimeLimits::no_limits().tx_state_growth_limit, u64::MAX);
    assert_eq!(LimitKind::StateGrowth.as_u8(), 3);
    assert_eq!(LimitKind::from_u8(3), Some(LimitKind::StateGrowth));
}

/// The transaction's own frame gets what the transaction has left, the whole 1,000 under the spec's
/// limits, and the 1,001st fresh slot stops it, kind 3, naming that budget: the stop settles as a
/// revert, and the slot, like every slot the frame wrote, is not kept. The gas pays for every slot,
/// so the count is what stops it, not the price. 1,000 fresh slots fit.
#[test]
fn test_the_1001st_fresh_slot_stops_the_transactions_own_frame() {
    let db = |count: u64| {
        funded()
            .account_code(A, write_slots(BytecodeBuilder::default(), 1, count + 1).stop().build())
    };
    let limits = EvmTxRuntimeLimits::default();
    let fits = run_a(db(1_000), 1_001, limits);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.state_growth, 1_000);

    let stopped = run_a(db(1_001), 1_001, limits);
    assert_eq!(stopped.limit_exceeded, budget_crossed(1_000, 1_001));
    assert!(matches!(&stopped.result, ExecutionResult::Revert { .. }), "{:?}", stopped.result);
    assert_eq!(output(&stopped), growth_stop(1_000));
    assert_eq!(
        stopped.usage,
        LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 },
        "nothing the frame wrote is kept"
    );
    assert!(stopped.state[&A].storage.values().all(|slot| slot.present_value.is_zero()));
}

/// A child gets 98/100 of what its caller has left: a caller that kept 20 of the 1,000 gives its
/// child `980 × 98 / 100` = 960, the product rounded down rather than the subtraction's 961. The
/// 960th slot fits and the 961st stops the child, naming 960; the caller catches the stop and runs
/// on within what it has left. After a child that kept 960, the caller has the 20 the transaction
/// has left: 20 more fit, and the 21st stops the caller itself, naming its own budget, 1,000.
#[test]
fn test_a_child_gets_98_percent_of_what_its_caller_has_left() {
    assert_eq!(share(1_000 - 20), 960);
    let db = |child: u64, after: u64| {
        let caller = write_slots(BytecodeBuilder::default(), 1, 21);
        let caller = write_slots(call_dropping(caller, B), 21, 21 + after).stop().build();
        funded()
            .account_code(A, caller)
            .account_code(B, write_slots(BytecodeBuilder::default(), 1, child + 1).stop().build())
    };
    let limits = EvmTxRuntimeLimits::default();

    let full = run_a(db(960, 20), 1_000, limits);
    assert!(full.result.is_success(), "{:?}", full.result);
    assert_eq!(full.usage.state_growth, 1_000, "20, the child's 960, and 20");

    let over = run_a(db(960, 21), 1_001, limits);
    assert_eq!(over.limit_exceeded, budget_crossed(1_000, 1_001), "the caller's own budget");

    let child_over = run_a(db(961, 20), 1_001, limits);
    assert!(child_over.result.is_success(), "{:?}", child_over.result);
    assert_eq!(child_over.limit_exceeded, None, "a stop below is no stop");
    assert_eq!(child_over.usage.state_growth, 40, "the caller's 40; none of the child's");
    assert!(
        child_over
            .state
            .get(&B)
            .is_none_or(|b| b.storage.values().all(|s| s.present_value.is_zero())),
        "the child's slots went with its stop"
    );
}

/// Under the spec's 1,000 a frame two calls down gets 960 and one three calls down 940
/// (`960 × 98 / 100` rounded down), when nothing above it kept anything: the 940th slot there fits
/// and the 941st stops that frame, naming 940; every caller catches the stop and ends.
#[test]
fn test_the_state_growth_budgets_at_depth() {
    assert_eq!(share(share(share(1_000))), 940);
    let db = |slots: u64| {
        let d = BytecodeBuilder::default();
        funded()
            .account_code(A, call_answering(BytecodeBuilder::default(), B, U256::ZERO))
            .account_code(B, call_answering(BytecodeBuilder::default(), C, U256::ZERO))
            .account_code(C, call_answering(BytecodeBuilder::default(), D, U256::ZERO))
            .account_code(D, write_slots(d, 1, slots + 1).stop().build())
    };
    let fits = run_a(db(940), 941, EvmTxRuntimeLimits::default());
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.state_growth, 940);

    let stopped = run_a(db(941), 941, EvmTxRuntimeLimits::default());
    assert!(stopped.result.is_success(), "{:?}", stopped.result);
    assert_eq!(stopped.limit_exceeded, None, "the callers caught it");
    assert_eq!(output(&stopped), growth_stop(940), "D's stop, handed up as each caller's answer");
    assert_eq!(stopped.usage.state_growth, 0);
}

/// A frame's net is signed, and its remaining is clamped to the budget it was given: a caller that
/// added 100 of its 1,000 gives its child 882; the child clears those 100 slots — it runs on the
/// caller's storage — so its net is −100, yet what it forwards is still 98/100 of its 882, 864, and
/// the 865th slot of its own child stops that frame. When the child succeeds its net merges, the
/// caller's net is 0, and a later child gets 980; when it reverts, its net is discarded with its
/// writes, the caller keeps its 100, and a later child gets 882.
#[test]
fn test_a_negative_net_does_not_enlarge_the_next_childs_budget() {
    assert_eq!((share(900), share(882), share(1_000)), (882, 864, 980));
    let writer = |slots: u64| write_slots(BytecodeBuilder::default(), 1, slots + 1).stop().build();
    let caller = |then: Option<Address>| {
        let code = delegatecall_dropping(write_slots(BytecodeBuilder::default(), 1, 101), B);
        match then {
            Some(child) => call_answering(code, child, U256::ZERO),
            None => code.stop().build(),
        }
    };
    let clearer = |then: Option<Address>, reverts: bool| {
        let code = clear_slots(BytecodeBuilder::default(), 1, 101);
        let code = match then {
            Some(child) => call_dropping(code, child),
            None => code,
        };
        if reverts {
            code.revert().build()
        } else {
            code.stop().build()
        }
    };
    let limits = EvmTxRuntimeLimits::default();

    // The clamp: the clearing child's own child gets 864.
    for (slots, fits) in [(864, true), (865, false)] {
        let db = funded()
            .account_code(A, caller(None))
            .account_code(B, clearer(Some(C), false))
            .account_code(C, writer(slots));
        let outcome = run_a(db, 1_100, limits);
        assert!(outcome.result.is_success(), "{slots}: {:?}", outcome.result);
        let kept = if fits { slots } else { 0 };
        assert_eq!(outcome.usage.state_growth, kept, "{slots}: the 100 cleared, and C's slots");
    }

    // The merge, and the revert: the next child gets 980, or 882.
    for (reverts, budget) in [(false, 980), (true, 882)] {
        for slots in [budget, budget + 1] {
            let db = funded()
                .account_code(A, caller(Some(D)))
                .account_code(B, clearer(None, reverts))
                .account_code(D, writer(slots));
            let outcome = run_a(db, 1_100, limits);
            let case = format!("child reverts {reverts}, {slots} slots");
            assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
            let caller_kept = if reverts { 100 } else { 0 };
            if slots == budget {
                assert_eq!(outcome.usage.state_growth, caller_kept + slots, "{case}");
                assert!(output(&outcome).is_empty(), "{case}");
            } else {
                assert_eq!(output(&outcome), growth_stop(budget), "{case}: D's budget");
                assert_eq!(outcome.usage.state_growth, caller_kept, "{case}");
            }
        }
    }
}

/// An `SSTORE` falls on the slot it writes back to zero from an original of zero, and the fall
/// makes room: under a limit of one, a slot written `0 → 1 → 0` nets nothing, so another fresh
/// slot fits after it, and a third crosses. A fall with no earlier rise in the frame — a child
/// clearing a slot its caller made — leaves the child's remaining at its budget, and later rises
/// consume the fall: the child keeps a net of its budget and no more.
#[test]
fn test_a_slot_written_back_to_zero_falls_and_makes_room() {
    let code = |extra: u64| {
        let code = BytecodeBuilder::default()
            .sstore(U256::from(1), U256::from(1))
            .sstore(U256::from(1), U256::ZERO);
        write_slots(code, 2, 3 + extra).stop().build()
    };
    let fits = run_a(funded().account_code(A, code(0)), 4, growth(1));
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.state_growth, 1, "slot 1 rose and fell; slot 2 rose");
    let crosses = run_a(funded().account_code(A, code(1)), 4, growth(1));
    assert_eq!(crosses.limit_exceeded, budget_crossed(1, 2));

    // Under ten, the caller keeps one slot and gives its child 8; the child clears the caller's
    // slot and writes nine of its own on the caller's storage, a net of 8.
    assert_eq!(share(10 - 1), 8);
    let child = |slots: u64| {
        write_slots(clear_slots(BytecodeBuilder::default(), 1, 2), 100, 100 + slots).stop().build()
    };
    for (slots, fits) in [(9, true), (10, false)] {
        let caller = delegatecall_dropping(write_slots(BytecodeBuilder::default(), 1, 2), B);
        let db = funded().account_code(A, caller.stop().build()).account_code(B, child(slots));
        let outcome = run_a(db, 12, growth(10));
        assert!(outcome.result.is_success(), "{slots}: {:?}", outcome.result);
        assert_eq!(outcome.usage.state_growth, if fits { slots } else { 1 }, "{slots}");
    }
}

/// Each site the spec lists adds one new account or slot, and nothing else does: a transaction's
/// value to an account that does not exist, a value `CALL` to one, a creation at an empty address —
/// whatever code it deploys — a creation transaction, a `SELFDESTRUCT` moving a balance to one, an
/// authorization creating its authority, and a deposit creating its caller. An account that exists,
/// a `CALLCODE`, a value call to the calling account itself, a destruction moving nothing, and an
/// authorization of an account that exists add none.
#[test]
fn test_each_site_adds_one_new_account_or_slot() {
    let one = U256::from(1);
    let contract = |code: Bytes| funded().account_code(A, code);
    let value_call =
        |to| contract(BytecodeBuilder::default().call(to, one).append(POP).stop().build());
    let fresh_caller = address!("00000000000000000000000000000000006000fe");
    let cases: Vec<(&str, MegaTransactionOutcome, u64)> = vec![
        (
            "the transaction's value to a new account",
            run(funded(), call(CALLER, EMPTY, one, BELOW_CAP)),
            1,
        ),
        (
            "the transaction's value to an account that exists",
            run(funded(), call(CALLER, A, one, BELOW_CAP)),
            0,
        ),
        (
            "a value CALL to a new account",
            run(value_call(EMPTY), call(CALLER, A, U256::ZERO, BELOW_CAP)),
            1,
        ),
        (
            "a value CALL to an account that exists",
            run(value_call(CALLER), call(CALLER, A, U256::ZERO, BELOW_CAP)),
            0,
        ),
        (
            "a value CALL to the calling account itself",
            run(value_call(A), call(CALLER, A, U256::ZERO, BELOW_CAP)),
            0,
        ),
        (
            "a value CALLCODE",
            run(
                contract(
                    BytecodeBuilder::default().callcode(EMPTY, one).append(POP).stop().build(),
                ),
                call(CALLER, A, U256::ZERO, BELOW_CAP),
            ),
            0,
        ),
        (
            "a CREATE deploying 32 bytes",
            run(
                contract(
                    BytecodeBuilder::default()
                        .create(U256::ZERO, crate::common::deploying(32))
                        .append(POP)
                        .stop()
                        .build(),
                ),
                call(CALLER, A, U256::ZERO, BELOW_CAP),
            ),
            1,
        ),
        (
            "a creation transaction",
            run(funded(), crate::common::create(CALLER, crate::common::deploying(32), BELOW_CAP)),
            1,
        ),
        (
            "a SELFDESTRUCT moving a balance to a new account",
            run(
                contract(BytecodeBuilder::default().selfdestruct(EMPTY).build()),
                call(CALLER, A, U256::ZERO, BELOW_CAP),
            ),
            1,
        ),
        (
            "a SELFDESTRUCT moving nothing",
            run(
                MemoryDatabase::default()
                    .account_balance(CALLER, U256::from(10u64.pow(18)))
                    .account_code(A, BytecodeBuilder::default().selfdestruct(EMPTY).build()),
                call(CALLER, A, U256::ZERO, BELOW_CAP),
            ),
            0,
        ),
        (
            "an authorization creating its authority",
            run(
                funded().account_code(A, BytecodeBuilder::default().stop().build()),
                authorizing_call(CALLER, A, U256::ZERO, BELOW_CAP, DELEGATE, &[(AUTHORITY_1, 0)]),
            ),
            1,
        ),
        (
            "an authorization of an account that exists",
            run(
                funded()
                    .account_balance(AUTHORITY_1, one)
                    .account_code(A, BytecodeBuilder::default().stop().build()),
                authorizing_call(CALLER, A, U256::ZERO, BELOW_CAP, DELEGATE, &[(AUTHORITY_1, 0)]),
            ),
            0,
        ),
        (
            "a deposit creating its caller",
            run(
                funded().account_code(A, BytecodeBuilder::default().stop().build()),
                crate::common::deposit(call(fresh_caller, A, U256::ZERO, BELOW_CAP)),
            ),
            1,
        ),
    ];
    for (site, outcome, expected) in cases {
        assert!(outcome.result.is_success(), "{site}: {:?}", outcome.result);
        assert_eq!(outcome.usage.state_growth, expected, "{site}");
    }
}

/// A start its caller's account refuses counts nothing and is stopped by no limit, even with no
/// state growth left: a value call the caller cannot fund, a creation it cannot endow. A creation
/// onto an occupied address fails on the collision, after the creator's nonce bump, and adds no
/// state growth, the address being no new account; nor does a `CREATE2` onto an address that holds
/// a balance, which succeeds under a limit of nothing, keeping the creator's nonce record and the
/// created account's.
#[test]
fn test_a_start_that_makes_no_new_account_is_held_to_nothing() {
    let one = U256::from(1);
    let poor = |code: Bytes| {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10u64.pow(18)))
            .account_code(A, code)
    };
    let refused_call = poor(BytecodeBuilder::default().call(EMPTY, one).append(POP).stop().build());
    let refused_creation = poor(
        BytecodeBuilder::default()
            .create(one, crate::common::deploying(0))
            .append(POP)
            .stop()
            .build(),
    );
    let init = crate::common::deploying(0);
    let target = A.create2(B256::ZERO, alloy_primitives::keccak256(&init));
    let creation = BytecodeBuilder::default()
        .create2(U256::ZERO, &init, U256::ZERO)
        .append(POP)
        .stop()
        .build();
    let collision = funded()
        .account_code(A, creation.clone())
        .account_code(target, Bytes::from_static(&[STOP]));
    let funded_target = funded().account_code(A, creation).account_balance(target, one);
    for (case, db) in [
        ("a value call the caller cannot fund", refused_call),
        ("a creation the caller cannot endow", refused_creation),
        ("a creation onto an address with code", collision),
        ("a creation onto an address that holds a balance", funded_target),
    ] {
        let outcome = run_a(db, 1, growth(0));
        assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{case}");
        assert_eq!(outcome.usage.state_growth, 0, "{case}");
        assert!(output(&outcome).is_empty(), "{case}");
    }
    let onto_funded = run_a(
        funded()
            .account_code(
                A,
                BytecodeBuilder::default()
                    .create2(U256::ZERO, &init, U256::ZERO)
                    .append(POP)
                    .stop()
                    .build(),
            )
            .account_balance(target, one),
        1,
        growth(0),
    );
    assert_eq!(onto_funded.usage.write_records, 2, "the creator's nonce and the created account");
    assert_eq!(onto_funded.state[&target].info.nonce, 1, "created");
}

/// A creation's budget is 98/100 of what its creator has left at its `CREATE`, the creator's nonce
/// record being counted on the creator once the creation returned: a creator with the whole 1,000
/// left, whose nonce bump creates nothing, gives its creation 980,
/// against which the created account is the creation's first rise — its init code fits 979 fresh
/// slots, and the 980th stops it, naming 980, which the creator catches. A creation transaction's
/// own frame is no such child: it gets the whole 1,000, the account and 999 slots.
#[test]
fn test_a_creation_is_budgeted_after_its_creators_counts() {
    assert_eq!(share(1_000), 980);
    let init = |slots: u64| write_slots(BytecodeBuilder::default(), 1, slots + 1).stop().build();
    for (slots, fits) in [(979, true), (980, false)] {
        let creator =
            BytecodeBuilder::default().create(U256::ZERO, init(slots)).append(POP).stop().build();
        let outcome =
            run_a(funded().account_code(A, creator), 1_000, EvmTxRuntimeLimits::default());
        assert!(outcome.result.is_success(), "{slots}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{slots}");
        let created = if fits { 1 + slots } else { 0 };
        assert_eq!(outcome.usage.state_growth, created, "{slots}: the account and its slots");
        assert_eq!(outcome.state[&A].info.nonce, 1, "{slots}: the creator's bump stands");
    }
    for (slots, fits) in [(999, true), (1_000, false)] {
        let tx = crate::common::create(CALLER, init(slots), room(1_001));
        let outcome = execute_under(funded(), tx, EvmTxRuntimeLimits::default());
        if fits {
            assert!(outcome.result.is_success(), "{slots}: {:?}", outcome.result);
            assert_eq!(outcome.usage.state_growth, 1_000, "the account and 999 slots");
        } else {
            assert_eq!(outcome.limit_exceeded, budget_crossed(1_000, 1_001), "{slots}");
        }
    }
}

/// A write that crosses more than one counted dimension stops on the first of data size, KV
/// updates and state growth: a fresh slot's record over the data size and its slot over the state
/// growth is kind 0; its record over the KV count and its slot over the state growth, kind 1.
/// Compute is decided at the charge, before the write is counted: a fresh `SSTORE` whose regular
/// charge crosses gas detention stops with kind 2, though its slot would have crossed the state
/// growth.
#[test]
fn test_a_slot_crossing_several_limits_stops_on_the_first_in_order() {
    use revm::bytecode::opcode::TIMESTAMP;
    let writes =
        || funded().account_code(A, write_slots(BytecodeBuilder::default(), 1, 2).stop().build());
    let kind_of = |outcome: &MegaTransactionOutcome| match outcome.limit_exceeded {
        Some(LimitCheck::ExceedsLimit { kind, .. }) => kind,
        other => panic!("expected a stop: {other:?}"),
    };
    let data = growth(0).with_tx_data_size_limit(TX_BODY_SIZE);
    assert_eq!(kind_of(&run_a(writes(), 1, data)), LimitKind::DataSize);
    let kv = growth(0).with_tx_kv_update_limit(0);
    assert_eq!(kind_of(&run_a(writes(), 1, kv)), LimitKind::KvUpdate);
    assert_eq!(kind_of(&run_a(writes(), 1, growth(0))), LimitKind::StateGrowth);

    let detained = funded().account_code(
        A,
        write_slots(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), 1, 2).stop().build(),
    );
    let capped = growth(0).with_block_env_access_compute_gas_limit(100);
    assert_eq!(kind_of(&run_a(detained.clone(), 1, capped)), LimitKind::ComputeGas);
    assert_eq!(kind_of(&run_a(detained, 1, growth(0))), LimitKind::StateGrowth, "undetained");
}

/// An authorization list is decided before it is applied, on the accounts it would create: a list
/// whose new authorities would cross the transaction's state growth is skipped whole — no authority
/// is bumped, delegated or counted — and the transaction's own frame returns the stop before it
/// runs, kind 3, naming the transaction's limit, since no frame budget exists yet. A list crossing
/// both counts is the KV count's, kind 1. Repeated acceptances of one new authority create it once.
/// A list that fits is applied, and its accounts are counted on the transaction, so the first
/// frame's budget is what they leave: with none left, the transaction's value to a new account
/// stops that frame at its start, naming a budget of nothing, and the authorities stay applied.
#[test]
fn test_an_authorization_list_is_held_to_the_state_growth_it_would_add() {
    let db = || funded().account_code(A, BytecodeBuilder::default().stop().build());
    let both = [(AUTHORITY_1, 0), (AUTHORITY_2, 0)];
    let tx = |value: U256, list: &[(Address, u64)]| {
        authorizing_call(CALLER, A, value, room(0), DELEGATE, list)
    };
    let untouched = |outcome: &MegaTransactionOutcome| {
        for authority in [AUTHORITY_1, AUTHORITY_2] {
            let account = outcome.state.get(&authority);
            assert!(
                account.is_none_or(|account| account.info.nonce == 0),
                "{authority}: not bumped"
            );
            assert!(
                account.is_none_or(|account| account.info.is_empty_code_hash()),
                "{authority}: not delegated"
            );
        }
    };

    let skipped = execute_under(db(), tx(U256::ZERO, &both), growth(1));
    assert_eq!(
        skipped.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::StateGrowth,
            limit: 1,
            used: 2,
            frame_local: false
        })
    );
    assert!(matches!(&skipped.result, ExecutionResult::Revert { .. }), "{:?}", skipped.result);
    assert_eq!(
        skipped.usage,
        LimitUsage { data_size: skipped.usage.data_size, write_records: 0, state_growth: 0 }
    );
    assert_eq!(skipped.gas.state, 0, "no authority's state gas");
    untouched(&skipped);

    let both_counts =
        execute_under(db(), tx(U256::ZERO, &both), growth(1).with_tx_kv_update_limit(1));
    assert!(
        matches!(
            both_counts.limit_exceeded,
            Some(LimitCheck::ExceedsLimit { kind: LimitKind::KvUpdate, limit: 1, .. })
        ),
        "{:?}",
        both_counts.limit_exceeded
    );

    let repeated =
        execute_under(db(), tx(U256::ZERO, &[(AUTHORITY_1, 0), (AUTHORITY_1, 1)]), growth(1));
    assert!(repeated.result.is_success(), "{:?}", repeated.result);
    assert_eq!((repeated.usage.write_records, repeated.usage.state_growth), (1, 1));
    assert_eq!(repeated.state[&AUTHORITY_1].info.nonce, 2, "both applied");

    let fits = execute_under(db(), tx(U256::ZERO, &both), growth(2));
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.state_growth, 2);

    let recipient_stopped = execute_under(
        funded(),
        authorizing_call(CALLER, EMPTY, U256::from(1), room(0), DELEGATE, &both),
        growth(2),
    );
    assert_eq!(recipient_stopped.limit_exceeded, budget_crossed(0, 1), "the recipient");
    assert_eq!(recipient_stopped.usage.state_growth, 2, "the authorities stay");
    for authority in [AUTHORITY_1, AUTHORITY_2] {
        assert_eq!(recipient_stopped.state[&authority].info.nonce, 1, "{authority}: applied");
    }
    assert!(
        recipient_stopped.state.get(&EMPTY).is_none_or(|account| account.info.is_empty()),
        "the recipient was not created"
    );
}

/// A deposit's caller, created before its first frame, is one account of state growth on the
/// transaction: it outlives a stop of the first frame, and the first frame's budget is what it
/// leaves — under a limit of two, one fresh slot fits and the second stops the frame.
#[test]
fn test_a_deposits_new_caller_is_counted_before_its_first_frame() {
    let fresh = address!("00000000000000000000000000000000006000fd");
    let db = |slots: u64| {
        funded()
            .account_code(A, write_slots(BytecodeBuilder::default(), 1, slots + 1).stop().build())
    };
    let tx = || crate::common::deposit(call(fresh, A, U256::ZERO, room(2)));
    let fits = execute_under(db(1), tx(), growth(2));
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.state_growth, 2, "the caller and the slot");
    let stopped = execute_under(db(2), tx(), growth(2));
    assert_eq!(
        stopped.limit_exceeded,
        budget_crossed(1, 2),
        "the frame's budget: what the caller left"
    );
    assert_eq!(stopped.usage.state_growth, 1, "the caller stays");
    assert_eq!(stopped.state[&fresh].info.nonce, 1, "and exists");
    // Under a limit of nothing, which the spec's 1,000 never is, the caller alone is over it, and
    // it is counted all the same: the first frame's start, held to the transaction's limit, is the
    // stop, naming that limit, before the frame writes anything.
    let none_left = execute_under(db(1), tx(), growth(0));
    assert_eq!(
        none_left.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::StateGrowth,
            limit: 0,
            used: 1,
            frame_local: false
        })
    );
    assert_eq!(none_left.usage.state_growth, 1, "the caller, over the limit, stays");
    assert!(none_left.state[&A].storage.values().all(|slot| slot.present_value.is_zero()));
}

/// A frame whose start adds a new account it has no budget for is stopped before it runs, and what
/// its start moved goes back: a value call to a new account under a budget of nothing is answered
/// with the stop, its value not moved, and its caller catches it. A creation's start alike: its
/// init code never runs, and its creator keeps the nonce bump revm made before the creation's
/// checkpoint. A value call to a precompile that gas detention answers from its price moves
/// nothing: the price is the stop, kind 2, though the account its value transfer would add has no
/// room in the state growth either.
#[test]
fn test_a_start_over_the_state_growth_is_stopped_before_it_runs() {
    use revm::bytecode::opcode::TIMESTAMP;
    let one = U256::from(1);
    let value_call =
        funded().account_code(A, call_answering(BytecodeBuilder::default(), EMPTY, one));
    let answered = run_a(value_call, 1, growth(0));
    assert!(answered.result.is_success(), "{:?}", answered.result);
    assert_eq!(output(&answered), growth_stop(0), "the value call's answer is the stop");
    assert!(answered.state.get(&EMPTY).is_none_or(|account| account.info.is_empty()), "not moved");
    assert_eq!(answered.state[&A].info.balance, U256::from(1_000), "the value came back");

    let marker = BytecodeBuilder::default().sstore(U256::from(1), one).stop().build();
    let creator = BytecodeBuilder::default().create(U256::ZERO, marker).append(POP).stop().build();
    let created = run_a(funded().account_code(A, creator), 1, growth(0));
    assert!(created.result.is_success(), "{:?}", created.result);
    assert_eq!(created.usage.state_growth, 0, "the init code never ran");
    assert_eq!(created.state[&A].info.nonce, 1, "the creator's bump stands");
    assert!(created.state.get(&A.create(0)).is_none_or(|account| account.info.is_empty()));

    // The KZG point evaluation, priced 100,000, past a compute limit of 50,000 after the read,
    // which the call's own charges stay within.
    let kzg = address!("000000000000000000000000000000000000000a");
    let precompile = funded().account_code(
        A,
        call_answering(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), kzg, one),
    );
    let cap = EvmTxRuntimeLimits::no_limits().with_block_env_access_compute_gas_limit(50_000);
    let both = run_a(precompile.clone(), 1, cap.with_tx_state_growth_limit(0));
    let (kind, _) = mega_evm::decode_mega_limit_exceeded(&output(&both)).expect("a stop");
    assert_eq!(kind, LimitKind::ComputeGas, "the price alone, not the account");
    assert!(both.state.get(&kzg).is_none_or(|account| account.info.is_empty()), "nothing moved");
    let priced = run_a(precompile, 1, cap);
    let (kind, _) = mega_evm::decode_mega_limit_exceeded(&output(&priced)).expect("a stop");
    assert_eq!(kind, LimitKind::ComputeGas, "without the state growth, the price");
}
