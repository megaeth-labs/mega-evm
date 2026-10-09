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
//! The figures are the schedule's state-gas entries — a fresh slot, a new account, a created
//! account, a byte of deployed code, a delegation indicator — at the minimum bucket, where the test
//! database puts every state, and no figure is read off a run. They move with the byte prices a
//! measurement build sets. Where a state byte costs nothing, which only a measurement build
//! arranges, no transaction adds state gas and the limit has nothing to hold, so the cases that
//! need a crossing, or an upfront charge to give back, return early.

use alloy_primitives::{address, Address, Bytes, B256, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm, MegaTransaction,
    MegaTransactionOutcome, AUTHORIZATION_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLCODE, CREATE, CREATE2, DELEGATECALL, GAS, INVALID, POP, PUSH0, PUSH1, RETURN,
        REVERT, SELFDESTRUCT, STOP,
    },
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, Gas, InstructionResult,
        InterpreterResult,
    },
    Database, Inspector,
};

use crate::common::{
    account_state_gas, authorizing_call, body_history, call, call_with_value_and_data, context,
    create, history, slot_state_gas, state_is_free,
};

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

/// The schedule's state gas for the account a creation adds.
fn created_account_state_gas() -> u64 {
    crate::salt::entry(revm::context_interface::cfg::GasId::create_state_gas())
}

/// The schedule's state gas for a creation that deposits `len` bytes of code: the created account
/// and the code.
fn deployment_state_gas(len: u64) -> u64 {
    use revm::context_interface::cfg::GasId;
    created_account_state_gas() + len * crate::salt::entry(GasId::code_deposit_state_gas())
}

/// One case per site state gas is charged at: the transaction, and the state gas it holds, from
/// the schedule.
fn sites(gas_limit: u64) -> Vec<(&'static str, MemoryDatabase, MegaTransaction, u64)> {
    let to_a = |code: BytecodeBuilder| funded().account_code(A, code.stop().build());
    let a = || call(CALLER, A, U256::ZERO, gas_limit);
    let deployed = constructor_returning(32);
    let (account, delegation) = account_and_delegation();
    vec![
        (
            "a fresh slot",
            to_a(write_slots(BytecodeBuilder::default(), 0, 1)),
            a(),
            slot_state_gas(),
        ),
        (
            "a value call that creates its recipient",
            to_a(then_call(BytecodeBuilder::default(), EMPTY, 1)),
            a(),
            account,
        ),
        (
            "a nested creation",
            to_a(then_create(BytecodeBuilder::default(), &Bytes::new(), false)),
            a(),
            created_account_state_gas(),
        ),
        (
            "a nested CREATE2 that deposits code",
            to_a(then_create(BytecodeBuilder::default(), &deployed, true)),
            a(),
            deployment_state_gas(32),
        ),
        (
            "a nested creation that deposits code",
            to_a(then_create(BytecodeBuilder::default(), &deployed, false)),
            a(),
            deployment_state_gas(32),
        ),
        (
            "a destruction that creates its beneficiary",
            to_a(BytecodeBuilder::default().push_address(EMPTY).append(SELFDESTRUCT)),
            a(),
            account,
        ),
        (
            "a value transaction that creates its recipient",
            funded(),
            call(CALLER, EMPTY, U256::from(1), gas_limit),
            account,
        ),
        (
            "a creation transaction",
            funded(),
            create(CALLER, Bytes::new(), gas_limit),
            created_account_state_gas(),
        ),
        (
            "a creation transaction that deposits code",
            funded(),
            create(CALLER, deployed.clone(), gas_limit),
            deployment_state_gas(32),
        ),
        (
            "an authority the transaction creates",
            funded().account_code(A, Bytes::from_static(&[STOP])),
            authorizing_call(CALLER, A, U256::ZERO, gas_limit, DELEGATE, &[(AUTHORITY_1, 0)]),
            account + delegation,
        ),
    ]
}

/* ---------- every site ---------- */

/// At every site, a limit equal to the state gas a transaction holds lets it through with that
/// state gas, and one below it stops it at the charge that crosses — the last one it makes — with
/// the figure it reports. The stop keeps none of it, below and above the execution cap alike.
///
/// Rules: [S10.36], [S10.41]. Independence: constants — what each site holds is the schedule's
/// entries, not a run's figure.
#[test]
fn test_every_site_is_held_to_the_limit_at_the_state_gas_it_reports() {
    if state_is_free() {
        return;
    }
    for gas_limit in [BELOW_CAP, ABOVE_CAP] {
        for (name, db, tx, held) in sites(gas_limit) {
            let name = format!("{name}, gas limit {gas_limit}");

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
///
/// Rules: [S10.36]. Independence: constants — what each site holds is the schedule's entries.
#[test]
fn test_the_crossing_charge_is_where_the_transaction_stops() {
    if state_is_free() {
        return;
    }
    let deployed = constructor_returning(32);
    let cases: [(&str, BytecodeBuilder, u64); 5] = [
        ("a fresh slot", write_slots(BytecodeBuilder::default(), 0, 1), slot_state_gas()),
        (
            "a value call that creates its recipient",
            then_call(BytecodeBuilder::default(), EMPTY, 1),
            account_state_gas(),
        ),
        (
            "a nested creation",
            then_create(BytecodeBuilder::default(), &Bytes::new(), false),
            created_account_state_gas(),
        ),
        (
            "deployed code",
            then_create(BytecodeBuilder::default(), &deployed, false),
            deployment_state_gas(32),
        ),
        (
            "a nested CREATE2",
            then_create(BytecodeBuilder::default(), &Bytes::new(), true),
            created_account_state_gas(),
        ),
    ];
    for (name, site, held) in cases {
        let db = funded().account_code(A, then_call(site, BURNER, 0).stop().build());
        let tx = call(CALLER, A, U256::ZERO, BELOW_CAP);
        let burned = run_under(db.clone(), tx.clone(), u64::MAX);
        assert!(burned.gas.regular > BELOW_CAP / 2, "{name}: the burner burns: {:?}", burned.gas);
        assert_eq!(burned.gas.state, held, "{name}: the site's state gas");

        let stopped = run_under(db, tx, held - 1);
        assert_state_stopped(name, &stopped, held - 1, held);
        assert!(stopped.gas.regular < 1_000_000, "{name}: nothing burned: {:?}", stopped.gas);
        assert_eq!(stopped.gas.state, 0, "{name}");
    }
}

/// Deployed code whose state gas crosses the limit is not left deployed, whether the creation is
/// the transaction's own or a nested one: the limit is held before `return_create` commits it.
///
/// Rules: [S10.44]. Independence: constants — the created account and its code, from the
/// schedule.
#[test]
fn test_deployed_code_that_crosses_is_not_left_deployed() {
    if state_is_free() {
        return;
    }
    let deployed = constructor_returning(32);
    let held = deployment_state_gas(32);
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
///
/// Rules: [S10.35]. Independence: constants — the limit is the schedule's created account.
#[test]
fn test_a_reverting_creation_is_not_held_for_its_revert_data() {
    let reverting =
        BytecodeBuilder::default().push_number(32_u8).push_number(0_u8).append(REVERT).build();
    let account = created_account_state_gas();
    let db = funded()
        .account_code(A, then_create(BytecodeBuilder::default(), &reverting, false).stop().build());
    let outcome = run_under(db, call(CALLER, A, U256::ZERO, BELOW_CAP), account);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None);
    assert_eq!(outcome.gas.state, 0, "the failed creation's account charge came back");
}

/* ---------- a frame's upfront charge, held once revm has decided the frame ---------- */

/// Appends a `CALL` or `CALLCODE` to `target` with all the gas and `value`, dropping its success
/// flag.
fn then_call_with(
    builder: BytecodeBuilder,
    opcode: u8,
    target: Address,
    value: u64,
) -> BytecodeBuilder {
    builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(value)
        .push_address(target)
        .append(GAS)
        .append(opcode)
        .append(POP)
}

/// Appends a `CREATE`, or a `CREATE2` with salt zero, of empty init code carrying `value`,
/// dropping the address.
fn then_create_with(builder: BytecodeBuilder, create2: bool, value: u64) -> BytecodeBuilder {
    let builder = if create2 { builder.push_number(0_u64) } else { builder };
    builder
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(value)
        .append(if create2 { CREATE2 } else { CREATE })
        .append(POP)
}

/// A frame revm refuses once its caller's opcode charged the state gas of the account it would add
/// gives the charge back, and adds nothing: the limit does not hold it. Each case writes a fresh
/// slot after the refused frame and runs under limits the refused charge would cross — what the
/// transaction ends up holding, and one account less one gas — and succeeds exactly as it does
/// without a limit.
///
/// Only the value `CALL` its caller cannot fund reaches revm with the charge made: its opcode
/// charges the new account before the balance is known, and revm refuses the frame when it moves
/// the value. The other cases pin that nothing is held where nothing is charged: `CALLCODE` moves
/// value within its caller's own account and never adds one, and under EIP-8037 `CREATE` and
/// `CREATE2` refuse a creation their caller cannot fund before they charge it, and charge only a
/// destination with nothing at it, which cannot collide. A frame past the call-stack limit is
/// refused with the charge made too; no transaction reaches that depth under the execution cap,
/// and the unit tests of the frame lifecycle pin it.
///
/// Rules: [S10.43]. Independence: constants — the slot and the account a refused frame would
/// have added are the schedule's entries.
#[test]
fn test_a_frame_revm_refuses_is_not_held_for_its_upfront_charge() {
    if state_is_free() {
        return;
    }
    let (account, created) = (account_state_gas(), created_account_state_gas());
    let empty_init = Bytes::new();
    let taken = |address: Address| funded().account_nonce(address, 1);
    // (case, database, the site, the state gas of the account its frame would add)
    let cases: [(&str, MemoryDatabase, BytecodeBuilder, u64); 6] = [
        (
            "a CALL its caller cannot fund",
            funded(),
            then_call_with(BytecodeBuilder::default(), CALL, EMPTY, 2_000),
            account,
        ),
        (
            "a CALLCODE its caller cannot fund",
            funded(),
            then_call_with(BytecodeBuilder::default(), CALLCODE, EMPTY, 2_000),
            account,
        ),
        (
            "a CREATE its caller cannot fund",
            funded(),
            then_create_with(BytecodeBuilder::default(), false, 2_000),
            created,
        ),
        (
            "a CREATE2 its caller cannot fund",
            funded(),
            then_create_with(BytecodeBuilder::default(), true, 2_000),
            created,
        ),
        (
            "a CREATE whose address is taken",
            taken(A.create(0)),
            then_create_with(BytecodeBuilder::default(), false, 0),
            created,
        ),
        (
            "a CREATE2 whose address is taken",
            taken(A.create2_from_code(B256::ZERO, &empty_init)),
            then_create_with(BytecodeBuilder::default(), true, 0),
            created,
        ),
    ];
    for (name, db, site, account) in cases {
        let db = db.account_code(A, write_slots(site, 0, 1).stop().build());
        let tx = call(CALLER, A, U256::ZERO, BELOW_CAP);
        let free = run_under(db.clone(), tx.clone(), u64::MAX);
        assert!(free.result.is_success(), "{name}: {:?}", free.result);
        assert_eq!(free.gas.state, slot_state_gas(), "{name}: the slot alone");

        for limit in [free.gas.state, account - 1] {
            let limited = run_under(db.clone(), tx.clone(), limit);
            assert!(limited.result.is_success(), "{name}, limit {limit}: {:?}", limited.result);
            assert_eq!(limited.limit_exceeded, None, "{name}, limit {limit}");
            assert_eq!(limited.gas, free.gas, "{name}, limit {limit}: as without the limit");
            assert!(
                limited.state.get(&EMPTY).is_none_or(|account| account.info.balance.is_zero()),
                "{name}, limit {limit}: nothing moved"
            );
        }
    }
}

/// The result of each call to [`EMPTY`] an inspector saw end.
#[derive(Default)]
struct CallsToEmpty(Vec<(InstructionResult, Bytes)>);

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for CallsToEmpty {
    fn call_end(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if inputs.target_address == EMPTY {
            self.0.push((outcome.result.result, outcome.result.output.clone()));
        }
    }
}

/// A frame revm decides to run, or answers with a success, adds the account its caller's opcode
/// was charged for, so the charge is held, and a crossing latches the transaction — one case per
/// opcode family. A value call to an account with no code is answered by revm without running and
/// rewritten to the stop at once, so an inspector sees the answer the caller gets. The stop keeps
/// none of what the frame's start wrote: no value moved, no account created, and the history of
/// its records given back.
///
/// Rules: [S10.42], [S11.19]. Independence: constants — the account each frame adds is the
/// schedule's entry.
#[test]
fn test_a_frame_revm_decides_is_held_for_its_upfront_charge() {
    if state_is_free() {
        return;
    }
    let account = account_state_gas();
    let body = mega_evm::history_gas(TX_BODY_SIZE).expect("a body has a price");
    let cases: [(&str, BytecodeBuilder, u64); 3] = [
        (
            "a value CALL that creates its recipient",
            then_call(BytecodeBuilder::default(), EMPTY, 1),
            account,
        ),
        (
            "a CREATE",
            then_create_with(BytecodeBuilder::default(), false, 0),
            created_account_state_gas(),
        ),
        (
            "a CREATE2",
            then_create_with(BytecodeBuilder::default(), true, 0),
            created_account_state_gas(),
        ),
    ];
    for (name, site, account) in cases {
        let db = funded().account_code(A, site.stop().build());
        let stopped = run_under(db.clone(), call(CALLER, A, U256::ZERO, BELOW_CAP), account - 1);
        assert_state_stopped(name, &stopped, account - 1, account);
        assert_eq!(stopped.gas.state, 0, "{name}");
        assert_eq!(stopped.gas.history, body, "{name}: the records' history came back");
        assert!(stopped.state.get(&EMPTY).is_none_or(|account| account.info.balance.is_zero()));
        assert!(stopped.state.get(&A.create(0)).is_none_or(|account| !account.is_created()));
    }

    let db =
        funded().account_code(A, then_call(BytecodeBuilder::default(), EMPTY, 1).stop().build());
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(account - 1),
    ))
    .with_inspector(CallsToEmpty::default());
    let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, BELOW_CAP)).unwrap();
    let stop = outcome.limit_exceeded.expect("the call is stopped");
    assert_eq!(evm.inspector().0, [(InstructionResult::Revert, stop.revert_data())]);
}

/// The records a frame's start makes are held before revm builds the frame, and its upfront state
/// gas after revm has decided it. A value call whose records and transfer log cross the data-size
/// limit is answered with that stop, adds no account, and gives its upfront charge back: the
/// state-gas limit that charge would have crossed is not.
///
/// Rules: [S10.49], [S10.50]. Independence: constants — the limit the upfront charge would have
/// crossed is the schedule's new account.
#[test]
fn test_a_frame_start_whose_records_cross_is_stopped_for_its_records() {
    if state_is_free() {
        return;
    }
    let account = account_state_gas();
    let limit = TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE + mega_evm::TRANSFER_LOG_SIZE - 1;
    let db =
        funded().account_code(A, then_call(BytecodeBuilder::default(), EMPTY, 1).stop().build());
    let outcome = MegaEvm::new(
        context(db).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_state_gas_limit(account - 1)
                .with_tx_data_size_limit(limit),
        ),
    )
    .execute_transaction(call(CALLER, A, U256::ZERO, BELOW_CAP))
    .unwrap();
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit,
            used: limit + 1,
            frame_local: false,
        }),
        "the caller's account, the recipient's and the transfer log, over the limit by one byte",
    );
    assert_eq!(outcome.gas.state, 0);
}

/* ---------- the first frame's upfront charge, held once revm has decided the frame ---------- */

/// The identity precompile, which answers any input with a success.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");
/// The BN254 pairing precompile, which rejects an input that is not a whole number of pairs.
const BN254_PAIRING: Address = address!("0000000000000000000000000000000000000008");

/// A transaction of one wei from [`CALLER`] to `to` carrying `data`.
fn one_wei_to(to: Address, data: &[u8]) -> MegaTransaction {
    call_with_value_and_data(CALLER, to, U256::from(1), Bytes::copy_from_slice(data), BELOW_CAP)
}

/// The transaction's own frame is held for the account EIP-2780 charges its start for as every
/// frame is held for its opcode's upfront charge: once revm has decided the frame. A first frame
/// revm answers with a failure — a value transfer to a precompile that rejects its input — adds no
/// account, and its charge comes back, so no limit holds it: the transaction halts exactly as it
/// does without a limit, and nothing moves.
///
/// Rules: [S10.43]. Independence: independent at a limit of nothing; constants at one account
/// less one gas, the schedule's new account.
#[test]
fn test_a_first_frame_revm_answers_with_a_failure_is_not_held_for_its_upfront_charge() {
    // Where a state byte is free the start is charged nothing for the account, and there is no
    // charge to give back.
    if state_is_free() {
        return;
    }
    let account = account_state_gas();
    let tx = one_wei_to(BN254_PAIRING, &[1]);
    let free = run_under(funded(), tx.clone(), u64::MAX);
    assert!(free.result.is_halt(), "the pairing rejects a one-byte input: {:?}", free.result);
    assert_eq!(free.gas.state, 0, "the account charge came back");
    for limit in [account - 1, 0] {
        let limited = run_under(funded(), tx.clone(), limit);
        assert_eq!(limited.limit_exceeded, None, "limit {limit}");
        assert_eq!(limited.result, free.result, "limit {limit}: as without the limit");
        assert_eq!(limited.gas, free.gas, "limit {limit}");
        assert!(
            limited.state.get(&BN254_PAIRING).is_none_or(|account| account.info.balance.is_zero()),
            "limit {limit}: nothing moved"
        );
    }
}

/// A first frame revm builds, or answers with a success, adds the account EIP-2780 charged its
/// start for, so the charge is held once revm has decided the frame, and a crossing stops the
/// transaction: a built frame before its first instruction, an answer by being rewritten to the
/// stop. The stop keeps none of what the start wrote — no value moved, no account added, the
/// record and the transfer log not counted, the record's history given back — and bills what ran,
/// as a stop below the first frame does: a precompile answered with a success ran, and its price
/// stays spent. A creation transaction keeps its creator's nonce bump, so it cannot be replayed.
///
/// Rules: [S10.42], [S11.19]. Independence: constants — the account each start adds is the
/// schedule's entry.
#[test]
fn test_a_first_frame_revm_decides_is_held_for_its_upfront_charge() {
    // Where a state byte is free the start is charged nothing for the account, and there is no
    // charge to hold.
    if state_is_free() {
        return;
    }
    let account = account_state_gas();
    let word = [7_u8; 32];
    // (case, the transaction, the account its start adds, its calldata's length, that account's
    // state gas)
    let cases = [
        ("a value transfer to an account with no code", one_wei_to(EMPTY, &[]), EMPTY, 0, account),
        (
            "a value transfer to a precompile that answers",
            one_wei_to(IDENTITY, &word),
            IDENTITY,
            32,
            account,
        ),
        (
            "a creation transaction",
            create(CALLER, Bytes::new(), BELOW_CAP),
            CALLER.create(0),
            0,
            created_account_state_gas(),
        ),
    ];
    for (name, tx, added, calldata, account) in cases {
        let free = run_under(funded(), tx.clone(), u64::MAX);
        assert!(free.result.is_success(), "{name}: {:?}", free.result);
        assert_eq!(free.gas.state, account, "{name}: the account EIP-2780 charged");

        let stopped = run_under(funded(), tx, account - 1);
        assert_state_stopped(name, &stopped, account - 1, account);
        assert_eq!(stopped.gas.state, 0, "{name}");
        let body = mega_evm::history_gas(TX_BODY_SIZE + calldata).expect("a body has a price");
        assert_eq!(stopped.gas.history, body, "{name}: the record's history came back");
        assert_eq!(stopped.gas.regular, free.gas.regular, "{name}: what ran is billed");
        assert_eq!(
            stopped.usage,
            LimitUsage { data_size: TX_BODY_SIZE + calldata, write_records: 0 },
            "{name}: the body alone"
        );
        let account = stopped.state.get(&added);
        assert!(
            account.is_none_or(|account| account.info.balance.is_zero() && !account.is_created()),
            "{name}: nothing moved, nothing added"
        );
        assert_eq!(stopped.state[&CALLER].info.nonce, 1, "{name}: the sender's nonce");
    }
}

/// The first frame's start is held as every frame start is: its record and transfer log before
/// revm builds the frame, its upfront state gas after revm has decided it. A value transfer whose
/// record and transfer log cross the data-size limit is stopped for them and adds no account, so
/// the state-gas limit its upfront charge would have crossed is not what stops it.
///
/// Rules: [S10.49]. Independence: constants — the limit the upfront charge would have crossed is
/// the schedule's new account.
#[test]
fn test_a_first_frame_start_whose_records_cross_is_stopped_for_its_records() {
    // Where a state byte is free the start is charged nothing for the account, and there is no
    // state-gas crossing for the records' stop to come before.
    if state_is_free() {
        return;
    }
    let account = account_state_gas();
    let limit = TX_BODY_SIZE + WRITE_RECORD_SIZE + mega_evm::TRANSFER_LOG_SIZE - 1;
    let outcome = MegaEvm::new(
        context(funded()).with_tx_runtime_limits(
            EvmTxRuntimeLimits::no_limits()
                .with_tx_state_gas_limit(account - 1)
                .with_tx_data_size_limit(limit),
        ),
    )
    .execute_transaction(one_wei_to(EMPTY, &[]))
    .unwrap();
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit,
            used: limit + 1,
            frame_local: false,
        }),
        "the recipient's record and the transfer log, over the limit by one byte",
    );
    assert_eq!(outcome.gas.state, 0);
    assert!(outcome.state.get(&EMPTY).is_none_or(|account| account.info.balance.is_zero()));
}

/// Answers every call to [`EMPTY`] itself, with a success.
struct AnswersEmpty;

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for AnswersEmpty {
    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (inputs.target_address == EMPTY).then(|| {
            CallOutcome::new(
                InterpreterResult::new(
                    InstructionResult::Stop,
                    Bytes::new(),
                    Gas::new(inputs.gas_limit),
                ),
                inputs.return_memory_offset.clone(),
            )
        })
    }
}

/// A first frame an inspector answers in place of running never started: it adds no account, and
/// its upfront charge comes back whatever the answer. So no limit holds that charge, and the
/// transaction keeps the inspector's answer.
///
/// Rules: [S10.42]. Independence: constants — the limit the upfront charge would cross is the
/// schedule's new account.
#[test]
fn test_a_first_frame_an_inspector_answers_is_not_held_for_its_upfront_charge() {
    // Where a state byte is free the start is charged nothing for the account, and there is no
    // charge to give back.
    if state_is_free() {
        return;
    }
    let account = account_state_gas();
    let mut evm = MegaEvm::new(context(funded()).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits().with_tx_state_gas_limit(account - 1),
    ))
    .with_inspector(AnswersEmpty);
    let outcome = evm.execute_transaction(one_wei_to(EMPTY, &[])).unwrap();
    assert_eq!(outcome.limit_exceeded, None);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0);
    assert!(outcome.state.get(&EMPTY).is_none_or(|account| account.info.balance.is_zero()));
}

/* ---------- one limit for the whole call stack ---------- */

/// A child's crossing stops the whole transaction: there is no frame budget to revert it alone.
/// What its callers hold counts: `A`, `B` and `C` each write one slot, and a limit one gas short
/// of three slots is crossed in `C`, with the three slots as the figure.
///
/// Rules: [S10.4], [S10.35]. Independence: constants — the slot is the schedule's entry.
#[test]
fn test_a_childs_crossing_latches_the_transaction() {
    if state_is_free() {
        return;
    }
    let slot = slot_state_gas();
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
///
/// Rules: [S10.35], [S10.38]. Independence: constants — the authority's account and delegation
/// and the slot are the schedule's entries.
#[test]
fn test_charges_before_the_first_frame_and_in_it_add_up() {
    if state_is_free() {
        return;
    }
    let slot = slot_state_gas();
    let (account, delegation) = account_and_delegation();
    let authority = account + delegation;
    let held = authority + slot;
    let db = funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
    let tx = authorizing_call(CALLER, A, U256::ZERO, BELOW_CAP, DELEGATE, &[(AUTHORITY_1, 0)]);

    let stopped = run_under(db, tx, held - 1);
    assert_state_stopped("the slot after the authority", &stopped, held - 1, held);
    assert_eq!(
        stopped.state[&AUTHORITY_1].info.nonce, 1,
        "the authority fitted, and the stop in the frame does not take it back"
    );
    assert_eq!(stopped.gas.state, authority, "the authority's state gas stays with it");
}

/// A stop takes back what the frames did and keeps what was applied before the first frame. An
/// admitted authority keeps its nonce, its delegation, its state gas, its write record and the
/// history that record cost, as it does when its frame does nothing; the value the first frame
/// carried and that frame's own record come back with the frame.
///
/// Rules: [S11.8], [S11.14]. Independence: constants — the authority's state gas and the slot are
/// the schedule's entries; what the authority keeps is also compared with a run whose frame does
/// nothing.
#[test]
fn test_a_stop_keeps_what_was_applied_before_the_first_frame() {
    if state_is_free() {
        return;
    }
    let delegation = |outcome: &MegaTransactionOutcome| {
        outcome.state[&AUTHORITY_1].info.code.as_ref().map(|code| code.original_bytes())
    };
    let (account, delegation_state_gas) = account_and_delegation();
    let authority = account + delegation_state_gas;
    let held = authority + slot_state_gas();
    for value in [U256::ZERO, U256::from(1)] {
        let writes =
            funded().account_code(A, write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
        let idle = funded().account_code(A, BytecodeBuilder::default().stop().build());
        let tx = authorizing_call(CALLER, A, value, BELOW_CAP, DELEGATE, &[(AUTHORITY_1, 0)]);
        let stopped = run_under(writes, tx.clone(), held - 1);
        assert_state_stopped("the slot after the authority", &stopped, held - 1, held);
        let applied = run_under(idle, tx, u64::MAX);
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
/// deposit's mint and the state gas charged for the account.
///
/// Rules: [S10.39], [S11.13]. Independence: constants — the caller's account and the slot are the
/// schedule's entries.
#[test]
fn test_a_stopped_deposit_keeps_the_caller_it_created() {
    if state_is_free() {
        return;
    }
    let fresh = address!("00000000000000000000000000000000006000ff");
    let deposit = |code: Bytes| {
        let mut tx = call(fresh, A, U256::ZERO, BELOW_CAP);
        tx.0.deposit.source_hash = B256::repeat_byte(0x11);
        tx.0.deposit.mint = Some(5);
        tx.0.base.gas_price = 0;
        (funded().account_code(A, code), tx)
    };
    let (writes, tx) = deposit(write_slots(BytecodeBuilder::default(), 0, 1).stop().build());
    let held = account_state_gas() + slot_state_gas();
    let stopped = run_under(writes, tx, held - 1);
    assert_state_stopped("the slot after the caller", &stopped, held - 1, held);
    let (idle, tx) = deposit(BytecodeBuilder::default().stop().build());
    let created = run_under(idle, tx, u64::MAX);
    assert!(created.result.is_success(), "{:?}", created.result);

    let caller = &stopped.state[&fresh].info;
    assert_eq!((caller.balance, caller.nonce), (U256::from(5), 1), "the mint and the nonce stay");
    assert_eq!(stopped.gas.state, created.gas.state, "the caller's state gas stays with it");
    assert_eq!(stopped.gas.state, account_state_gas(), "the caller's account");
}

/// A stop reports as used the state gas held where the limit was crossed. Before the first frame
/// that is what stands whatever the frame does, a deposit's created caller, and not the account
/// EIP-2780 charges the first frame's start for, which is held once revm has decided the frame.
/// So a deposit that creates its caller and sends value to an account that does not exist
/// reports the caller alone when the caller crosses the limit, and both accounts when only both
/// do.
///
/// Rules: [S10.39]. Independence: constants — each account is the schedule's entry.
#[test]
fn test_a_stop_reports_the_state_gas_held_where_it_crossed() {
    if state_is_free() {
        return;
    }
    let fresh = address!("00000000000000000000000000000000006000fe");
    let deposit = || {
        let mut tx = call(fresh, EMPTY, U256::from(1), BELOW_CAP);
        tx.0.deposit.source_hash = B256::repeat_byte(0x11);
        tx.0.deposit.mint = Some(5);
        tx.0.base.gas_price = 0;
        tx
    };
    let account = account_state_gas();
    let caller_alone = run_under(funded(), deposit(), account - 1);
    assert_state_stopped("the created caller", &caller_alone, account - 1, account);
    let both = run_under(funded(), deposit(), 2 * account - 1);
    assert_state_stopped("the caller and the recipient", &both, 2 * account - 1, 2 * account);
    for stopped in [&caller_alone, &both] {
        assert!(
            stopped.state.get(&EMPTY).is_none_or(|a| a.info.balance.is_zero()),
            "no value moved"
        );
        assert_eq!(
            stopped.gas.state, account,
            "the caller's account stays, the recipient's does not"
        );
    }
}

/* ---------- refills give their room back ---------- */

/// The limit holds what the transaction holds net: a slot written back gives its room back, and
/// so do a failed child's writes, a creation that failed, and a slot a delegate writes back
/// before writing another. Each case holds two slots' worth of charges over its run, and fits a
/// limit of one.
///
/// Rules: [S10.35]. Independence: constants — the slot and the created account are the
/// schedule's entries.
#[test]
fn test_what_is_given_back_gives_its_room_back() {
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
        let outcome = run_under(db, call(CALLER, A, U256::ZERO, BELOW_CAP), slot);
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{name}");
        assert_eq!(outcome.gas.state, slot, "{name}");
    }

    // A creation that fails gives its account back to its creator, which then has room for a
    // slot under a limit of one account.
    let account = created_account_state_gas();
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
///
/// Rules: [S10.45]. Independence: constants — the slot is the schedule's entry.
#[test]
fn test_a_crowded_bucket_reaches_the_limit_sooner() {
    if state_is_free() {
        return;
    }
    use crate::salt::{crowded_slot, minimal_envs, salt_context};

    let slot = slot_state_gas();
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
        let outcome = run_under(
            db.account_code(A, Bytes::from_static(&[STOP])),
            authorizing(A, U256::ZERO, &authorizations),
            u64::MAX,
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
    let outcome = run_under(
        funded(),
        authorizing(AUTHORITY_1, U256::from(1), &[(Some(AUTHORITY_1), 0, 0)]),
        u64::MAX,
    );
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, account + delegation);
    assert_eq!(outcome.usage.write_records, 1);

    let unauthorized =
        run_under(funded(), call(CALLER, AUTHORITY_1, U256::from(1), BELOW_CAP), u64::MAX);
    assert_eq!(unauthorized.gas.state, account, "the same transfer without it pays the account");
}

/// Authorities that cross a limit are not applied, whichever limit it is: the limit is enforced
/// before the writes it guards, the gas they charged is taken back, and the transaction is stopped
/// at its first frame. Their state gas is held first, then their records' data size and KV count,
/// so a transaction that crosses more than one limit reports the first of them. Taking them back
/// forgoes the refund an authority that existed would have earned: two transactions that differ
/// in that alone spend the same gas.
#[test]
fn test_authorities_crossing_any_limit_are_not_applied() {
    if state_is_free() {
        return;
    }
    let (account, delegation) = account_and_delegation();
    let both = [(Some(AUTHORITY_1), 0, 0), (Some(AUTHORITY_2), 0, 0)];
    let fresh = || funded().account_code(A, Bytes::from_static(&[STOP]));
    let existing = || {
        fresh()
            .account_balance(AUTHORITY_1, U256::from(1))
            .account_balance(AUTHORITY_2, U256::from(1))
    };
    let held = 2 * (account + delegation);
    let body = TX_BODY_SIZE + 2 * mega_evm::AUTHORIZATION_SIZE;
    let limits = EvmTxRuntimeLimits::no_limits;
    let state_stop = |limit, used| LimitCheck::ExceedsLimit {
        kind: LimitKind::StateGrowth,
        limit,
        used,
        frame_local: false,
    };
    // (case, database, authorizations, limits, the stop)
    type Case = (&'static str, MemoryDatabase, Vec<Authorization>, EvmTxRuntimeLimits, LimitCheck);
    let cases: [Case; 6] = [
        (
            "a new authority under no state gas at all",
            fresh(),
            vec![both[0]],
            limits().with_tx_state_gas_limit(0),
            state_stop(0, account + delegation),
        ),
        (
            "two new authorities one gas short",
            fresh(),
            both.to_vec(),
            limits().with_tx_state_gas_limit(held - 1),
            state_stop(held - 1, held),
        ),
        (
            "two new authorities over the state-gas and the KV limit",
            fresh(),
            both.to_vec(),
            limits().with_tx_state_gas_limit(held - 1).with_tx_kv_update_limit(0),
            state_stop(held - 1, held),
        ),
        (
            "two existing authorities over the KV limit",
            existing(),
            both.to_vec(),
            limits().with_tx_kv_update_limit(1),
            LimitCheck::ExceedsLimit {
                kind: LimitKind::KVUpdate,
                limit: 1,
                used: 2,
                frame_local: false,
            },
        ),
        (
            "two new authorities over the data-size limit",
            fresh(),
            both.to_vec(),
            limits().with_tx_data_size_limit(body + 2 * WRITE_RECORD_SIZE - 1),
            LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit: body + 2 * WRITE_RECORD_SIZE - 1,
                used: body + 2 * WRITE_RECORD_SIZE,
                frame_local: false,
            },
        ),
        (
            "two existing authorities under no state gas at all",
            existing(),
            both.to_vec(),
            limits().with_tx_state_gas_limit(0),
            state_stop(0, 2 * delegation),
        ),
    ];
    for (name, db, authorizations, limits, stop) in cases {
        let stopped = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .execute_transaction(authorizing(A, U256::ZERO, &authorizations))
            .unwrap();
        assert!(!stopped.result.is_success() && !stopped.result.is_halt(), "{name}");
        assert_eq!(stopped.limit_exceeded, Some(stop), "{name}");
        assert_eq!(stopped.result.output().unwrap(), &stop.revert_data(), "{name}");
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
        MegaEvm::new(context(db).with_tx_runtime_limits(limits().with_tx_state_gas_limit(0)))
            .execute_transaction(authorizing(A, U256::ZERO, &both))
            .unwrap()
            .gas
            .gas_used
    };
    assert_eq!(spent(existing()), spent(fresh()), "no refund for an authority never applied");

    let applied = run_under(fresh(), authorizing(A, U256::ZERO, &both), held);
    assert!(applied.result.is_success(), "a limit they fit applies them: {:?}", applied.result);
    assert_eq!(applied.usage.write_records, 2);
}

/* ---------- destructions ---------- */

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
        let outcome = run_under(db, call(CALLER, A, U256::ZERO, BELOW_CAP), u64::MAX);
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.gas.state, state_gas, "{name}: the state gas");
        assert_eq!(outcome.usage.write_records, records, "{name}: the records");
    }
}

/* ---------- fresh slots ---------- */

/// Three fresh slots fit a limit of three slots' state gas exactly; a fourth crosses it, on the
/// fourth `SSTORE`, and none of the four is kept.
///
/// Rules: [S10.40]. Independence: constants — the slot is the schedule's entry.
#[test]
fn test_fresh_slots_fit_a_limit_of_their_state_gas_and_one_more_stops() {
    if state_is_free() {
        return;
    }
    let slot = slot_state_gas();
    let run = |slots| {
        run_under(
            funded()
                .account_code(A, write_slots(BytecodeBuilder::default(), 0, slots).stop().build()),
            call(CALLER, A, U256::ZERO, BELOW_CAP),
            3 * slot,
        )
    };
    let fits = run(3);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.gas.state, 3 * slot);

    let stopped = run(4);
    assert_state_stopped("a fourth slot", &stopped, 3 * slot, 4 * slot);
    let written =
        stopped.state.get(&A).map_or(0, |a| a.storage.values().filter(|v| v.is_changed()).count());
    assert_eq!(written, 0, "none of the slots is kept");
}

/* ---------- a charge the frame cannot pay ---------- */

/// A state charge the frame cannot pay is an out-of-gas whatever the state-gas limit, even one the
/// charge would cross [S10.37]: the limit is checked after the charge, so a charge that is never
/// made is never held.
///
/// Each case is a transaction below the execution cap, so there is no reservoir, whose gas limit
/// pays everything before its state charge and leaves it one gas short of that charge: a fresh
/// slot's `SSTORE`, a `SELFDESTRUCT` that moves a balance to an account that does not exist, a
/// value transaction to an account that does not exist, whose start EIP-2780 charges for the new
/// account, and, from `A`, a value `CALL` to an account that does not exist and a `CREATE`, whose
/// opcodes charge the account the frame they start adds. Under a state-gas limit of one gas and
/// of one gas short of the charge, it halts out of gas, consumes its whole gas limit, keeps no
/// state gas, and reports no stop: not a stop that would give the unspent gas back.
///
/// Once the state charge is paid, a frame start still charges the caller the history of the two
/// records the frame makes, after the gas it forwards is computed [S7.23]; one gas short of what
/// the start costs in all is an out-of-gas too, and the frame does not start [S7.24]. A `CREATE`
/// charges the created account before its 63/64 split, as EIP-8037 has it, so the records come out
/// of the 64th the creator keeps, and the creator must hold 64 times their price after the state
/// charge. Where a value `CALL`'s state charge falls against its forward and its records is not
/// fixed, so the `CALL` forwards an explicit nothing — the same forward in any order — and its rows
/// hold whatever the order: a gas limit short of the state charge, and one gas short of the whole
/// start, runs out of gas.
///
/// With exactly what the transaction needs to the end of its state charge — and of the start's
/// records — the same limit stops the transaction there, with the charge as the state gas it
/// reports, which pins each gas limit as one gas short of the charge, or of the start, and nothing
/// else.
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
        for limit in [1, charge - 1] {
            let case = format!("{name}, limit {limit}");
            let mut rows = vec![(format!("{case}, one gas short of the state charge"), short)];
            if then > 0 {
                rows.push((format!("{case}, one gas short of the frame start"), start - 1));
            }
            for (row, gas_limit) in rows {
                let ran_out = run_under(db.clone(), tx(gas_limit), limit);
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
                assert_eq!(
                    ran_out.gas.gas_used, gas_limit,
                    "{row}: the whole gas limit is consumed"
                );
                assert_eq!(ran_out.gas.state, 0, "{row}: no state gas is kept");
            }

            let paid = run_under(db.clone(), tx(start), limit);
            assert_state_stopped(&format!("{case}, everything paid"), &paid, limit, charge);
        }
    }
}

/* ---------- which limit binds first ---------- */

/// A fresh slot is charged its state gas inside `SSTORE`, before its record is counted, so a slot
/// that crosses the state-gas and data-size limits at once is the state-gas limit's stop.
///
/// Rules: [S10.40], [S10.46]. Independence: constants — the slot is the schedule's entry.
#[test]
fn test_a_slot_crossing_both_limits_reports_the_state_gas() {
    if state_is_free() {
        return;
    }
    let slot = slot_state_gas();
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
///
/// No rule: an implementation contract, the per-transaction reset of the held state gas on a
/// reused EVM. Independence: constants — the slot is the schedule's entry.
#[test]
fn test_each_transaction_is_held_to_the_limit_from_zero() {
    let slot = slot_state_gas();
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
