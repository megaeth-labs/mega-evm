//! Who is held to no per-transaction limit: a transaction the protocol itself sent, and a system
//! call.
//!
//! The data-size limit, the KV limit, the state-gas limit and the frame budgets of the first two
//! hold what users do. The protocol's own work — the maintenance a system transaction performs,
//! the calls the protocol makes before a block's transactions — must not fail on them, as it pays
//! no history gas for the same reason. What it uses is counted all the same, and reported.
//!
//! A user's deposit is not the protocol's work: it is held to every limit, and so is a user's
//! transaction that calls a system contract.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    system::{MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitKind, MegaEvm, MegaTransaction, MegaTransactionOutcome, TX_BODY_SIZE,
    WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, GAS, LOG0, POP, PUSH0},
    context::TxEnv,
};

use crate::common::context;

const CALLER: Address = address!("0000000000000000000000000000000000b00000");
/// A contract the program calls, which writes a slot of its own.
const HELPER: Address = address!("0000000000000000000000000000000000b00001");
/// A contract that runs the program, for the transactions that are not the system's.
const CONTRACT: Address = address!("0000000000000000000000000000000000b00002");

const GAS_LIMIT: u64 = 50_000_000;

/// Two fresh slots, a log of 64 bytes, and a call to [`HELPER`], which writes a third slot in a
/// frame of its own: state gas, write records and data size in the transaction's own frame and in
/// a child.
fn program() -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(1))
        .sstore(U256::from(2), U256::from(2))
        .push_number(64_u64)
        .push_number(0_u64)
        .append(LOG0)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(HELPER)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build()
}

/// The program at the whitelisted contract a system transaction may call, and at [`CONTRACT`].
///
/// A call with no calldata carries no selector, so the Oracle's interceptor lets it through to the
/// code at its address.
fn db() -> MemoryDatabase {
    let helper = BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build();
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(MEGA_SYSTEM_ADDRESS, U256::ZERO)
        .account_code(ORACLE_CONTRACT_ADDRESS, program())
        .account_code(CONTRACT, program())
        .account_code(HELPER, helper)
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

/// `tx` as a user's deposit: no fee, and a source hash a user's transaction could carry.
fn deposit(mut tx: MegaTransaction) -> MegaTransaction {
    tx.0.deposit.source_hash = B256::repeat_byte(0x11);
    tx.0.base.gas_price = 0;
    tx
}

/// Every per-transaction limit the program crosses, one at a time, and all of them together.
fn limits() -> [(&'static str, EvmTxRuntimeLimits); 6] {
    let none = EvmTxRuntimeLimits::no_limits();
    [
        ("the transaction's data size", none.with_tx_data_size_limit(TX_BODY_SIZE)),
        ("a frame's data size", none.with_frame_data_size_limit(WRITE_RECORD_SIZE)),
        ("the transaction's KV count", none.with_tx_kv_update_limit(1)),
        ("a frame's KV count", none.with_frame_kv_update_limit(1)),
        ("the transaction's state gas", none.with_tx_state_gas_limit(1)),
        (
            "every limit at zero",
            none.with_tx_data_size_limit(0)
                .with_frame_data_size_limit(0)
                .with_tx_kv_update_limit(0)
                .with_frame_kv_update_limit(0)
                .with_tx_state_gas_limit(0),
        ),
    ]
}

fn execute(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(tx)
        .expect("the transaction is valid")
}

/// A system transaction over any of the limits, or all of them, runs as it does without one, and
/// reports what it used: its body, three slots' records and state gas, and the log.
#[test]
fn test_a_system_transaction_is_held_to_no_limit() {
    let tx = || call_from(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
    let free = execute(db(), tx(), EvmTxRuntimeLimits::no_limits());
    assert!(free.result.is_success(), "{:?}", free.result);
    assert_eq!(free.usage.write_records, 3, "three slots");
    assert_eq!(free.usage.data_size, TX_BODY_SIZE + 3 * WRITE_RECORD_SIZE + 32 + 64);
    assert!(free.gas.state > 0);

    for (name, limits) in limits() {
        let outcome = execute(db(), tx(), limits);
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{name}");
        assert_eq!(outcome.usage, free.usage, "{name}: its usage is counted all the same");
        assert_eq!(outcome.gas, free.gas, "{name}");
        assert_eq!(outcome.state, free.state, "{name}");
    }
}

/// A system call over any of the limits runs as it does without one, whatever caller it names,
/// and the layer reports what it used.
#[test]
fn test_a_system_call_is_held_to_no_limit() {
    let run = |limits| {
        let mut evm = MegaEvm::new(context(db()).with_tx_runtime_limits(limits));
        let result = Evm::transact_system_call(&mut evm, CALLER, CONTRACT, Bytes::new())
            .expect("the system call runs");
        (
            result,
            evm.ctx().additional_limit().usage(),
            evm.ctx().additional_limit().latched().copied(),
        )
    };
    let (free, free_usage, _) = run(EvmTxRuntimeLimits::no_limits());
    assert!(free.result.is_success(), "{:?}", free.result);
    assert_eq!(free_usage.write_records, 3);

    for (name, limits) in limits() {
        let (result, usage, latched) = run(limits);
        assert!(result.result.is_success(), "{name}: {:?}", result.result);
        assert_eq!(latched, None, "{name}");
        assert_eq!(usage, free_usage, "{name}: its usage is counted all the same");
        assert_eq!(result.result, free.result, "{name}");
    }
}

/// A user's deposit is held to the limits: over the KV count or the state gas, it is stopped.
/// So is a user's transaction to the contract a system transaction may call.
#[test]
fn test_a_deposit_and_a_user_call_to_a_system_contract_are_held_to_the_limits() {
    let none = EvmTxRuntimeLimits::no_limits();
    let cases = [
        (
            "a deposit",
            deposit(call_from(CALLER, CONTRACT)),
            none.with_tx_kv_update_limit(0),
            LimitKind::KVUpdate,
        ),
        (
            "a deposit",
            deposit(call_from(CALLER, CONTRACT)),
            none.with_tx_state_gas_limit(0),
            LimitKind::StateGrowth,
        ),
        (
            "a user's call to a system contract",
            call_from(CALLER, ORACLE_CONTRACT_ADDRESS),
            none.with_tx_data_size_limit(TX_BODY_SIZE),
            LimitKind::DataSize,
        ),
        (
            "a user's call to a system contract",
            call_from(CALLER, ORACLE_CONTRACT_ADDRESS),
            none.with_tx_kv_update_limit(0),
            LimitKind::KVUpdate,
        ),
        (
            "a user's call to a system contract",
            call_from(CALLER, ORACLE_CONTRACT_ADDRESS),
            none.with_tx_state_gas_limit(0),
            LimitKind::StateGrowth,
        ),
    ];
    for (name, tx, limits, kind) in cases {
        let outcome = execute(db(), tx, limits);
        let stop = outcome.limit_exceeded.unwrap_or_else(|| panic!("{name}, {kind:?}: stopped"));
        assert!(
            matches!(stop, mega_evm::LimitCheck::ExceedsLimit { kind: k, .. } if k == kind),
            "{name}: {stop:?}"
        );
        assert!(
            !outcome.result.is_success() && !outcome.result.is_halt(),
            "{name}: {:?}",
            outcome.result
        );
    }
}
