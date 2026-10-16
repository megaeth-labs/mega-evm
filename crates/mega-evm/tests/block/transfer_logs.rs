//! The EIP-7708 transfer logs in a block's receipts.
//!
//! Every value movement is logged where it happens, so a receipt carries its transaction's
//! transfer logs in execution order among the logs contracts emit, and its bloom includes them.
//! The block's data size counts them; its history columns do not. A deposit's value is logged and
//! its mint is not; a system transaction, as the sequencer builds it, moves no value and logs no
//! transfer.

use alloy_consensus::{transaction::Recovered, Sealed, Signed, TxLegacy, TxReceipt};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{
    address, logs_bloom, Address, BloomInput, Bytes, Log, Signature, TxKind, B256, U256,
};
use alloy_sol_types::SolCall;
use mega_evm::{
    revm::primitives::eip7708::{ETH_TRANSFER_LOG_ADDRESS, ETH_TRANSFER_LOG_TOPIC},
    system::{IOracle, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::{is_transfer_log, transfer_log, BytecodeBuilder},
    MegaTxEnvelope, LOG_BASE_SIZE, LOG_TOPIC_SIZE, TRANSFER_LOG_SIZE, TX_BODY_SIZE,
    WRITE_RECORD_SIZE,
};
use op_alloy_consensus::{OpReceiptEnvelope, TxDeposit};
use revm::{bytecode::opcode::LOG1, database::State};

use crate::common::{self, executor, CALLER, CHAIN_ID};

/// An account with no code.
const PAYEE: Address = address!("0x7000000000000000000000000000000000000001");
/// A contract that emits an event, then passes three wei on to [`PAYEE`].
const FORWARDER: Address = address!("0x7000000000000000000000000000000000000002");
/// A contract holding [`DESTRUCTOR_BALANCE`] that destructs to [`PAYEE`].
const DESTRUCTOR: Address = address!("0x7000000000000000000000000000000000000003");
/// The account a deposit comes from.
const DEPOSITOR: Address = address!("0x7000000000000000000000000000000000000004");

const DESTRUCTOR_BALANCE: u64 = 13;

/// The event [`FORWARDER`] emits between the value it receives and the value it passes on.
const MARKER: B256 = B256::repeat_byte(0x4d);

fn state() -> State<mega_evm::test_utils::MemoryDatabase> {
    let forwarder = BytecodeBuilder::default()
        .push_bytes(MARKER)
        .append_many([0x5f, 0x5f]) // PUSH0 PUSH0: no data at offset 0
        .append(LOG1)
        .call(PAYEE, U256::from(3))
        .stop()
        .build();
    let mut db = common::database();
    db.set_account_code(FORWARDER, forwarder);
    db.set_account_code(DESTRUCTOR, BytecodeBuilder::default().selfdestruct(PAYEE).build());
    db.set_account_balance(DESTRUCTOR, U256::from(DESTRUCTOR_BALANCE));
    State::builder().with_database(db).build()
}

/// A gas limit for any transaction here: 1,000,000 of regular gas on top of the state of two new
/// accounts and a fresh slot, a kilobyte of history beyond the body, and 64 times the history of
/// two write records, at the byte prices in effect: [`FORWARDER`] forwards all but a 64th of its
/// gas with the value it passes on, and pays the records of the frame it starts from the 64th it
/// keeps.
fn gas_limit() -> u64 {
    1_000_000 +
        2 * common::new_account_state_gas() +
        common::slot_state_gas() +
        common::body_history(1_000) +
        64 * mega_evm::write_record_history_gas(2).expect("two records have a price")
}

/// A legacy transaction from `signer` with `value`.
fn legacy(
    signer: Address,
    nonce: u64,
    to: TxKind,
    value: u64,
    input: Bytes,
) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 1_000_000,
        gas_limit: gas_limit(),
        to,
        value: U256::from(value),
        input,
    };
    let signed = Signed::new_unchecked(
        tx,
        Signature::test_signature(),
        B256::repeat_byte(nonce as u8 + 0x10),
    );
    Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), signer)
}

/// A deposit from [`DEPOSITOR`] to [`PAYEE`] minting `mint` and moving `value`.
fn deposit(mint: u128, value: u64) -> Recovered<MegaTxEnvelope> {
    let deposit = TxDeposit {
        source_hash: B256::repeat_byte(0xd0),
        from: DEPOSITOR,
        to: TxKind::Call(PAYEE),
        mint,
        value: U256::from(value),
        // Room for the state gas of the two accounts it creates: its depositor's and its payee's.
        gas_limit: gas_limit(),
        is_system_transaction: false,
        input: Bytes::new(),
    };
    let sealed = Sealed::new_unchecked(deposit, B256::repeat_byte(mint as u8));
    Recovered::new_unchecked(MegaTxEnvelope::Deposit(sealed), DEPOSITOR)
}

/// The logs a receipt carries, having checked its bloom is theirs, with the transfer logs in it.
fn logs_of(receipt: &OpReceiptEnvelope) -> &[Log] {
    let logs = receipt.logs();
    assert_eq!(receipt.bloom(), logs_bloom(logs), "the bloom is the receipt's logs'");
    if logs.iter().any(is_transfer_log) {
        let bloom = receipt.bloom();
        assert!(bloom.contains_input(BloomInput::Raw(ETH_TRANSFER_LOG_ADDRESS.as_slice())));
        assert!(bloom.contains_input(BloomInput::Raw(ETH_TRANSFER_LOG_TOPIC.as_slice())));
    }
    logs
}

/// A block with a value transfer, a value call passed on by a contract that emits an event
/// between the two moves, a creation with an endowment and a destruction. Each receipt carries its
/// transfer logs in execution order, its bloom includes them, and the block's data size counts
/// each at 160 bytes while its history bytes leave them out.
#[test]
fn test_a_blocks_receipts_carry_its_transfer_logs_in_execution_order() {
    let mut state = state();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let txs = [
        legacy(CALLER, 0, TxKind::Call(PAYEE), 5, Bytes::new()),
        legacy(CALLER, 1, TxKind::Call(FORWARDER), 7, Bytes::new()),
        legacy(CALLER, 2, TxKind::Create, 11, Bytes::new()),
        legacy(CALLER, 3, TxKind::Call(DESTRUCTOR), 0, Bytes::new()),
    ];
    let expected: [Vec<Log>; 4] = [
        vec![transfer_log(CALLER, PAYEE, U256::from(5))],
        vec![
            transfer_log(CALLER, FORWARDER, U256::from(7)),
            Log::new_unchecked(FORWARDER, vec![MARKER], Bytes::new()),
            transfer_log(FORWARDER, PAYEE, U256::from(3)),
        ],
        vec![transfer_log(CALLER, CALLER.create(2), U256::from(11))],
        vec![transfer_log(DESTRUCTOR, PAYEE, U256::from(DESTRUCTOR_BALANCE))],
    ];
    // The records each keeps: the recipient; the forwarder and the payee; the created account;
    // the destruction's beneficiary.
    let records = [1, 2, 1, 1];

    let mut data_size = 0;
    let mut history_bytes = 0;
    for (i, tx) in txs.iter().enumerate() {
        let outcome = executor.run_transaction(tx).expect("it executes");
        assert!(outcome.result.is_success(), "transaction {i}: {:?}", outcome.result);
        // No transaction here carries calldata, an access list or an authorization.
        let (transfers, events): (Vec<&Log>, Vec<&Log>) =
            expected[i].iter().partition(|log| is_transfer_log(log));
        let event_bytes: u64 = events
            .iter()
            .map(|log| {
                LOG_BASE_SIZE +
                    LOG_TOPIC_SIZE * log.topics().len() as u64 +
                    log.data.data.len() as u64
            })
            .sum();
        let kept = TX_BODY_SIZE + records[i] * WRITE_RECORD_SIZE + event_bytes;
        assert_eq!(
            outcome.usage.data_size,
            kept + transfers.len() as u64 * TRANSFER_LOG_SIZE,
            "transaction {i}: the transfer logs are data size",
        );
        assert_eq!(outcome.gas.history_bytes, kept, "transaction {i}: and not history");
        data_size += outcome.usage.data_size;
        history_bytes += outcome.gas.history_bytes;
        executor.commit_transaction_outcome(outcome).expect("the block has room");
    }

    for (i, receipt) in executor.receipts().iter().enumerate() {
        assert_eq!(logs_of(receipt), expected[i].as_slice(), "receipt {i}");
    }
    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    assert_eq!(result.usage.data_size, data_size, "the block counts every transfer log");
    assert_eq!(result.gas.history_bytes, history_bytes, "and its history bytes none of them");
}

/// A deposit's mint is credited before it runs and is logged nowhere; its value moves in its
/// first frame and is logged there, into the deposit's receipt. A deposit that only mints logs
/// nothing.
#[test]
fn test_a_deposit_receipt_logs_its_value_and_not_its_mint() {
    let mut state = state();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&deposit(1_000, 400)).expect("the deposit executes");
    executor.execute_transaction(&deposit(1_000, 0)).expect("the deposit executes");

    let [OpReceiptEnvelope::Deposit(moved), OpReceiptEnvelope::Deposit(minted)] =
        executor.receipts()
    else {
        panic!("two deposit receipts, got {:?}", executor.receipts());
    };
    assert!(
        moved.receipt.inner.status.coerce_status() && minted.receipt.inner.status.coerce_status()
    );
    assert_eq!(
        logs_of(&OpReceiptEnvelope::Deposit(moved.clone())),
        [transfer_log(DEPOSITOR, PAYEE, U256::from(400))],
        "the value's log, and none for the mint",
    );
    assert!(minted.receipt.inner.logs.is_empty(), "a mint alone is logged nowhere");
}

/// A system transaction writes the protocol's state and moves no value, so its receipt carries no
/// transfer log.
#[test]
fn test_a_system_transaction_receipt_logs_no_transfer() {
    let mut state = state();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");
    let set_slot = IOracle::setSlotCall { slot: U256::from(1), value: B256::repeat_byte(1) };
    let tx = legacy(
        MEGA_SYSTEM_ADDRESS,
        0,
        TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        0,
        set_slot.abi_encode().into(),
    );
    let outcome = executor.run_transaction(&tx).expect("the system transaction executes");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    executor.commit_transaction_outcome(outcome).expect("the block has room");

    let [receipt] = executor.receipts() else { panic!("one receipt") };
    assert!(!logs_of(receipt).iter().any(is_transfer_log), "no value moved, nothing logged");
}
