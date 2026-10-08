//! Gas detention against EIP-7708 transfer logs: value that moves after a read of volatile data.
//!
//! The two meet where a frame starts and at `SELFDESTRUCT`. A frame start counts its transfer log
//! with its records before revm decides the frame — revm then moves the value, answers the frame
//! or refuses it on the caller's account — and a precompile started after a read runs on what the
//! compute limit leaves its frame. A `SELFDESTRUCT` counts its log with its beneficiary's record
//! once the opcode completed, and revm journals the move before the opcode's last charge, which
//! the cap can fail.
//!
//! Whatever detention does to the movement, the transfer logs a receipt carries are the ones the
//! data size kept: a transaction keeps its body, forty bytes a write record and 160 a transfer
//! log, and a transaction the cap stops keeps its body alone. A debug build also asserts at every
//! frame start that revm refused exactly the start predicted and journaled exactly the log
//! counted, so every case below runs that check too.
//!
//! Each case runs with a read of the block's timestamp first, and with `PUSH0`, which costs the
//! same two gas and reads nothing, in its place.

use std::collections::BTreeMap;

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{
        is_transfer_log, op_transaction, transfer_log, BytecodeBuilder, MemoryDatabase, OutcomeView,
    },
    transaction_body_bytes, EvmTxRuntimeLimits, LimitUsage, MegaEvm, MegaTransaction,
    TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE,
};
use revm::{bytecode::opcode::*, context::TxEnv, interpreter::InstructionResult};

use crate::{
    detention::{
        assert_stopped, context, intrinsic, memory_cost, op, run_on, spin, Calls, Charges, Run,
        BENEFICIARY, CALLER, CAP, CHILD, CONTRACT, TIERS,
    },
    withheld_gas::{costly_modexp_input, intrinsic_with},
};

/// A contract whose code stops.
const RECEIVER: Address = address!("0000000000000000000000000000000000d00020");
/// An account with no code.
const PAYEE: Address = address!("0000000000000000000000000000000000d00021");
/// A contract that destructs to [`RECEIVER`], holding [`VALUE`].
const DESTRUCTOR: Address = address!("0000000000000000000000000000000000d00022");

/// The identity precompile.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");
/// The modexp precompile.
const MODEXP: Address = address!("0000000000000000000000000000000000000005");
/// The BN254 pairing precompile.
const EC_PAIRING: Address = address!("0000000000000000000000000000000000000008");

/// What every movement moves.
const VALUE: u64 = 1_000;

/// A contract that copies its calldata into memory, then, after `first`, calls `to` with it,
/// [`VALUE`] and `gas` (all of it when `None`), and stores the call's status in slot 0.
fn calls_with_value(first: u8, to: Address, gas: Option<u32>) -> Bytes {
    let code = op(BytecodeBuilder::default(), first)
        .append_many([CALLDATASIZE, PUSH0, PUSH0, CALLDATACOPY])
        .append_many([PUSH0, PUSH0, CALLDATASIZE, PUSH0])
        .push_number(VALUE)
        .push_address(to);
    let code = match gas {
        Some(gas) => code.push_number(gas),
        None => code.append(GAS),
    };
    code.append(CALL).push_number(0_u8).append(SSTORE).stop().build()
}

/// The regular charges [`calls_with_value`] makes after its first instruction reads, with calldata
/// of `len` bytes, up to the call: the read's `POP`; `CALLDATASIZE`, two `PUSH0` and
/// `CALLDATACOPY` — its static gas, the copy and the memory it expands — the call's four pushes,
/// the value's and the address's, and `GAS`; and the call's own charges, `call`, less the stipend
/// the callee is given, which nobody paid and is not compute.
fn charges_to_the_call(len: u64, call: u64) -> Charges {
    let words = len.div_ceil(32);
    Charges::default().then(&[
        2,
        2,
        2,
        2,
        3,
        3 * words,
        memory_cost(words),
        2,
        2,
        2,
        2,
        3,
        3,
        2,
        call - 2_300,
    ])
}

/// A value call's own charges to a cold contract: the cold access and the transfer.
const CALL_TO_A_CONTRACT: u64 = 2_600 + 9_000;

/// A value call's own charges to a precompile whose account is empty: the warm access, the
/// transfer, and the account the transfer adds, whose regular price Satin keeps beside its state
/// gas.
const CALL_TO_AN_EMPTY_PRECOMPILE: u64 = 100 + 9_000 + 25_000;

/// The accounts every case runs against: [`CONTRACT`] running `code` and holding `held`, a
/// [`RECEIVER`] and a [`DESTRUCTOR`].
fn accounts(code: Bytes, held: u64) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_code(CONTRACT, code)
        .account_balance(CONTRACT, U256::from(held))
        .account_code(RECEIVER, Bytes::from_static(&[STOP]))
        .account_code(DESTRUCTOR, BytecodeBuilder::default().selfdestruct(RECEIVER).build())
        .account_balance(DESTRUCTOR, U256::from(VALUE))
}

/// A transaction from `caller` to `to` carrying `input` and `value`.
fn tx_to(
    caller: Address,
    to: Address,
    input: &[u8],
    value: u64,
    gas_limit: u64,
) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller,
        kind: TxKind::Call(to),
        gas_limit,
        data: input.to_vec().into(),
        value: U256::from(value),
        ..Default::default()
    }))
}

/// A call's target, how it ended, and the gas it spent as its caller got it back.
type Ended = (Address, InstructionResult, u64);

/// Runs `tx` against `db` under `limits`, with the recording inspector when `inspected`, whose
/// entry point starts frames through the same frame start: what the transaction did, and every
/// call the inspector saw end, in the order they ended.
fn run_tx(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
    inspected: bool,
) -> (Run, Vec<Ended>) {
    let evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
    if !inspected {
        return (run_on(&mut { evm }, tx), Vec::new());
    }
    let mut evm = evm.with_inspector(Calls::default());
    let run = run_on(&mut evm, tx);
    let ended = evm.inspector().calls.iter().map(|call| (call.target, call.result, call.spent));
    (run, ended.collect())
}

/// How the call to `target` ended, and what it spent.
fn ended(calls: &[Ended], target: Address) -> (InstructionResult, u64) {
    let (_, result, spent) = calls.iter().find(|call| call.0 == target).expect("the call ended");
    (*result, *spent)
}

/// Asserts that the data size `run` kept is its body, its write records and the transfer logs its
/// receipt carries, and nothing else, and returns those logs.
fn assert_counts_its_transfer_logs(run: &Run, body: u64, case: &str) -> Vec<alloy_primitives::Log> {
    let logs = run.outcome.result.logs().to_vec();
    assert!(logs.iter().all(is_transfer_log), "{case}: no contract emits a log here");
    let usage = run.outcome.usage;
    assert_eq!(
        usage.data_size,
        body + WRITE_RECORD_SIZE * usage.write_records + TRANSFER_LOG_SIZE * logs.len() as u64,
        "{case}: the data size kept is the body, the records and the receipt's transfer logs"
    );
    logs
}

/// Asserts the detained run is the plain one: its result, logs included, every ledger of its bill
/// and what it kept.
fn assert_as_without_read(detained: &Run, plain: &Run, case: &str) {
    assert!(detained.limit.is_some(), "{case}: the read set a limit");
    assert_eq!(plain.limit, None, "{case}: the push read nothing");
    assert_eq!(detained.outcome.limit_exceeded, None, "{case}: no stop");
    assert_eq!(detained.outcome.result, plain.outcome.result, "{case}: the result");
    assert_eq!(detained.outcome.gas, plain.outcome.gas, "{case}: the bill");
    assert_eq!(detained.outcome.usage, plain.outcome.usage, "{case}: what it kept");
}

/// The balance `address` holds after the transaction: zero for an account it did not touch.
fn balance(run: &Run, address: Address) -> U256 {
    run.outcome.state.get(&address).map(|account| account.info.balance).unwrap_or_default()
}

/// Puts the views of a detained run and of its plain twin into `views` under `case`.
fn view_pair(views: &mut BTreeMap<String, OutcomeView>, case: &str, detained: &Run, plain: &Run) {
    views.insert(format!("{case}, detained"), OutcomeView::new(&detained.outcome));
    views.insert(format!("{case}, plain"), OutcomeView::new(&plain.outcome));
}

/* ---------- a value call to a precompile ---------- */

/// A value `CALL` to modexp after a read, with all the gas: the call counts its transfer log
/// before revm runs the precompile, and the precompile runs on the allowance.
///
/// - Priced within the allowance, modexp computes, the value moves, and the receipt carries the log
///   the data size counted, as without the read.
/// - Priced past the allowance but within the forward, modexp is answered without running: nothing
///   moves and no log is journaled, and the transaction stops at the limit, billed its intrinsic
///   gas and its compute at the call, and keeping its body alone. Without the read the call
///   succeeds, and its log is kept.
/// - Sent with value by the beneficiary straight to modexp, the transaction's own frame is detained
///   from its start with the cap as its allowance: priced past it, the same stop, billed its
///   intrinsic gas alone, and the value the transaction carries does not move.
#[test]
fn test_a_value_call_to_a_precompile_after_a_read_keeps_the_log_it_counted() {
    let (within, above) = (costly_modexp_input(32, 32), costly_modexp_input(64, 0));
    let mut views = BTreeMap::new();
    for gas_limit in TIERS {
        let exec = |first, input: &[u8]| {
            let db = accounts(calls_with_value(first, MODEXP, None), VALUE);
            let tx = tx_to(CALLER, CONTRACT, input, 0, gas_limit);
            let body = transaction_body_bytes(&tx);
            (run_tx(db, tx, EvmTxRuntimeLimits::default(), true), body)
        };

        let ((detained, calls), body) = exec(TIMESTAMP, &within);
        let ((plain, _), _) = exec(PUSH0, &within);
        assert_as_without_read(&detained, &plain, "priced within the allowance");
        assert_eq!(ended(&calls, MODEXP).0, InstructionResult::Return, "modexp computed");
        let logs = assert_counts_its_transfer_logs(&detained, body, "priced within the allowance");
        assert_eq!(logs, [transfer_log(CONTRACT, MODEXP, U256::from(VALUE))]);
        assert_eq!(detained.outcome.usage.write_records, 3, "the two ends and the status");
        assert_eq!(balance(&detained, MODEXP), U256::from(VALUE));
        view_pair(
            &mut views,
            &format!("priced within the allowance, gas limit {gas_limit}"),
            &detained,
            &plain,
        );

        let ((detained, calls), body) = exec(TIMESTAMP, &above);
        let ((plain, _), _) = exec(PUSH0, &above);
        // The price is the charge past what the limit leaves the call, a precompile's warm access.
        let price = mega_evm::satin_precompiles().get(&MODEXP).unwrap().required_gas(&above);
        let charges = charges_to_the_call(above.len() as u64, CALL_TO_AN_EMPTY_PRECOMPILE)
            .then(&[price.unwrap()]);
        assert_stopped(&detained, intrinsic_with(&above, gas_limit), charges.left(CAP));
        // The answer is the stop, having spent nothing.
        assert_eq!(ended(&calls, MODEXP), (InstructionResult::Revert, 0));
        assert_eq!(detained.outcome.usage, LimitUsage { data_size: body, write_records: 0 });
        assert_counts_its_transfer_logs(&detained, body, "priced past the allowance");
        assert_eq!(balance(&detained, MODEXP), U256::ZERO, "nothing moved");
        assert!(plain.outcome.result.is_success());
        let logs = assert_counts_its_transfer_logs(&plain, body, "without the read");
        assert_eq!(logs, [transfer_log(CONTRACT, MODEXP, U256::from(VALUE))]);
        view_pair(
            &mut views,
            &format!("priced past the allowance, gas limit {gas_limit}"),
            &detained,
            &plain,
        );

        // The transaction's own frame: its value is counted with its start, before revm runs the
        // precompile on the cap.
        let sent = |caller| {
            let db = MemoryDatabase::default().account_balance(caller, U256::from(VALUE));
            let tx = tx_to(caller, MODEXP, &above, VALUE, gas_limit);
            let body = transaction_body_bytes(&tx);
            (run_tx(db, tx, EvmTxRuntimeLimits::default(), true), body)
        };
        let ((plain, calls), body) = sent(CALLER);
        assert!(plain.outcome.result.is_success());
        let logs = assert_counts_its_transfer_logs(&plain, body, "a transaction to modexp");
        assert_eq!(logs, [transfer_log(CALLER, MODEXP, U256::from(VALUE))]);
        let price = ended(&calls, MODEXP).1;
        let ((detained, calls), body) = sent(BENEFICIARY);
        assert_eq!(detained.limit, Some(CAP), "the sender is the beneficiary");
        assert_eq!(ended(&calls, MODEXP), (InstructionResult::Revert, 0));
        // Nothing is charged before the call: all the cap is left when its price does not fit it.
        let left = Charges::default().then(&[price]).left(CAP);
        assert_eq!(left, CAP);
        assert_stopped(&detained, plain.outcome.gas.regular - price, left);
        assert_eq!(detained.outcome.usage, LimitUsage { data_size: body, write_records: 0 });
        assert_eq!(balance(&detained, MODEXP), U256::ZERO, "the transaction's value did not move");
        views.insert(
            format!("a transaction to modexp from another sender, gas limit {gas_limit}"),
            OutcomeView::new(&plain.outcome),
        );
        views.insert(
            format!("a transaction to modexp from the beneficiary, gas limit {gas_limit}"),
            OutcomeView::new(&detained.outcome),
        );
    }
    crate::assert_sorted_json_snapshot!(&views);
}

/* ---------- a value call its caller cannot fund ---------- */

/// A value call its caller cannot fund, after a read: revm refuses it on the caller's account
/// before anything moves, so its start counts no record and no log and is charged nothing, and
/// detention settles the refusal as it settles any answer. A precompile forwarded more than the
/// allowance, run on the allowance, gets the rest of its forward back. The transaction runs as
/// without the read: the call fails with the whole forward back, nothing moves, no log, and the
/// transaction keeps its body alone.
#[test]
fn test_a_value_call_its_caller_cannot_fund_after_a_read_runs_as_without_it() {
    let cases: [(&str, Address, Option<u32>); 4] = [
        ("a contract", RECEIVER, None),
        ("an account with no code", PAYEE, None),
        ("a precompile forwarded more than the allowance", MODEXP, None),
        ("a precompile forwarded less than the allowance", IDENTITY, Some(100_000)),
    ];
    let mut views = BTreeMap::new();
    for gas_limit in TIERS {
        for (case, to, gas) in cases {
            for inspected in [false, true] {
                let exec = |first| {
                    let db = accounts(calls_with_value(first, to, gas), VALUE - 1);
                    let tx = tx_to(CALLER, CONTRACT, &[0xab; 64], 0, gas_limit);
                    run_tx(db, tx, EvmTxRuntimeLimits::default(), inspected)
                };
                let ((detained, calls), (plain, plain_calls)) = (exec(TIMESTAMP), exec(PUSH0));
                assert_as_without_read(&detained, &plain, case);
                let body = transaction_body_bytes(&tx_to(CALLER, CONTRACT, &[0xab; 64], 0, 0));
                assert_eq!(
                    detained.outcome.usage,
                    LimitUsage { data_size: body, write_records: 0 },
                    "{case}: the refused start counted nothing"
                );
                assert!(detained.outcome.result.logs().is_empty(), "{case}");
                assert_eq!(balance(&detained, CONTRACT), U256::from(VALUE - 1), "{case}");
                if inspected {
                    let (call, twin) = (ended(&calls, to), ended(&plain_calls, to));
                    assert_eq!(call.0, InstructionResult::OutOfFunds, "{case}");
                    assert_eq!((call.1, twin.1), (0, 0), "{case}: the forward came back");
                }
                view_pair(
                    &mut views,
                    &format!("{case}, inspected: {inspected}, gas limit {gas_limit}"),
                    &detained,
                    &plain,
                );
            }
        }
    }
    crate::assert_sorted_json_snapshot!(&views);
}

/* ---------- a SELFDESTRUCT ---------- */

/// What a frame computes from a timestamp read to a completed `SELFDESTRUCT` of its balance to a
/// cold contract: the read's `POP`, the beneficiary's `PUSH20`, the opcode's 5,000 and the cold
/// access of 2,600, which revm charges after it journaled the move and its transfer log.
const DESTRUCT_AFTER_READ: u64 = 2 + 3 + 5_000 + 2_600;

/// A `SELFDESTRUCT` of a balance, after a read, whose frame the cap stops.
///
/// - Destructing in the transaction's own frame under a cap that pays exactly the opcode, the
///   balance moves and the receipt carries the log the data size counted with the beneficiary's
///   record, as without the read. A unit less and the cold access, charged after revm journaled the
///   move and the log, crosses the cap: the opcode failed, so nothing is counted, and the stop
///   takes the move and the log back. Less than the opcode's own price, and its static charge
///   crosses before revm moves anything: the same stop.
/// - Destructing in a child, the record and the log land on the child's lane and merge into its
///   caller's. The caller then computes past the cap: the stop takes back the child's move and log
///   with the caller's lane, and the transaction keeps its body alone. Without the spin, the same
///   call keeps the log and the bytes, read or not.
#[test]
fn test_a_selfdestructs_transfer_log_goes_with_the_frame_the_cap_stops() {
    let own = |first| BytecodeBuilder::default().append_many([first, POP]).selfdestruct(RECEIVER);
    let calls_destructor =
        |first| crate::detention::call(op(BytecodeBuilder::default(), first), CALL, DESTRUCTOR);
    let mut views = BTreeMap::new();
    for gas_limit in TIERS {
        let tx = tx_to(CALLER, CONTRACT, &[], 0, gas_limit);
        let body = transaction_body_bytes(&tx);
        let under =
            |cap| EvmTxRuntimeLimits::default().with_block_env_access_compute_gas_limit(cap);

        // In the transaction's own frame.
        let run_own = |first, cap| {
            run_tx(accounts(own(first).build(), VALUE), tx.clone(), under(cap), false).0
        };
        let (detained, plain) =
            (run_own(TIMESTAMP, DESTRUCT_AFTER_READ), run_own(PUSH0, DESTRUCT_AFTER_READ));
        assert_as_without_read(&detained, &plain, "a cap that pays the destruction");
        let logs = assert_counts_its_transfer_logs(&detained, body, "a destruction the cap pays");
        assert_eq!(logs, [transfer_log(CONTRACT, RECEIVER, U256::from(VALUE))]);
        assert_eq!(detained.outcome.usage.write_records, 1, "the beneficiary's record");
        view_pair(
            &mut views,
            &format!("own frame under a cap that pays the destruction, gas limit {gas_limit}"),
            &detained,
            &plain,
        );
        for cap in [DESTRUCT_AFTER_READ - 1, DESTRUCT_AFTER_READ - 2_600 - 1] {
            let stopped = run_own(TIMESTAMP, cap);
            let left = Charges::default().then(&[2, 3, 5_000, 2_600]).left(cap);
            assert_stopped(&stopped, intrinsic(gas_limit), left);
            assert_eq!(stopped.limit, Some(2 + cap));
            assert_eq!(stopped.outcome.usage, LimitUsage { data_size: body, write_records: 0 });
            assert_eq!(balance(&stopped, RECEIVER), U256::ZERO, "under {cap}, nothing moved");
            views.insert(
                format!("own frame under a cap of {cap}, gas limit {gas_limit}"),
                OutcomeView::new(&stopped.outcome),
            );
        }

        // In a child, whose caller the cap stops afterwards.
        let run_child = |code: BytecodeBuilder| {
            run_tx(accounts(code.build(), 0), tx.clone(), EvmTxRuntimeLimits::default(), false).0
        };
        let (detained, plain) = (
            run_child(calls_destructor(TIMESTAMP).stop()),
            run_child(calls_destructor(PUSH0).stop()),
        );
        assert_as_without_read(&detained, &plain, "a child's destruction");
        let logs = assert_counts_its_transfer_logs(&detained, body, "a child's destruction");
        assert_eq!(logs, [transfer_log(DESTRUCTOR, RECEIVER, U256::from(VALUE))]);
        view_pair(
            &mut views,
            &format!("a child's destruction, gas limit {gas_limit}"),
            &detained,
            &plain,
        );
        let stopped = run_tx(
            accounts(spin(calls_destructor(TIMESTAMP)), 0),
            tx.clone(),
            EvmTxRuntimeLimits::default(),
            false,
        )
        .0;
        // The read's `POP`, the call's five pushes, its address, `GAS` and cold access, the child's
        // `PUSH20` and destruction to a cold account, the caller's `POP`, then its loop.
        let left = Charges::default()
            .then(&[2, 2, 2, 2, 2, 2, 3, 2, 2_600, 3, 5_000, 2_600, 2])
            .spin(0)
            .left(CAP);
        assert_stopped(&stopped, intrinsic(gas_limit), left);
        assert_eq!(stopped.outcome.usage, LimitUsage { data_size: body, write_records: 0 });
        assert_eq!(balance(&stopped, RECEIVER), U256::ZERO, "the child's move was taken back");
        views.insert(
            format!("a child's destruction the cap then stops, gas limit {gas_limit}"),
            OutcomeView::new(&stopped.outcome),
        );
    }
    crate::assert_sorted_json_snapshot!(&views);
}

/* ---------- every detained frame start ---------- */

/// A frame start after a read, as one case of
/// [`test_every_detained_frame_start_keeps_the_log_it_counted`].
struct Start {
    name: &'static str,
    /// [`CONTRACT`]'s code after `first`.
    code: fn(u8) -> Bytes,
    /// The transaction's calldata.
    input: fn() -> Vec<u8>,
    /// What [`CONTRACT`] holds.
    held: u64,
    /// When the cap stops the transaction: the regular charges it makes after the read, with
    /// calldata of the given length.
    stops: Option<fn(u64) -> Charges>,
}

/// Every path a frame start takes through detention, each moving value or refused on its caller's
/// account, after a read: a built frame, one the cap then stops, an answer without code, a
/// precompile run on its forward and on the allowance — computing, running out of the allowance, or
/// failing on its input past its gas check — a refused call and a refused creation, and the
/// creations and a destruction. Each runs at both tiers, with and without an inspector, whose
/// entry point reaches the same frame start.
///
/// Every transaction keeps the data size of the transfer logs its receipt carries: a debug build
/// asserts at each start that revm journaled exactly the log counted, and the receipt shows what
/// that means for the transaction. Every case the cap does not stop runs as without the read.
#[test]
fn test_every_detained_frame_start_keeps_the_log_it_counted() {
    let none = Vec::new;
    let starts = [
        Start {
            name: "a value call to a contract",
            code: |first| calls_with_value(first, RECEIVER, None),
            input: none,
            held: VALUE,
            stops: None,
        },
        Start {
            name: "a value call to a contract the cap then stops",
            code: |first| calls_with_value(first, CHILD, None),
            input: none,
            held: VALUE,
            // The call to the child's cold account, then the child's loop.
            stops: Some(|len| charges_to_the_call(len, CALL_TO_A_CONTRACT).spin(0)),
        },
        Start {
            name: "a value call to an account with no code",
            code: |first| calls_with_value(first, PAYEE, None),
            input: none,
            held: VALUE,
            stops: None,
        },
        Start {
            name: "a value call to a precompile forwarded less than the allowance",
            code: |first| calls_with_value(first, IDENTITY, Some(100_000)),
            input: || vec![0xab; 64],
            held: VALUE,
            stops: None,
        },
        Start {
            name: "a value call to a precompile the allowance pays",
            code: |first| calls_with_value(first, IDENTITY, None),
            input: || vec![0xab; 64],
            held: VALUE,
            stops: None,
        },
        Start {
            name: "a value call to a precompile priced past the allowance",
            code: |first| calls_with_value(first, MODEXP, None),
            input: || costly_modexp_input(64, 0),
            held: VALUE,
            // The call to the precompile's warm account, then its price.
            stops: Some(|len| {
                let input = costly_modexp_input(64, 0);
                let price =
                    mega_evm::satin_precompiles().get(&MODEXP).unwrap().required_gas(&input);
                charges_to_the_call(len, CALL_TO_AN_EMPTY_PRECOMPILE).then(&[price.unwrap()])
            }),
        },
        Start {
            name: "a value call to a precompile whose input fails past its gas check",
            code: |first| calls_with_value(first, EC_PAIRING, None),
            input: || vec![0; 3 * 192 + 1],
            held: VALUE,
            stops: None,
        },
        Start {
            name: "a value call its caller cannot fund",
            code: |first| calls_with_value(first, RECEIVER, None),
            input: none,
            held: VALUE - 1,
            stops: None,
        },
        Start {
            name: "a value call to a precompile its caller cannot fund",
            code: |first| calls_with_value(first, MODEXP, None),
            input: || costly_modexp_input(64, 0),
            held: VALUE - 1,
            stops: None,
        },
        Start {
            name: "a CREATE with an endowment",
            code: |first| {
                op(BytecodeBuilder::default(), first)
                    .create(U256::from(VALUE), [STOP])
                    .append(POP)
                    .stop()
                    .build()
            },
            input: none,
            held: VALUE,
            stops: None,
        },
        Start {
            name: "a CREATE2 with an endowment",
            code: |first| {
                op(BytecodeBuilder::default(), first)
                    .create2(U256::from(VALUE), [STOP], U256::from(7))
                    .append(POP)
                    .stop()
                    .build()
            },
            input: none,
            held: VALUE,
            stops: None,
        },
        Start {
            name: "a CREATE its creator cannot fund",
            code: |first| {
                op(BytecodeBuilder::default(), first)
                    .create(U256::from(VALUE), [STOP])
                    .append(POP)
                    .stop()
                    .build()
            },
            input: none,
            held: VALUE - 1,
            stops: None,
        },
        Start {
            name: "a call to a contract that destructs",
            code: |first| {
                crate::detention::call(op(BytecodeBuilder::default(), first), CALL, DESTRUCTOR)
                    .stop()
                    .build()
            },
            input: none,
            held: VALUE,
            stops: None,
        },
    ];
    let mut views = BTreeMap::new();
    for gas_limit in TIERS {
        for start in &starts {
            let input = (start.input)();
            let tx = tx_to(CALLER, CONTRACT, &input, 0, gas_limit);
            let body = transaction_body_bytes(&tx);
            for inspected in [false, true] {
                let case = format!("{} at {gas_limit}, inspected: {inspected}", start.name);
                let exec = |first| {
                    let db = accounts((start.code)(first), start.held)
                        .account_code(CHILD, spin(BytecodeBuilder::default()));
                    run_tx(db, tx.clone(), EvmTxRuntimeLimits::default(), inspected).0
                };
                let detained = exec(TIMESTAMP);
                assert_counts_its_transfer_logs(&detained, body, &case);
                if let Some(charges) = start.stops {
                    let left = charges(input.len() as u64).left(CAP);
                    assert_stopped(&detained, intrinsic_with(&input, gas_limit), left);
                    assert_eq!(detained.outcome.usage.write_records, 0, "{case}");
                    views.insert(format!("{case}, detained"), OutcomeView::new(&detained.outcome));
                } else {
                    let plain = exec(PUSH0);
                    assert_counts_its_transfer_logs(&plain, body, &case);
                    assert_as_without_read(&detained, &plain, &case);
                    view_pair(&mut views, &case, &detained, &plain);
                }
            }
        }
    }
    crate::assert_sorted_json_snapshot!(&views);
}
