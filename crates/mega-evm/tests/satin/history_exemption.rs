//! Who pays no history gas at all: a deposit, a transaction the protocol itself sent, and a
//! system call.
//!
//! What those three append to the chain is the chain carrying its own weight — a deposit the
//! sequencer relays and that was paid for on L1, the maintenance a system transaction performs,
//! the calls the protocol makes before a block's transactions — and there is no sender to charge
//! for it. Every one of them runs the same program as the control transaction below, which pays
//! for every byte of it.
//!
//! The exemption is history's alone. State gas is charged as usual, which each test asserts, so an
//! exempt transaction cannot grow the state for free.

use std::collections::BTreeMap;

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    constants::{
        ACCOUNT_STATE_GAS, COST_PER_HISTORY_BYTE, COST_PER_STATE_BYTE, SLOT_STATE_GAS,
        TX_GAS_LIMIT_CAP,
    },
    system::{IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE},
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase, OutcomeView},
    transaction_body_bytes, LimitUsage, MegaEvm, MegaGasUsage, MegaHaltReason, MegaTransaction,
    MegaTransactionOutcome, LOG_BASE_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, LOG0, POP},
    context::{result::ResultAndState, TxEnv},
    inspector::NoOpInspector,
};

use crate::common::{context, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000400000");
const CONTRACT: Address = address!("0000000000000000000000000000000000400001");
const PAYEE: Address = address!("0000000000000000000000000000000000400002");

/// Room for the state gas of a new slot, a new account and a deployment.
const GAS_LIMIT: u64 = 50_000_000;

/// The program every transaction here runs: a new slot, a log, a transfer that creates its
/// recipient, and a deployment of [`DEPLOYED_BYTES`] bytes — one of each history site the engine
/// charges, including the one revm charges itself out of the schedule.
fn program() -> Bytes {
    program_deploying(DEPLOYED_BYTES)
}

/// The same program with a creation that deploys `len` bytes.
fn program_deploying(len: u64) -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .push_number(0u64)
        .push_number(32u64)
        .push_number(0u64)
        .append(LOG0)
        // CALL(gas, PAYEE, 1, 0, 0, 0, 0)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(1u64)
        .push_address(PAYEE)
        .push_number(1_000_000u64)
        .append(CALL)
        .append(POP)
        // `PUSH1 len; PUSH0; RETURN` in memory, then a CREATE over those four bytes.
        .mstore(0, [0x60, len as u8, 0x5f, 0xf3])
        .push_number(4u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .append(POP)
        .stop()
        .build()
}

/// Bytes the program's creation deploys, which the schedule prices at a history byte each.
const DEPLOYED_BYTES: u64 = 32;

/// The history bytes a user transaction running the program from [`CALLER`] appends: its body
/// with no calldata; the program's four write records — the new slot, then `CONTRACT` (the
/// transfer's sender and the creator, recorded once in its frame), `PAYEE` and the created
/// account; its log of one word with no topic; and the deployed code. The transfer's log is data
/// size and appends no history.
const PROGRAM_HISTORY_BYTES: u64 =
    TX_BODY_SIZE + 4 * WRITE_RECORD_SIZE + (LOG_BASE_SIZE + 32) + DEPLOYED_BYTES;

/// The state gas the program adds, exempt or not: the new slot, `PAYEE`'s account, the created
/// account and the code it deploys, in minimum-size buckets.
const PROGRAM_STATE_GAS: u64 =
    SLOT_STATE_GAS + 2 * ACCOUNT_STATE_GAS + DEPLOYED_BYTES * COST_PER_STATE_BYTE;

fn db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::ZERO)
        .account_balance(CONTRACT, U256::from(10u64.pow(9)))
        .account_code(CONTRACT, program())
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
        .sequencer_registry(MEGA_SYSTEM_ADDRESS)
}

/// A call to `to` from `caller`.
fn call_from(caller: Address, to: Address) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller,
        kind: TxKind::Call(to),
        gas_limit: GAS_LIMIT,
        ..Default::default()
    }))
}

/// The slot of the Oracle the system transaction sets, and the value it sets it to.
const ORACLE_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);
const ORACLE_VALUE: B256 = B256::repeat_byte(0x5a);

/// The protocol's own transaction: the system address setting [`ORACLE_SLOT`] of the Oracle, a
/// contract it may call, through `setSlot`, which only the system address may call.
fn system_tx() -> MegaTransaction {
    let mut tx = call_from(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
    tx.0.base.data =
        IOracle::setSlotCall { slot: ORACLE_SLOT, value: ORACLE_VALUE }.abi_encode().into();
    tx
}

/// The value `outcome` left in the Oracle's [`ORACLE_SLOT`], if it changed the slot.
fn written_oracle_slot(outcome: &MegaTransactionOutcome) -> Option<B256> {
    let slot = outcome.state.get(&ORACLE_CONTRACT_ADDRESS)?.storage.get(&ORACLE_SLOT)?;
    slot.is_changed().then(|| B256::from(slot.present_value))
}

/// Runs `tx` and reports its history and state ledgers, and the outcome they are read from.
fn ledgers(tx: MegaTransaction) -> (u64, u64, MegaTransactionOutcome) {
    let outcome =
        MegaEvm::new(context(db())).execute_transaction(tx).expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    (outcome.gas.history, outcome.gas.state, outcome)
}

/// The outcome of the system call `evm` just ran, whose result and state are `result`, gathered
/// from the same parts `execute_transaction` gathers a transaction's outcome from.
fn system_call_outcome(
    evm: &MegaEvm<MemoryDatabase, NoOpInspector>,
    result: ResultAndState<MegaHaltReason>,
) -> MegaTransactionOutcome {
    let layer = evm.ctx().additional_limit();
    MegaTransactionOutcome {
        gas: MegaGasUsage::new(
            result.result.gas(),
            layer.history_gas_spent(),
            layer.history_bytes(),
        ),
        usage: layer.usage(),
        limit_exceeded: layer.latched().copied(),
        oracle_reads: evm.oracle_reads().to_vec(),
        result_and_state: result,
    }
}

/// The control: a user transaction running the program pays for every byte of it, on top of its
/// own body.
#[test]
fn test_a_user_transaction_pays_history_for_the_program() {
    if runs_at_measurement_prices() {
        return;
    }
    let (history, state, outcome) = ledgers(call_from(CALLER, CONTRACT));
    assert!(history > 0, "a user transaction pays for the bytes it appends");
    assert!(state > 0, "and for the state it adds");
    assert_eq!(history, PROGRAM_HISTORY_BYTES * COST_PER_HISTORY_BYTE, "every byte of the program");
    assert_eq!(outcome.gas.history_bytes, PROGRAM_HISTORY_BYTES);
    assert_eq!(state, PROGRAM_STATE_GAS, "the slot, the two accounts and the code");
    crate::assert_sorted_json_snapshot!(&OutcomeView::new(&outcome));
}

/// The one history charge revm makes itself is the deployed code's, out of the schedule; the
/// exemption reaches it too, by handing an exempt transaction a schedule that prices a deposited
/// byte at zero.
///
/// The program above deploys thirty-two bytes. A user transaction pays for them and a deposit
/// running the same program pays for nothing at all, so the difference the schedule makes is
/// visible where a ledger comparison alone would only show two zeroes.
#[test]
fn test_the_exempt_schedule_prices_a_deposited_byte_at_zero() {
    if runs_at_measurement_prices() {
        return;
    }
    let mut outcomes = BTreeMap::new();
    let mut deployed = |name: &'static str, code: Bytes| {
        let db = db().account_code(CONTRACT, code);
        let outcome = MegaEvm::new(context(db))
            .execute_transaction(call_from(CALLER, CONTRACT))
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        let history = outcome.gas.history;
        outcomes.insert(name, OutcomeView::new(&outcome));
        history
    };
    let thirty_two = deployed("thirty_two", program());
    let nothing = deployed("nothing", program_deploying(0));

    assert_eq!(
        thirty_two - nothing,
        DEPLOYED_BYTES * mega_evm::constants::COST_PER_HISTORY_BYTE,
        "a user transaction pays a history byte per deployed byte",
    );
    assert_eq!(ledgers(deposit(call_from(CALLER, CONTRACT))).0, 0, "a deposit pays none of it");
    outcomes.insert("deposit", OutcomeView::new(&ledgers(deposit(call_from(CALLER, CONTRACT))).2));
    crate::assert_sorted_json_snapshot!(&outcomes);
}

/// `tx` as a deposit: no fee, and a source hash a user's transaction could carry.
fn deposit(mut tx: MegaTransaction) -> MegaTransaction {
    tx.0.deposit.source_hash = B256::repeat_byte(0x11);
    tx.0.base.gas_price = 0;
    tx
}

/// A deposit pays no history gas: the bytes it carries were paid for on L1.
#[test]
fn test_a_deposit_pays_no_history_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    let (history, state, outcome) = ledgers(deposit(call_from(CALLER, CONTRACT)));
    assert_eq!(history, 0, "a deposit pays no history gas");
    assert!(state > 0, "the exemption is history's alone");
    assert_eq!(state, PROGRAM_STATE_GAS, "the state a user transaction pays");
    assert_eq!(outcome.gas.history_bytes, 0);
    crate::assert_sorted_json_snapshot!(&OutcomeView::new(&outcome));
}

/// A transaction from the system address pays no history gas: the protocol is maintaining its own
/// state, and a resource charge it cannot pay would stop it.
#[test]
fn test_a_system_transaction_pays_no_history_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    // The system address may only call a whitelisted contract, so the program runs there: the
    // Oracle's own code is what a system transaction reaches, and `setSlot` writes a slot of its
    // own.
    let tx = system_tx();
    let outcome =
        MegaEvm::new(context(db())).execute_transaction(tx).expect("the transaction is valid");
    assert_eq!(outcome.gas.history, 0, "the protocol's own transaction pays no history gas");
    assert_eq!(outcome.gas.history_bytes, 0, "and appends no history byte, its body included");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(written_oracle_slot(&outcome), Some(ORACLE_VALUE), "it wrote the Oracle's slot");
    assert_eq!(
        outcome.usage,
        LimitUsage {
            data_size: transaction_body_bytes(&system_tx()) + WRITE_RECORD_SIZE,
            write_records: 1,
        },
        "the body and the slot's record are counted, though neither pays history",
    );
    // The state is charged as usual: the new slot, and the system address's own account, which
    // the database leaves empty, so the transaction creates it, as it does a deposit's caller.
    assert_eq!(outcome.gas.state, SLOT_STATE_GAS + ACCOUNT_STATE_GAS);
    crate::assert_sorted_json_snapshot!(&OutcomeView::new(&outcome));
}

/// A system call pays no history gas either: it is the protocol running, not a transaction
/// anybody sent, and it has no sender to charge.
#[test]
fn test_a_system_call_pays_no_history_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    let mut evm = MegaEvm::new(context(db()));
    let result = Evm::transact_system_call(&mut evm, CALLER, CONTRACT, Bytes::new())
        .expect("the system call runs");
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(
        evm.ctx().additional_limit().history_gas_spent(),
        0,
        "a system call pays no history gas",
    );
    assert!(result.result.gas().state_gas_spent_final() > 0, "the exemption is history's alone");
    assert_eq!(result.result.gas().state_gas_spent_final(), PROGRAM_STATE_GAS);
    assert_eq!(evm.ctx().additional_limit().history_bytes(), 0);
    crate::assert_sorted_json_snapshot!(&OutcomeView::new(&system_call_outcome(&evm, result)));
}

/// The bytes a transaction appended are reported beside its history gas, and they are the bytes
/// that gas priced: the control reports every byte of the program, each at the price, and an
/// exempt transaction, which prices none of them, reports none.
///
/// Each transaction runs below the execution cap and above it. Above it the reservoir pays the
/// state gas, which the exemption leaves in place, and no history.
///
/// A system call runs at the gas limit its entry point fixes, below the cap.
#[test]
fn test_an_exempt_transaction_reports_no_history_bytes() {
    if runs_at_measurement_prices() {
        return;
    }
    const RESERVOIR: u64 = 100_000_000;
    let mut outcomes = BTreeMap::new();
    for gas_limit in [GAS_LIMIT, TX_GAS_LIMIT_CAP + RESERVOIR] {
        let mut run = |name: &str, mut tx: MegaTransaction| {
            tx.0.base.gas_limit = gas_limit;
            let outcome = MegaEvm::new(context(db()))
                .execute_transaction(tx)
                .expect("the transaction is valid");
            outcomes.insert(format!("{name} at {gas_limit}"), OutcomeView::new(&outcome));
            let gas = outcome.gas;
            if gas_limit > TX_GAS_LIMIT_CAP {
                assert_eq!(
                    gas.reservoir_remaining,
                    RESERVOIR - gas.state - gas.history,
                    "the reservoir paid the state and history ledgers",
                );
            }
            outcome
        };

        let paying = run("a user transaction", call_from(CALLER, CONTRACT)).gas;
        assert!(paying.history_bytes > 0);
        assert_eq!(
            paying.history,
            paying.history_bytes * mega_evm::constants::COST_PER_HISTORY_BYTE,
            "at {gas_limit}: the control pays for every byte it reports",
        );

        let deposit = run("a deposit", deposit(call_from(CALLER, CONTRACT))).gas;
        assert_eq!((deposit.history, deposit.history_bytes), (0, 0), "at {gas_limit}: a deposit");
        assert!(deposit.state > 0, "at {gas_limit}: the exemption is history's alone");
        let system = run("a system transaction", system_tx());
        assert!(system.result.is_success(), "at {gas_limit}: {:?}", system.result);
        assert_eq!(written_oracle_slot(&system), Some(ORACLE_VALUE), "at {gas_limit}");
        let system = system.gas;
        assert_eq!(
            (system.history, system.history_bytes),
            (0, 0),
            "at {gas_limit}: a system transaction",
        );
    }

    let mut evm = MegaEvm::new(context(db()));
    let result = Evm::transact_system_call(&mut evm, CALLER, CONTRACT, Bytes::new())
        .expect("the system call runs");
    assert_eq!(evm.ctx().additional_limit().history_bytes(), 0, "a system call");
    outcomes.insert("a system call".into(), OutcomeView::new(&system_call_outcome(&evm, result)));
    crate::assert_sorted_json_snapshot!(&outcomes);
}

/// The exemption belongs to one transaction: the next transaction on the same EVM pays again.
#[test]
fn test_the_exemption_does_not_outlive_its_transaction() {
    if runs_at_measurement_prices() {
        return;
    }
    let mut evm = MegaEvm::new(context(db()));

    let exempt = evm
        .execute_transaction(deposit(call_from(CALLER, CONTRACT)))
        .expect("the deposit is valid");
    assert_eq!(exempt.gas.history, 0);

    // Nothing was committed, so the sender's nonce is where it was.
    let paying =
        evm.execute_transaction(call_from(CALLER, CONTRACT)).expect("the transaction is valid");
    assert!(paying.result.is_success(), "{:?}", paying.result);
    assert!(paying.gas.history > 0, "the next transaction pays for its own bytes");
    assert_eq!(paying.gas.history, PROGRAM_HISTORY_BYTES * COST_PER_HISTORY_BYTE, "all of them");
    crate::assert_sorted_json_snapshot!(&BTreeMap::from([
        ("exempt", OutcomeView::new(&exempt)),
        ("paying", OutcomeView::new(&paying)),
    ]));
}
