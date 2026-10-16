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

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    constants::TX_GAS_LIMIT_CAP,
    system::{MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE},
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase},
    MegaEvm, MegaTransaction,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, LOG0, POP},
    context::TxEnv,
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

/// Runs `tx` and reports its history and state ledgers.
fn ledgers(tx: MegaTransaction) -> (u64, u64) {
    let outcome =
        MegaEvm::new(context(db())).execute_transaction(tx).expect("the transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    (outcome.gas.history, outcome.gas.state)
}

/// The control: a user transaction running the program pays for every byte of it, on top of its
/// own body.
#[test]
fn test_a_user_transaction_pays_history_for_the_program() {
    if runs_at_measurement_prices() {
        return;
    }
    let (history, state) = ledgers(call_from(CALLER, CONTRACT));
    assert!(history > 0, "a user transaction pays for the bytes it appends");
    assert!(state > 0, "and for the state it adds");
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
    let deployed = |code: Bytes| {
        let db = db().account_code(CONTRACT, code);
        let outcome = MegaEvm::new(context(db))
            .execute_transaction(call_from(CALLER, CONTRACT))
            .expect("the transaction is valid");
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        outcome.gas.history
    };
    let thirty_two = deployed(program());
    let nothing = deployed(program_deploying(0));

    assert_eq!(
        thirty_two - nothing,
        DEPLOYED_BYTES * mega_evm::constants::COST_PER_HISTORY_BYTE,
        "a user transaction pays a history byte per deployed byte",
    );
    assert_eq!(ledgers(deposit(call_from(CALLER, CONTRACT))).0, 0, "a deposit pays none of it");
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
    let (history, state) = ledgers(deposit(call_from(CALLER, CONTRACT)));
    assert_eq!(history, 0, "a deposit pays no history gas");
    assert!(state > 0, "the exemption is history's alone");
}

/// A transaction from the system address pays no history gas: the protocol is maintaining its own
/// state, and a resource charge it cannot pay would stop it.
#[test]
fn test_a_system_transaction_pays_no_history_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    // The system address may only call a whitelisted contract, so the program runs there: the
    // Oracle's own code is what a system transaction reaches, and it writes a slot of its own.
    let tx = call_from(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
    let outcome =
        MegaEvm::new(context(db())).execute_transaction(tx).expect("the transaction is valid");
    assert_eq!(outcome.gas.history, 0, "the protocol's own transaction pays no history gas");
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
    for gas_limit in [GAS_LIMIT, TX_GAS_LIMIT_CAP + RESERVOIR] {
        let gas = |mut tx: MegaTransaction| {
            tx.0.base.gas_limit = gas_limit;
            let gas = MegaEvm::new(context(db()))
                .execute_transaction(tx)
                .expect("the transaction is valid")
                .gas;
            if gas_limit > TX_GAS_LIMIT_CAP {
                assert_eq!(
                    gas.reservoir_remaining,
                    RESERVOIR - gas.state - gas.history,
                    "the reservoir paid the state and history ledgers",
                );
            }
            gas
        };

        let paying = gas(call_from(CALLER, CONTRACT));
        assert!(paying.history_bytes > 0);
        assert_eq!(
            paying.history,
            paying.history_bytes * mega_evm::constants::COST_PER_HISTORY_BYTE,
            "at {gas_limit}: the control pays for every byte it reports",
        );

        let deposit = gas(deposit(call_from(CALLER, CONTRACT)));
        assert_eq!((deposit.history, deposit.history_bytes), (0, 0), "at {gas_limit}: a deposit");
        assert!(deposit.state > 0, "at {gas_limit}: the exemption is history's alone");
        let system = gas(call_from(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS));
        assert_eq!(
            (system.history, system.history_bytes),
            (0, 0),
            "at {gas_limit}: a system transaction",
        );
    }

    let mut evm = MegaEvm::new(context(db()));
    Evm::transact_system_call(&mut evm, CALLER, CONTRACT, Bytes::new())
        .expect("the system call runs");
    assert_eq!(evm.ctx().additional_limit().history_bytes(), 0, "a system call");
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
}
