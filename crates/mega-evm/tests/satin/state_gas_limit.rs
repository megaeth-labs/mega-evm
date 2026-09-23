//! The state-gas limit: a transaction's state growth, held to a limit in state gas.
//!
//! EIP-8037 charges state gas for exactly the state a transaction adds, so the limit on its state
//! gas is the limit on its growth. A transaction is held to `tx_state_gas_limit` at every site
//! state gas is charged — a fresh slot, an account a value transfer, a creation or a destruction
//! adds, deployed code, an applied EIP-7702 authority, the first frame's recipient or created
//! account — on what it holds net of refills and of failed frames. A crossing stops the
//! transaction with a revert carrying `MegaLimitExceeded(3, limit)`, wherever on the call stack
//! it happens: there are no frame budgets.
//!
//! The figures are read off the engine rather than written out: each case first runs without a
//! limit, and the state gas it reports is what the limit is set against.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaEvm, MegaTransaction, MegaTransactionOutcome,
    TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::bytecode::opcode::{
    CALL, CREATE, CREATE2, DELEGATECALL, GAS, INVALID, POP, PUSH0, PUSH1, RETURN, REVERT,
    SELFDESTRUCT, STOP,
};

use crate::common::{authorizing_call, call, context, create};

const CALLER: Address = address!("0000000000000000000000000000000000600000");
const A: Address = address!("0000000000000000000000000000000000600001");
const B: Address = address!("0000000000000000000000000000000000600002");
const C: Address = address!("0000000000000000000000000000000000600003");
const BURNER: Address = address!("0000000000000000000000000000000000600004");
/// An account that does not exist, so reaching it with value creates it.
const EMPTY: Address = address!("0000000000000000000000000000000000600005");
const DELEGATE: Address = address!("0000000000000000000000000000000000600006");
const AUTHORITY_1: Address = address!("0000000000000000000000000000000000600007");
const AUTHORITY_2: Address = address!("0000000000000000000000000000000000600008");

/// Below the execution cap: the reservoir is empty and every state charge spills onto regular gas.
const BELOW_CAP: u64 = 50_000_000;
/// Above the execution cap: the reservoir pays every state charge.
const ABOVE_CAP: u64 = TX_GAS_LIMIT_CAP + 100_000_000;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(A, U256::from(1_000))
        .account_code(BURNER, Bytes::from_static(&[INVALID]))
}

fn run_under(db: MemoryDatabase, tx: MegaTransaction, limit: u64) -> MegaTransactionOutcome {
    MegaEvm::new(
        context(db)
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(limit)),
    )
    .execute_transaction(tx)
    .unwrap()
}

/// The outcome was stopped by the state-gas limit `limit`, crossed at `used`: a revert carrying
/// `MegaLimitExceeded(3, limit)`, never a halt.
fn assert_state_stopped(what: &str, outcome: &MegaTransactionOutcome, limit: u64, used: u64) {
    assert!(
        !outcome.result.is_success() && !outcome.result.is_halt(),
        "{what}: {:?}",
        outcome.result
    );
    let stop =
        LimitCheck::ExceedsLimit { kind: LimitKind::StateGrowth, limit, used, frame_local: false };
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

/// Init code that returns `size` zero bytes as the deployed contract.
fn constructor_returning(size: u8) -> Bytes {
    BytecodeBuilder::default().push_number(size).push_number(0_u8).append(RETURN).build()
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

/// The state gas `tx` holds when it runs without a limit.
fn state_gas_of(db: &MemoryDatabase, tx: &MegaTransaction) -> u64 {
    let outcome = run_under(db.clone(), tx.clone(), u64::MAX);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    outcome.gas.state
}

/// The state gas one fresh slot of `A` costs.
fn one_slot() -> u64 {
    let db = funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
    state_gas_of(&db, &call(CALLER, A, U256::ZERO, BELOW_CAP))
}

/// One case per site state gas is charged at: the transaction, and the state it holds.
fn sites(gas_limit: u64) -> Vec<(&'static str, MemoryDatabase, MegaTransaction)> {
    let to_a = |code: BytecodeBuilder| funded().account_code(A, code.stop().build());
    let a = || call(CALLER, A, U256::ZERO, gas_limit);
    let deployed = constructor_returning(32);
    vec![
        ("a fresh slot", to_a(write_slots(BytecodeBuilder::default(), 0, 1)), a()),
        (
            "a value call that creates its recipient",
            to_a(then_call(BytecodeBuilder::default(), EMPTY, 1)),
            a(),
        ),
        (
            "a nested creation",
            to_a(then_create(BytecodeBuilder::default(), &Bytes::new(), false)),
            a(),
        ),
        (
            "a nested CREATE2 that deposits code",
            to_a(then_create(BytecodeBuilder::default(), &deployed, true)),
            a(),
        ),
        (
            "a nested creation that deposits code",
            to_a(then_create(BytecodeBuilder::default(), &deployed, false)),
            a(),
        ),
        (
            "a destruction that creates its beneficiary",
            to_a(BytecodeBuilder::default().push_address(EMPTY).append(SELFDESTRUCT)),
            a(),
        ),
        ("a value transaction that creates its recipient", funded(), {
            call(CALLER, EMPTY, U256::from(1), gas_limit)
        }),
        ("a creation transaction", funded(), create(CALLER, Bytes::new(), gas_limit)),
        (
            "a creation transaction that deposits code",
            funded(),
            create(CALLER, deployed.clone(), gas_limit),
        ),
        (
            "an authority the transaction creates",
            funded().account_code(A, Bytes::from_static(&[STOP])),
            authorizing_call(CALLER, A, U256::ZERO, gas_limit, DELEGATE, &[(AUTHORITY_1, 0)]),
        ),
    ]
}

/* ---------- every site ---------- */

/// At every site, a limit equal to the state gas a transaction holds lets it through with that
/// state gas, and one below it stops it at the charge that crosses — the last one it makes — with
/// the figure it reports. The stop keeps none of it, below and above the execution cap alike.
#[test]
fn test_every_site_is_held_to_the_limit_at_the_state_gas_it_reports() {
    for gas_limit in [BELOW_CAP, ABOVE_CAP] {
        for (name, db, tx) in sites(gas_limit) {
            let name = format!("{name}, gas limit {gas_limit}");
            let held = state_gas_of(&db, &tx);
            assert!(held > 0, "{name}");

            let fits = run_under(db.clone(), tx.clone(), held);
            assert!(fits.result.is_success(), "{name}: {:?}", fits.result);
            assert_eq!(fits.limit_exceeded, None, "{name}");
            assert_eq!(fits.gas.state, held, "{name}");

            let stopped = run_under(db, tx, held - 1);
            assert_state_stopped(&name, &stopped, held - 1, held);
            assert_eq!(stopped.gas.state, 0, "{name}: the stop keeps none of it");
            if gas_limit > TX_GAS_LIMIT_CAP {
                assert_eq!(
                    stopped.gas.reservoir_remaining,
                    gas_limit - TX_GAS_LIMIT_CAP - stopped.gas.state - stopped.gas.history,
                    "{name}: the stop hands the reservoir back"
                );
            }
        }
    }
}

/// A crossing stops the transaction at the charge that crossed: nothing after it runs. Each case
/// crosses at its site and would then burn what its frame has left in a call to a contract that
/// halts; the stop spends none of it.
#[test]
fn test_the_crossing_charge_is_where_the_transaction_stops() {
    let deployed = constructor_returning(32);
    let cases: [(&str, BytecodeBuilder); 5] = [
        ("a fresh slot", write_slots(BytecodeBuilder::default(), 0, 1)),
        (
            "a value call that creates its recipient",
            then_call(BytecodeBuilder::default(), EMPTY, 1),
        ),
        ("a nested creation", then_create(BytecodeBuilder::default(), &Bytes::new(), false)),
        ("deployed code", then_create(BytecodeBuilder::default(), &deployed, false)),
        ("a nested CREATE2", then_create(BytecodeBuilder::default(), &Bytes::new(), true)),
    ];
    for (name, site) in cases {
        let db = funded().account_code(A, then_call(site, BURNER, 0).stop().build());
        let tx = call(CALLER, A, U256::ZERO, BELOW_CAP);
        let burned = run_under(db.clone(), tx.clone(), u64::MAX);
        assert!(burned.gas.regular > BELOW_CAP / 2, "{name}: the burner burns: {:?}", burned.gas);

        let held = burned.gas.state;
        let stopped = run_under(db, tx, held - 1);
        assert_state_stopped(name, &stopped, held - 1, held);
        assert!(stopped.gas.regular < 1_000_000, "{name}: nothing burned: {:?}", stopped.gas);
        assert_eq!(stopped.gas.state, 0, "{name}");
    }
}

/// Deployed code whose state gas crosses the limit is not left deployed, whether the creation is
/// the transaction's own or a nested one: the limit is held before `return_create` commits it.
#[test]
fn test_deployed_code_that_crosses_is_not_left_deployed() {
    let deployed = constructor_returning(32);
    let nested = funded()
        .account_code(A, then_create(BytecodeBuilder::default(), &deployed, false).stop().build());
    let cases = [
        (
            "the transaction's creation",
            funded(),
            create(CALLER, deployed, BELOW_CAP),
            CALLER.create(0),
        ),
        ("a nested creation", nested, call(CALLER, A, U256::ZERO, BELOW_CAP), A.create(0)),
    ];
    for (name, db, tx, created) in cases {
        let held = state_gas_of(&db, &tx);
        let kept = run_under(db.clone(), tx.clone(), held);
        assert_eq!(
            kept.state[&created].info.code.as_ref().map(|code| code.original_bytes().len()),
            Some(32),
            "{name}"
        );

        let stopped = run_under(db, tx, held - 1);
        assert_state_stopped(name, &stopped, held - 1, held);
        assert!(
            stopped.state.get(&created).is_none_or(|account| account.info.is_empty_code_hash()),
            "{name}: no code is left at the created address"
        );
    }
}

/// A creation whose init code reverts deposits nothing, whatever its revert data: the limit holds
/// the account the creation would have added and the caller goes on.
#[test]
fn test_a_reverting_creation_is_not_held_for_its_revert_data() {
    let reverting =
        BytecodeBuilder::default().push_number(32_u8).push_number(0_u8).append(REVERT).build();
    let account = state_gas_of(
        &funded().account_code(
            A,
            then_create(BytecodeBuilder::default(), &Bytes::new(), false).stop().build(),
        ),
        &call(CALLER, A, U256::ZERO, BELOW_CAP),
    );
    let db = funded()
        .account_code(A, then_create(BytecodeBuilder::default(), &reverting, false).stop().build());
    let outcome = run_under(db, call(CALLER, A, U256::ZERO, BELOW_CAP), account);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None);
    assert_eq!(outcome.gas.state, 0, "the failed creation's account charge came back");
}

/* ---------- one limit for the whole call stack ---------- */

/// A child's crossing stops the whole transaction: there is no frame budget to revert it alone.
/// What its callers hold counts: `A`, `B` and `C` each write one slot, and a limit one gas short
/// of three slots is crossed in `C`, with the three slots as the figure.
#[test]
fn test_a_childs_crossing_latches_the_transaction() {
    let slot = one_slot();
    let db = funded()
        .account_code(
            A,
            then_call(write_slots(BytecodeBuilder::default(), 0, 1), B, 0).stop().build(),
        )
        .account_code(
            B,
            then_call(write_slots(BytecodeBuilder::default(), 0, 1), C, 0).stop().build(),
        )
        .account_code(C, write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
    let tx = call(CALLER, A, U256::ZERO, BELOW_CAP);

    let fits = run_under(db.clone(), tx.clone(), 3 * slot);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.gas.state, 3 * slot);

    let stopped = run_under(db.clone(), tx.clone(), 3 * slot - 1);
    assert_state_stopped("C crosses", &stopped, 3 * slot - 1, 3 * slot);

    let stopped = run_under(db, tx, 2 * slot - 1);
    assert_state_stopped("B crosses", &stopped, 2 * slot - 1, 2 * slot);
}

/// What was charged before the first frame and what the frames charge add up: an authority the
/// transaction creates and a slot its frame writes cross a limit neither crosses alone.
#[test]
fn test_charges_before_the_first_frame_and_in_it_add_up() {
    let slot = one_slot();
    let db = funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
    let tx = authorizing_call(CALLER, A, U256::ZERO, BELOW_CAP, DELEGATE, &[(AUTHORITY_1, 0)]);
    let held = state_gas_of(&db, &tx);
    let authority = held - slot;
    assert!(authority > slot, "an authority costs an account and its delegation");

    let stopped = run_under(db, tx, held - 1);
    assert_state_stopped("the slot after the authority", &stopped, held - 1, held);
    assert_eq!(
        stopped.state[&AUTHORITY_1].info.nonce, 1,
        "the authority fitted, and the stop in the frame does not take it back"
    );
    assert_eq!(stopped.gas.state, authority, "the authority's state gas stays with it");
}

/* ---------- refills give their room back ---------- */

/// The limit holds what the transaction holds net: a slot written back gives its room back, and
/// so do a failed child's writes, a creation that failed, and a slot a delegate writes back
/// before writing another. Each case holds two slots' worth of charges over its run, and fits a
/// limit of one.
#[test]
fn test_what_is_given_back_gives_its_room_back() {
    let slot = one_slot();
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
        let outcome = run_under(db, call(CALLER, A, U256::ZERO, BELOW_CAP), slot);
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{name}");
        assert_eq!(outcome.gas.state, slot, "{name}");
    }

    // A creation that fails gives its account back to its creator, which then has room for a
    // slot under a limit of one account.
    let account = state_gas_of(
        &funded().account_code(
            A,
            then_create(BytecodeBuilder::default(), &Bytes::new(), false).stop().build(),
        ),
        &call(CALLER, A, U256::ZERO, BELOW_CAP),
    );
    let db = funded().account_code(
        A,
        then_create(BytecodeBuilder::default(), &reverting, false)
            .sstore(U256::from(9), U256::from(1))
            .stop()
            .build(),
    );
    let outcome = run_under(db, call(CALLER, A, U256::ZERO, BELOW_CAP), account);
    assert!(outcome.result.is_success(), "a failed creation: {:?}", outcome.result);
    assert_eq!(outcome.gas.state, slot);
}

/* ---------- SALT ---------- */

/// The limit is on state gas, so it counts state at the price SALT sets for it: a slot in a bucket
/// twice the minimum costs two slots of the limit, and a limit of two minimal slots admits two
/// slots in minimal buckets but crosses on the second when the first is in the crowded one.
#[test]
fn test_a_crowded_bucket_reaches_the_limit_sooner() {
    use crate::salt::{crowded_slot, minimal_envs, salt_context};

    let slot = one_slot();
    let db =
        || funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 2).stop().build());
    let run = |envs| {
        MegaEvm::new(salt_context(db(), envs).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(2 * slot),
        ))
        .execute_transaction(call(CALLER, A, U256::ZERO, BELOW_CAP))
        .unwrap()
    };

    let minimal = run(minimal_envs());
    assert!(minimal.result.is_success(), "{:?}", minimal.result);
    assert_eq!(minimal.gas.state, 2 * slot);

    let unlimited =
        MegaEvm::new(salt_context(db(), crowded_slot(minimal_envs(), A, U256::ZERO, 2)))
            .execute_transaction(call(CALLER, A, U256::ZERO, BELOW_CAP))
            .unwrap();
    assert_eq!(unlimited.gas.state, 3 * slot, "the crowded slot costs two");
    let crowded = run(crowded_slot(minimal_envs(), A, U256::ZERO, 2));
    assert_state_stopped("the second slot after a crowded first", &crowded, 2 * slot, 3 * slot);

    let crowded = run(crowded_slot(minimal_envs(), A, U256::ZERO, 3));
    assert_state_stopped("a first slot three times the price", &crowded, 2 * slot, 3 * slot);
}

/* ---------- authorities ---------- */

/// Authorities whose state gas crosses the limit are not applied: the limit is enforced before
/// the writes it guards, the gas they charged is taken back, and the transaction is stopped at its
/// first frame. The state gas is held before their records are, so a transaction that crosses
/// both limits reports the state gas.
#[test]
fn test_authorities_crossing_the_limit_are_not_applied() {
    let db = || funded().account_code(A, Bytes::from_static(&[STOP]));
    let tx = || {
        authorizing_call(
            CALLER,
            A,
            U256::ZERO,
            BELOW_CAP,
            DELEGATE,
            &[(AUTHORITY_1, 0), (AUTHORITY_2, 0)],
        )
    };
    let held = state_gas_of(&db(), &tx());
    let applied = run_under(db(), tx(), held);
    assert!(applied.result.is_success());
    assert_eq!(applied.usage.write_records, 2);

    for limits in [
        EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(held - 1),
        EvmTxRuntimeLimits::no_limits()
            .with_tx_state_gas_limit(held - 1)
            .with_tx_kv_update_limit(0),
    ] {
        let stopped = MegaEvm::new(context(db()).with_tx_runtime_limits(limits))
            .execute_transaction(tx())
            .unwrap();
        assert_state_stopped("two authorities", &stopped, held - 1, held);
        assert_eq!(stopped.usage.write_records, 0, "no record of them is kept");
        for authority in [AUTHORITY_1, AUTHORITY_2] {
            let account = stopped.state.get(&authority);
            assert!(
                account.is_none_or(|a| a.info.nonce == 0 && a.info.is_empty_code_hash()),
                "{authority} was not delegated: {account:?}"
            );
        }
    }
}

/* ---------- which limit binds first ---------- */

/// A fresh slot is charged its state gas inside `SSTORE`, before its record is counted, so a slot
/// that crosses the state-gas and data-size limits at once is the state-gas limit's stop.
#[test]
fn test_a_slot_crossing_both_limits_reports_the_state_gas() {
    let slot = one_slot();
    let db = funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
    let outcome = MegaEvm::new(
        context(db).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_state_gas_limit(slot - 1)
                .with_tx_data_size_limit(TX_BODY_SIZE + WRITE_RECORD_SIZE - 1),
        ),
    )
    .execute_transaction(call(CALLER, A, U256::ZERO, BELOW_CAP))
    .unwrap();
    assert_state_stopped("a slot over both", &outcome, slot - 1, slot);
}

/// Each transaction the same EVM runs is held to the limit from zero.
#[test]
fn test_each_transaction_is_held_to_the_limit_from_zero() {
    let slot = one_slot();
    let db = funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 2).stop().build());
    let mut evm =
        MegaEvm::new(context(db).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(2 * slot),
        ));
    for run in 0..2 {
        let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, BELOW_CAP)).unwrap();
        assert!(outcome.result.is_success(), "run {run}: {:?}", outcome.result);
        assert_eq!(outcome.gas.state, 2 * slot, "run {run}");
    }
}
