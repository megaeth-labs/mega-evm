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
    system::{MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE},
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase},
    MegaEvm, MegaTransaction,
};
use revm::context::TxEnv;

use crate::common::{context, runs_at_measurement_prices};

const CALLER: Address = address!("0000000000000000000000000000000000400000");
const CONTRACT: Address = address!("0000000000000000000000000000000000400001");
const PAYEE: Address = address!("0000000000000000000000000000000000400002");

/// Room for the state gas of a new slot, a new account and a deployment.
const GAS_LIMIT: u64 = 50_000_000;

/// The program every transaction here runs: a new slot, a log, a transfer that creates its
/// recipient, and a deployment — one of each history site the engine charges.
fn program() -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .push_number(0u64)
        .push_number(32u64)
        .push_number(0u64)
        .append(revm::bytecode::opcode::LOG0)
        // CALL(gas, PAYEE, 1, 0, 0, 0, 0)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(1u64)
        .push_address(PAYEE)
        .push_number(1_000_000u64)
        .append(revm::bytecode::opcode::CALL)
        .append(revm::bytecode::opcode::POP)
        // CREATE(0, 0, 0): an empty deployment, which still creates an account
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(revm::bytecode::opcode::CREATE)
        .append(revm::bytecode::opcode::POP)
        .stop()
        .build()
}

fn db() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::ZERO)
        .account_balance(CONTRACT, U256::from(10u64.pow(9)))
        .account_code(CONTRACT, program())
        .account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE)
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

/// A deposit pays no history gas: the bytes it carries were paid for on L1.
#[test]
fn test_a_deposit_pays_no_history_gas() {
    if runs_at_measurement_prices() {
        return;
    }
    let mut tx = call_from(CALLER, CONTRACT);
    tx.0.deposit.source_hash = B256::repeat_byte(0x11);
    tx.0.base.gas_price = 0;
    let (history, state) = ledgers(tx);
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

/// The exemption belongs to one transaction: the next transaction on the same EVM pays again.
#[test]
fn test_the_exemption_does_not_outlive_its_transaction() {
    if runs_at_measurement_prices() {
        return;
    }
    let mut evm = MegaEvm::new(context(db()));

    let mut deposit = call_from(CALLER, CONTRACT);
    deposit.0.deposit.source_hash = B256::repeat_byte(0x11);
    deposit.0.base.gas_price = 0;
    let exempt = evm.execute_transaction(deposit).expect("the deposit is valid");
    assert_eq!(exempt.gas.history, 0);

    // Nothing was committed, so the sender's nonce is where it was.
    let paying =
        evm.execute_transaction(call_from(CALLER, CONTRACT)).expect("the transaction is valid");
    assert!(paying.result.is_success(), "{:?}", paying.result);
    assert!(paying.gas.history > 0, "the next transaction pays for its own bytes");
}
