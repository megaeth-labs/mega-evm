//! The history bytes a transaction appended, reported beside the history gas it paid.
//!
//! Every history charge is a byte count at the cost per history byte, and the transaction reports
//! the count as well as the gas. Without a history allowance the two are a price apart. A value
//! transfer's allowance pays for its callee's first event before the callee's gas does, and what
//! it pays for is on no gas ledger, so the byte count is larger than the gas says — by exactly
//! what the allowances paid.
//!
//! Every case runs twice: below the execution cap, where the reservoir is empty and every charge
//! spills onto regular gas, and above it, where the reservoir pays first.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::{COST_PER_HISTORY_BYTE, MAX_CONTRACT_SIZE, TX_GAS_LIMIT_CAP},
    system::{
        IMegaAccessControl, IOracle, ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE,
        ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE,
    },
    test_utils::{is_transfer_log, op_transaction, BytecodeBuilder, MemoryDatabase},
    untouched_create_gas, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaTransaction,
    MegaTransactionOutcome, ACCESS_LIST_ADDRESS_SIZE, ACCESS_LIST_SLOT_SIZE, AUTHORIZATION_SIZE,
    HISTORY_ALLOWANCE_BYTES, LOG_BASE_SIZE, LOG_TOPIC_SIZE, TRANSFER_LOG_SIZE, TX_BODY_SIZE,
    WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CALLDATASIZE, CREATE, DELEGATECALL, GAS, JUMPDEST, JUMPI, LOG0, MSTORE8, POP, PUSH0,
        PUSH1, RETURN, REVERT, SELFDESTRUCT,
    },
    context::{
        transaction::{AccessList, AccessListItem, TransactionType},
        TxEnv,
    },
    context_interface::{
        either::Either,
        transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    },
    interpreter::{
        interpreter::EthInterpreter, CreateInputs, CreateOutcome, InstructionResult,
        InterpreterResult,
    },
    Inspector,
};

#[cfg(debug_assertions)]
use crate::common::{answered_on_its_shares, first_frame_pools, frame_shares};
use crate::common::{call, call_with_data, context, create, execute, runs_at_measurement_prices};
#[cfg(debug_assertions)]
use mega_evm::test_utils::{take_start_ledger_divergences, tolerate_start_ledger_divergences};

const CALLER: Address = address!("0000000000000000000000000000000000a00000");
const CALLEE: Address = address!("0000000000000000000000000000000000a00001");
const CHILD: Address = address!("0000000000000000000000000000000000a00002");
const RECEIVER: Address = address!("0000000000000000000000000000000000a00003");
const OTHER_RECEIVER: Address = address!("0000000000000000000000000000000000a00004");
/// An account nothing has touched.
const FRESH: Address = address!("0000000000000000000000000000000000a00005");
/// An account that exists and has no code.
const PAYEE: Address = address!("0000000000000000000000000000000000a00006");

const CPHB: u64 = COST_PER_HISTORY_BYTE;

/// The reservoir the runs above the execution cap carry.
const RESERVOIR: u64 = 100_000_000;

/// The two gas limits every case runs at: below the execution cap and above it.
const GAS_LIMITS: [u64; 2] = [50_000_000, TX_GAS_LIMIT_CAP + RESERVOIR];

/// Bytes the deployments here leave behind.
const DEPLOYED: u64 = 32;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(10u64.pow(9)))
        .account_balance(PAYEE, U256::from(1))
}

/// A log with `topics` topics over the first `len` bytes of memory.
fn log(code: BytecodeBuilder, topics: u8, len: u64) -> BytecodeBuilder {
    let mut code = code;
    for topic in 0..topics {
        code = code.push_number(u64::from(topic) + 1);
    }
    code.push_number(len).push_number(0u64).append(LOG0 + topics)
}

/// The bytes a log with `topics` topics and `len` bytes of data appends.
const fn log_bytes(topics: u64, len: u64) -> u64 {
    LOG_BASE_SIZE + topics * LOG_TOPIC_SIZE + len
}

/// Init code that deploys [`DEPLOYED`] zero bytes: `PUSH1 32; PUSH0; RETURN`.
fn deploying() -> Bytes {
    BytecodeBuilder::default().push_number(DEPLOYED).append_many([PUSH0, RETURN]).build()
}

/// `CREATE` over `init_code` placed in memory, discarding the address.
fn creating(code: BytecodeBuilder, init_code: &[u8]) -> BytecodeBuilder {
    code.mstore(0, init_code)
        .push_number(init_code.len() as u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .append(POP)
}

/// `CALL(gas, target, value, 0, 0, 0, 0)`, discarding the flag.
fn calling(code: BytecodeBuilder, target: Address, value: u64, gas: u64) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(value)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
        .append(POP)
}

/// `funded()` with `code` at [`CALLEE`].
fn callee_running(code: BytecodeBuilder) -> MemoryDatabase {
    funded().account_code(CALLEE, code.stop().build())
}

/// Asserts what the reservoir of a run above the execution cap says about its two ledgers: what
/// the transaction spent on state and history came out of it, and nothing else did.
fn assert_reservoir_paid(name: &str, gas_limit: u64, outcome: &MegaTransactionOutcome) {
    if gas_limit <= TX_GAS_LIMIT_CAP {
        return;
    }
    let reservoir = gas_limit - TX_GAS_LIMIT_CAP;
    assert!(outcome.gas.reservoir_remaining > 0, "{name}: the reservoir is not exhausted");
    assert_eq!(
        outcome.gas.reservoir_remaining,
        reservoir - outcome.gas.state - outcome.gas.history,
        "{name}: the reservoir paid the state and history ledgers and nothing else",
    );
}

/// A callee that writes a slot, logs, pays an account it creates and deploys code: every history
/// site a frame reaches, and state gas beside it.
fn every_site() -> MemoryDatabase {
    let code = creating(
        calling(
            log(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), 3, 64),
            FRESH,
            1,
            1_000_000,
        ),
        &deploying(),
    );
    callee_running(code)
}

/// A call to [`every_site`]'s callee carrying ten bytes of calldata.
fn every_site_called(gas_limit: u64) -> MegaTransaction {
    call_with_data(CALLER, CALLEE, Bytes::from(vec![1; 10]), gas_limit)
}

/// A reservoir too small for what a transaction spends on state and history runs out, and the
/// rest spills onto regular gas. That changes who paid and nothing else: the bytes, the history
/// gas and the state gas are the ones an ample reservoir leaves, the regular ledger is too — a
/// spilled charge stays on its own ledger — and the reservoir is spent to the last gas.
#[test]
fn test_a_reservoir_that_runs_out_changes_only_who_paid() {
    if runs_at_measurement_prices() {
        return;
    }
    let ample = execute(every_site(), every_site_called(TX_GAS_LIMIT_CAP + RESERVOIR));
    assert!(ample.result.is_success(), "{:?}", ample.result);
    assert_reservoir_paid("an ample reservoir", TX_GAS_LIMIT_CAP + RESERVOIR, &ample);
    let spent = ample.gas.state + ample.gas.history;

    // Short of the body; short of everything but the body; short by one gas.
    for reservoir in [1, TX_BODY_SIZE * CPHB - 1, TX_BODY_SIZE * CPHB + 1, spent - 1] {
        let outcome = execute(every_site(), every_site_called(TX_GAS_LIMIT_CAP + reservoir));
        let name = format!("a reservoir of {reservoir}");
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.gas.reservoir_remaining, 0, "{name}: spent to the last gas");
        assert_eq!(outcome.gas.history_bytes, ample.gas.history_bytes, "{name}: the bytes");
        assert_eq!(outcome.gas.history, ample.gas.history, "{name}: the history gas");
        assert_eq!(outcome.gas.state, ample.gas.state, "{name}: the state gas");
        assert_eq!(outcome.gas.regular, ample.gas.regular, "{name}: the regular ledger");
        assert_eq!(
            outcome.gas.regular + outcome.gas.state + outcome.gas.history,
            outcome.result.gas().total_gas_spent(),
            "{name}: the three ledgers split the raw spend",
        );
    }

    let exact = execute(every_site(), every_site_called(TX_GAS_LIMIT_CAP + spent));
    assert_eq!(exact.gas.reservoir_remaining, 0, "a reservoir of exactly what is spent");
    assert_eq!(exact.gas.regular, ample.gas.regular);
}

/// A transaction whose gas cannot pay for the record its own frame makes runs out of gas before
/// that frame, and the record is not made: it reports its body alone, in bytes and in gas.
#[test]
fn test_a_transaction_that_cannot_pay_its_first_record_reports_its_body_alone() {
    if runs_at_measurement_prices() {
        return;
    }
    let transfer = |gas| call(CALLER, PAYEE, U256::from(1), gas);
    // What the transfer spends is its intrinsic gas and the one record, nothing else: that is the
    // least gas limit that pays for all of it.
    let ample = execute(funded(), transfer(GAS_LIMITS[0]));
    assert!(ample.result.is_success(), "{:?}", ample.result);
    let fits = ample.gas.gas_used;

    let exact = execute(funded(), transfer(fits));
    assert!(exact.result.is_success(), "{:?}", exact.result);
    assert_eq!(exact.gas.history_bytes, TX_BODY_SIZE + WRITE_RECORD_SIZE);
    assert_eq!(exact.gas.history, exact.gas.history_bytes * CPHB);

    let short = execute(funded(), transfer(fits - 1));
    assert!(short.result.is_halt(), "{:?}", short.result);
    assert_eq!(short.usage.write_records, 0, "no frame ran, so no record was made");
    assert_eq!(short.gas.history_bytes, TX_BODY_SIZE, "the body alone");
    assert_eq!(short.gas.history, TX_BODY_SIZE * CPHB);
}

/// Answers every creation itself with a success whose output is a word of bytes, so no frame runs
/// and nothing is deposited.
///
/// The answer's gas is the creation's, untouched, with the reservoir it inherited, as the
/// engine's own answers build it: a `Gas::new` of the forwarded limit would carry no reservoir,
/// and the caller that adopts it would lose its own.
struct AnswersCreations;

impl Inspector<MegaContext<MemoryDatabase>, EthInterpreter> for AnswersCreations {
    fn create(
        &mut self,
        _context: &mut MegaContext<MemoryDatabase>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        Some(CreateOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                Bytes::from(vec![0xfe; 32]),
                untouched_create_gas(inputs),
            ),
            Some(CHILD),
        ))
    }
}

/// A creation answered without running deposits nothing, whatever the answer's output: no byte of
/// code is counted. The answer is taken as given, as revm takes it: no record is counted, the
/// creator's nonce never having been bumped, and what the `CREATE` was charged for the records its
/// start would make stays charged, so the history ledger carries both records beyond the bytes the
/// transaction reports.
#[test]
fn test_a_creation_answered_without_running_appends_no_code() {
    if runs_at_measurement_prices() {
        return;
    }
    for gas_limit in GAS_LIMITS {
        let db = callee_running(creating(BytecodeBuilder::default(), &deploying()));
        let tx = call(CALLER, CALLEE, U256::ZERO, gas_limit);
        let mut evm = MegaEvm::new(context(db)).with_inspector(AnswersCreations);
        #[cfg(debug_assertions)]
        tolerate_start_ledger_divergences(evm.ctx_mut());
        let outcome = evm.execute_transaction(tx.clone()).expect("the transaction is valid");
        // The answer carries the creation's shares, pools the inspector set: an API removal of the
        // inspector table, where a debug build's start ledger, which settles an inspector's answer
        // at unlimited pools, parts from the fork.
        #[cfg(debug_assertions)]
        {
            let caller = first_frame_pools(&EvmTxRuntimeLimits::default(), &tx);
            let expected = answered_on_its_shares(2, 1, caller, frame_shares(caller));
            assert_eq!(take_start_ledger_divergences(evm.ctx_mut()), expected);
        }

        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(outcome.usage.write_records, 0, "no bump, no record");
        assert_eq!(outcome.gas.history_bytes, TX_BODY_SIZE, "the body alone: no record, no code");
        assert_eq!(
            outcome.gas.history,
            (TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE) * CPHB,
            "the two records' history stays charged"
        );
        assert_reservoir_paid("a creation answered without running", gas_limit, &outcome);
    }
}

/* ---------- the edge paths ---------- */

/// Two authorities, and the account an applied authorization delegates to.
const AUTHORITY: Address = address!("0000000000000000000000000000000000a00007");
const OTHER_AUTHORITY: Address = address!("0000000000000000000000000000000000a00008");
const DELEGATE: Address = address!("0000000000000000000000000000000000a00009");

/// A type-1 call from [`CALLER`] to [`CALLEE`] whose access list names each `(address, keys)`
/// with that many storage keys.
fn with_access_list(entries: &[(Address, u64)], gas_limit: u64) -> MegaTransaction {
    let items = entries.iter().map(|&(address, keys)| AccessListItem {
        address,
        storage_keys: (0..keys).map(|key| B256::from(U256::from(key))).collect(),
    });
    OpTx(op_transaction(TxEnv {
        tx_type: TransactionType::Eip2930 as u8,
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit,
        access_list: AccessList(items.collect()),
        ..Default::default()
    }))
}

/// A type-4 call from [`CALLER`] to [`CALLEE`] carrying authorizations `(authority, nonce)` that
/// delegate to [`DELEGATE`].
fn with_authorizations(authorizations: &[(Address, u64)], gas_limit: u64) -> MegaTransaction {
    let authorization_list = authorizations
        .iter()
        .map(|&(authority, nonce)| {
            Either::Right(RecoveredAuthorization::new_unchecked(
                Authorization { chain_id: U256::ZERO, address: DELEGATE, nonce },
                RecoveredAuthority::Valid(authority),
            ))
        })
        .collect();
    OpTx(op_transaction(TxEnv {
        tx_type: TransactionType::Eip7702 as u8,
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit,
        gas_priority_fee: Some(0),
        authorization_list,
        ..Default::default()
    }))
}

/// `SELFDESTRUCT` to `beneficiary`.
fn destructs_to(beneficiary: Address) -> Bytes {
    BytecodeBuilder::default().push_address(beneficiary).append(SELFDESTRUCT).build()
}

/// `CALL(GAS, MegaAccessControl, value, isVolatileDataAccessDisabled())`, discarding the flag. The
/// interceptor answers it without a frame, and refuses it when it carries value.
fn asks_access_control(code: BytecodeBuilder, value: u64) -> BytecodeBuilder {
    code.mstore(0, IMegaAccessControl::isVolatileDataAccessDisabledCall::SELECTOR)
        .append_many([PUSH0, PUSH0])
        .push_number(4u64)
        .append(PUSH0)
        .push_number(value)
        .push_address(ACCESS_CONTROL_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
}

/// `funded()` with `code` at [`CALLEE`] and `MegaAccessControl` deployed.
fn beside_access_control(code: BytecodeBuilder) -> MemoryDatabase {
    callee_running(code)
        .account_balance(ACCESS_CONTROL_ADDRESS, U256::from(1))
        .account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE)
}

/// Init code that returns one byte, `0xEF`, which EIP-3541 refuses to deposit.
fn refused_code() -> Bytes {
    Bytes::from_static(&[PUSH1, 0xef, PUSH0, MSTORE8, PUSH1, 1, PUSH0, RETURN])
}

/// Init code that returns one byte more than a contract may hold.
fn oversized_code() -> Bytes {
    BytecodeBuilder::default()
        .push_number(MAX_CONTRACT_SIZE as u64 + 1)
        .append_many([PUSH0, RETURN])
        .build()
}

/// `DELEGATECALL(GAS, CHILD, 0, 0, 0, 0)`, discarding the flag: [`CHILD`]'s code runs on
/// [`CALLEE`]'s storage, in a frame of its own.
fn delegating(code: BytecodeBuilder) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(CHILD)
        .append(GAS)
        .append(DELEGATECALL)
        .append(POP)
}

/// Code for [`CALLEE`] that sets slot 1 and calls itself with a byte of calldata; called with
/// calldata, it writes the slot back.
fn writes_then_calls_itself_to_write_back() -> Bytes {
    let outer = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .append_many([PUSH0, PUSH0])
        .push_number(1u64)
        .append_many([PUSH0, PUSH0])
        .push_address(CALLEE)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build();
    let inner = BytecodeBuilder::default()
        .append(JUMPDEST)
        .sstore(U256::from(1), U256::ZERO)
        .stop()
        .build();
    // `CALLDATASIZE; PUSH1 inner; JUMPI` takes four bytes ahead of the outer path.
    let inner_at = u8::try_from(4 + outer.len()).expect("the outer path is short");
    let mut code = vec![CALLDATASIZE, PUSH1, inner_at, JUMPI];
    code.extend_from_slice(&outer);
    code.extend_from_slice(&inner);
    Bytes::from(code)
}

/// A transaction-level data-size limit of `bytes`, or none at all. The body counts towards it, so
/// a limit that stops a write the transaction makes is the body's bytes and more.
fn limited_to(bytes: u64) -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(bytes)
}

/// A write-back takes the record back only if the frame that made it is kept: a delegate that
/// writes the slot back and then reverts takes its write-back with it, and the record stands.
#[test]
fn test_a_write_back_dies_with_the_frame_that_made_it() {
    if runs_at_measurement_prices() {
        return;
    }
    for gas_limit in GAS_LIMITS {
        let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1));
        let db = callee_running(delegating(code)).account_code(
            CHILD,
            BytecodeBuilder::default().sstore(U256::from(1), U256::ZERO).revert().build(),
        );
        let outcome = execute(db, call(CALLER, CALLEE, U256::ZERO, gas_limit));
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        assert_eq!(outcome.gas.history_bytes, TX_BODY_SIZE + WRITE_RECORD_SIZE);
        assert_eq!(outcome.gas.history, outcome.gas.history_bytes * CPHB);
        assert_reservoir_paid("a write-back that reverts", gas_limit, &outcome);
    }
}

/* ---------- where an allowance pays ---------- */

/// One three-topic event over one word: the event the allowance is sized for.
fn event() -> BytecodeBuilder {
    log(BytecodeBuilder::default(), 3, 32)
}

/// A value call's history allowance pays for bytes its callee appends, and those bytes are on no
/// gas ledger: the byte count exceeds the history gas by exactly what the allowances paid, at
/// most one allowance per value call, and nothing for an event its frame did not keep.
#[test]
fn test_the_bytes_exceed_the_history_gas_by_what_the_allowances_paid() {
    if runs_at_measurement_prices() {
        return;
    }
    // The two records every transfer below writes: the sender's account and the receiver's.
    let transfer = 2 * WRITE_RECORD_SIZE;

    type Case = (&'static str, fn() -> MemoryDatabase, u64, u64);
    let cases: [Case; 5] = [
        (
            "a transfer's receiver emits the event, and the allowance pays all of it",
            || {
                callee_running(calling(BytecodeBuilder::default(), RECEIVER, 1, 2_300))
                    .account_code(RECEIVER, event().stop().build())
            },
            TX_BODY_SIZE + transfer + log_bytes(3, 32),
            HISTORY_ALLOWANCE_BYTES,
        ),
        (
            "an event larger than the allowance: the receiver's gas pays the rest",
            || {
                callee_running(calling(BytecodeBuilder::default(), RECEIVER, 1, 100_000))
                    .account_code(RECEIVER, log(BytecodeBuilder::default(), 3, 64).stop().build())
            },
            TX_BODY_SIZE + transfer + log_bytes(3, 64),
            HISTORY_ALLOWANCE_BYTES,
        ),
        (
            "two events: the allowance pays the first",
            || {
                let code = log(event(), 3, 32).stop().build();
                callee_running(calling(BytecodeBuilder::default(), RECEIVER, 1, 100_000))
                    .account_code(RECEIVER, code)
            },
            TX_BODY_SIZE + transfer + 2 * log_bytes(3, 32),
            HISTORY_ALLOWANCE_BYTES,
        ),
        (
            "the receiver reverts: the event is not appended, and the transfer's records neither",
            || {
                callee_running(calling(BytecodeBuilder::default(), RECEIVER, 1, 100_000))
                    .account_code(RECEIVER, event().revert().build())
            },
            TX_BODY_SIZE,
            0,
        ),
        (
            "two receivers, two allowances",
            || {
                let code = calling(
                    calling(BytecodeBuilder::default(), RECEIVER, 1, 2_300),
                    OTHER_RECEIVER,
                    1,
                    2_300,
                );
                callee_running(code)
                    .account_code(RECEIVER, event().stop().build())
                    .account_code(OTHER_RECEIVER, event().stop().build())
            },
            // The sender's account is written once; each receiver's once.
            TX_BODY_SIZE + 3 * WRITE_RECORD_SIZE + 2 * log_bytes(3, 32),
            2 * HISTORY_ALLOWANCE_BYTES,
        ),
    ];

    for (name, db, bytes, paid_by_allowances) in cases {
        for gas_limit in GAS_LIMITS {
            let outcome = execute(db(), call(CALLER, CALLEE, U256::ZERO, gas_limit));
            assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
            assert_eq!(outcome.gas.history_bytes, bytes, "{name} at {gas_limit}: the bytes");
            assert_eq!(
                outcome.gas.history_bytes * CPHB - outcome.gas.history,
                paid_by_allowances * CPHB,
                "{name} at {gas_limit}: the gap is what the allowances paid",
            );
            assert_reservoir_paid(name, gas_limit, &outcome);
        }
    }
}

/* ---------- the bytes and the data size ---------- */

/// The calldata of `sendHint(topic, payload)`.
fn hint_call() -> Bytes {
    let call =
        IOracle::sendHintCall { topic: B256::repeat_byte(0x7a), data: Bytes::from(vec![7; 40]) };
    Bytes::from(call.abi_encode())
}

/// `CALL(GAS, Oracle, 0, hint_call())`, discarding the flag: a hint sent from a frame.
fn hinting(code: BytecodeBuilder) -> BytecodeBuilder {
    let data = hint_call();
    code.mstore(0, &data)
        .append_many([PUSH0, PUSH0])
        .push_number(data.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append(GAS)
        .append(CALL)
        .append(POP)
}

/// `funded()` with the Oracle deployed.
fn beside_the_oracle() -> MemoryDatabase {
    funded().account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
}

/// How a case of [`test_the_history_bytes_are_the_data_size_every_record_kept`] runs.
#[derive(Clone, Copy)]
enum Run {
    /// With no limit.
    Plain,
    /// Under a transaction data-size limit of this many bytes, which the transaction's own frame
    /// crosses.
    Limited(u64),
    /// Under a transaction data-size limit of this many bytes, which a frame below the
    /// transaction's own crosses, whose caller catches the stop.
    Caught(u64),
}

/// The history bytes a transaction reports and the data size it kept are counted from one byte
/// table, record by record, so they move together at every site both count — the body, calldata,
/// the access list, authorizations, logs, write records and deployed code — and on every path
/// that takes a record back: a failed creation, output revm does not deposit, a frame answered
/// without running, a stop. They part at two sites: an Oracle hint, whose payload is data size and
/// never history, and the transfer log a kept value movement leaves, which is data size and never
/// history either — the history columns stay where they are, and the receipt carries the log. None
/// of these cases draws on an allowance, so the history gas is the bytes at the price as well.
#[test]
fn test_the_history_bytes_are_the_data_size_every_record_kept() {
    if runs_at_measurement_prices() {
        return;
    }
    let hint = hint_call().len() as u64;
    type Case =
        (&'static str, fn() -> MemoryDatabase, fn(u64) -> MegaTransaction, Run, u64, u64, u64);
    let cases: [Case; 34] = [
        (
            "the body",
            || callee_running(BytecodeBuilder::default()),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "calldata",
            || callee_running(BytecodeBuilder::default()),
            |gas| call_with_data(CALLER, CALLEE, Bytes::from(vec![0; 100]), gas),
            Run::Plain,
            TX_BODY_SIZE + 100,
            0,
            0,
        ),
        (
            "an access list",
            || callee_running(BytecodeBuilder::default()),
            |gas| with_access_list(&[(CALLEE, 2), (PAYEE, 1), (FRESH, 0)], gas),
            Run::Plain,
            TX_BODY_SIZE + 3 * ACCESS_LIST_ADDRESS_SIZE + 3 * ACCESS_LIST_SLOT_SIZE,
            0,
            0,
        ),
        (
            "authorizations, one applied",
            || callee_running(BytecodeBuilder::default()),
            |gas| with_authorizations(&[(AUTHORITY, 0), (OTHER_AUTHORITY, 7)], gas),
            Run::Plain,
            TX_BODY_SIZE + 2 * AUTHORIZATION_SIZE + WRITE_RECORD_SIZE,
            0,
            0,
        ),
        (
            "a log",
            || callee_running(log(BytecodeBuilder::default(), 2, 50)),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + log_bytes(2, 50),
            0,
            0,
        ),
        (
            "a log a reverted child emitted",
            || {
                callee_running(calling(BytecodeBuilder::default(), CHILD, 0, 1_000_000))
                    .account_code(CHILD, log(BytecodeBuilder::default(), 1, 32).revert().build())
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "a storage write",
            || callee_running(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1))),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
            0,
            0,
        ),
        (
            "a storage write written back",
            || {
                callee_running(
                    BytecodeBuilder::default()
                        .sstore(U256::from(1), U256::from(1))
                        .sstore(U256::from(1), U256::ZERO),
                )
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "a value transfer's two records",
            || callee_running(calling(BytecodeBuilder::default(), PAYEE, 1, 100_000)),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE,
            0,
            1,
        ),
        (
            "a SELFDESTRUCT's beneficiary, and the account it clears",
            || funded().account_code(CALLEE, destructs_to(PAYEE)),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE,
            0,
            1,
        ),
        (
            "a creation transaction's code",
            funded,
            |gas| create(CALLER, deploying(), gas),
            Run::Plain,
            TX_BODY_SIZE + deploying().len() as u64 + WRITE_RECORD_SIZE + DEPLOYED,
            0,
            0,
        ),
        (
            "a nested creation's code",
            || callee_running(creating(BytecodeBuilder::default(), &deploying())),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE + DEPLOYED,
            0,
            0,
        ),
        (
            "a failed creation: its creator's nonce, and no code",
            || callee_running(creating(BytecodeBuilder::default(), &[PUSH1, 32, PUSH0, REVERT])),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
            0,
            0,
        ),
        (
            "output EIP-3541 refuses to deposit",
            || callee_running(creating(BytecodeBuilder::default(), &refused_code())),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
            0,
            0,
        ),
        (
            "output past the code-size limit",
            || callee_running(creating(BytecodeBuilder::default(), &oversized_code())),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
            0,
            0,
        ),
        (
            "a creation transaction whose output EIP-3541 refuses",
            funded,
            |gas| create(CALLER, refused_code(), gas),
            Run::Plain,
            TX_BODY_SIZE + refused_code().len() as u64,
            0,
            0,
        ),
        (
            "a call an interceptor answers without a frame",
            || {
                let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1));
                beside_access_control(asks_access_control(code, 0))
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
            0,
            0,
        ),
        (
            "a value call an interceptor refuses without a frame",
            || beside_access_control(asks_access_control(BytecodeBuilder::default(), 1)),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "a first frame answered with the stop",
            funded,
            |gas| call(CALLER, FRESH, U256::from(1), gas),
            Run::Limited(TX_BODY_SIZE + WRITE_RECORD_SIZE - 1),
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "a stop at a child's write, which its caller catches: its log is kept",
            || {
                callee_running(calling(log(BytecodeBuilder::default(), 0, 0), CHILD, 0, 1_000_000))
                    .account_code(
                        CHILD,
                        BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).build(),
                    )
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Caught(TX_BODY_SIZE + LOG_BASE_SIZE + WRITE_RECORD_SIZE - 1),
            TX_BODY_SIZE + LOG_BASE_SIZE,
            0,
            0,
        ),
        (
            "a hint sent by the transaction: its calldata is history, its payload is not",
            beside_the_oracle,
            |gas| call_with_data(CALLER, ORACLE_CONTRACT_ADDRESS, hint_call(), gas),
            Run::Plain,
            TX_BODY_SIZE + hint_call().len() as u64,
            1,
            0,
        ),
        (
            "a hint sent from a frame",
            || {
                beside_the_oracle()
                    .account_code(CALLEE, hinting(BytecodeBuilder::default()).stop().build())
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            1,
            0,
        ),
        (
            "a transfer that creates its recipient",
            funded,
            |gas| call(CALLER, FRESH, U256::from(1), gas),
            Run::Plain,
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
            0,
            1,
        ),
        (
            "a transaction whose own frame writes, logs, deploys and reverts",
            || {
                let code = creating(
                    log(BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)), 1, 32),
                    &deploying(),
                );
                funded().account_code(CALLEE, code.revert().build())
            },
            |gas| call(CALLER, CALLEE, U256::from(1), gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "every site at once",
            every_site,
            every_site_called,
            Run::Plain,
            // The body and its calldata; the slot; the log; the transfer's two records (the
            // callee's account and the recipient's), which leave the creation none of its own
            // for the callee's nonce; the created account and its code.
            TX_BODY_SIZE +
                10 +
                WRITE_RECORD_SIZE +
                log_bytes(3, 64) +
                2 * WRITE_RECORD_SIZE +
                WRITE_RECORD_SIZE +
                DEPLOYED,
            0,
            1,
        ),
        (
            "an authority applied twice is one record",
            || callee_running(BytecodeBuilder::default()),
            |gas| with_authorizations(&[(AUTHORITY, 0), (AUTHORITY, 1)], gas),
            Run::Plain,
            TX_BODY_SIZE + 2 * AUTHORIZATION_SIZE + WRITE_RECORD_SIZE,
            0,
            0,
        ),
        (
            "the sender's own authorization: in the body, and no record beside it",
            || callee_running(BytecodeBuilder::default()),
            |gas| with_authorizations(&[(CALLER, 1)], gas),
            Run::Plain,
            TX_BODY_SIZE + AUTHORIZATION_SIZE,
            0,
            0,
        ),
        (
            "a SELFDESTRUCT's beneficiary the destruction creates, and the account it clears",
            || funded().account_code(CALLEE, destructs_to(FRESH)),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE,
            0,
            1,
        ),
        (
            "a SELFDESTRUCT to the sender records the account it clears alone: the sender's \
             account the transaction changed already",
            || funded().account_code(CALLEE, destructs_to(CALLER)),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
            0,
            1,
        ),
        (
            "a SELFDESTRUCT that moves nothing records nothing",
            || funded().account_code(RECEIVER, destructs_to(PAYEE)),
            |gas| call(CALLER, RECEIVER, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "a stop at a value call before its frame is built: nothing its start would write is \
             kept, and its caller, which catches the stop, keeps its own write",
            || {
                let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1));
                callee_running(calling(code, PAYEE, 1, 100_000))
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Caught(TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE - 1),
            TX_BODY_SIZE + WRITE_RECORD_SIZE,
            0,
            0,
        ),
        (
            "a slot written, then written back by a delegate: the record is taken back",
            || {
                let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1));
                callee_running(delegating(code)).account_code(
                    CHILD,
                    BytecodeBuilder::default().sstore(U256::from(1), U256::ZERO).build(),
                )
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "a slot a delegate wrote, written back by its caller",
            || {
                let code = delegating(BytecodeBuilder::default());
                callee_running(code.sstore(U256::from(1), U256::ZERO)).account_code(
                    CHILD,
                    BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).build(),
                )
            },
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
        (
            "a slot written, then written back by a call to itself",
            || funded().account_code(CALLEE, writes_then_calls_itself_to_write_back()),
            |gas| call(CALLER, CALLEE, U256::ZERO, gas),
            Run::Plain,
            TX_BODY_SIZE,
            0,
            0,
        ),
    ];

    for (name, db, tx, run, bytes, hints, moves) in cases {
        for gas_limit in GAS_LIMITS {
            let limit = match run {
                Run::Limited(limit) | Run::Caught(limit) => limit,
                Run::Plain => u64::MAX,
            };
            // A limit the body alone crosses would stop every case before its first write, and the
            // case would pin the body's stop rather than its own.
            assert!(limit > TX_BODY_SIZE, "{name}: the body fits under the limit");
            let mut evm = MegaEvm::new(context(db()).with_tx_runtime_limits(limited_to(limit)));
            let outcome = evm.execute_transaction(tx(gas_limit)).expect("the transaction is valid");
            assert_eq!(
                outcome.limit_exceeded.is_some(),
                matches!(run, Run::Limited(_)),
                "{name} at {gas_limit}: {:?}",
                outcome.result,
            );
            assert_eq!(
                outcome.gas.history_bytes, bytes,
                "{name} at {gas_limit}: the history bytes"
            );
            assert_eq!(
                outcome.result.logs().iter().filter(|log| is_transfer_log(log)).count() as u64,
                moves,
                "{name} at {gas_limit}: the transfer logs kept",
            );
            assert_eq!(
                outcome.usage.data_size,
                bytes + hints * hint + moves * TRANSFER_LOG_SIZE,
                "{name} at {gas_limit}: the data size is the history bytes, the hints' payloads \
                 and the transfer logs",
            );
            assert_eq!(
                outcome.gas.history,
                bytes * CPHB,
                "{name} at {gas_limit}: the history gas is the bytes at the price",
            );
            assert_reservoir_paid(name, gas_limit, &outcome);
        }
    }
}
