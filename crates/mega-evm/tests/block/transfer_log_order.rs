//! The order of an EIP-7708 transfer log among a contract's own logs, in a block's receipts.
//!
//! [S14.3] independent: each expected log is the event the bytecode emits, or the transfer of
//! the wei the program moves, built from those accounts and that amount.

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy, TxReceipt};
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{
    address, logs_bloom, Address, BloomInput, Bytes, Log, Signature, TxKind, B256, U256,
};
use mega_evm::{
    revm::primitives::eip7708::{ETH_TRANSFER_LOG_ADDRESS, ETH_TRANSFER_LOG_TOPIC},
    test_utils::{is_transfer_log, transfer_log, BytecodeBuilder},
    MegaTxEnvelope,
};
use op_alloy_consensus::OpReceiptEnvelope;
use revm::{
    bytecode::opcode::{LOG1, POP, PUSH0},
    database::State,
};

use crate::common::{self, executor, CALLER, CHAIN_ID};

/// A contract that emits one event and then destructs to `BENEFICIARY`.
const DESTRUCTOR: Address = address!("0x7100000000000000000000000000000000000001");
/// The account `DESTRUCTOR` sends its balance to.
const BENEFICIARY: Address = address!("0x7100000000000000000000000000000000000002");
/// A contract that creates an account and endows it.
const ACTOR: Address = address!("0x7100000000000000000000000000000000000003");

/// The wei each program moves, and the topic of the event it emits.
const MOVED: u64 = 7;
const MARKER: B256 = B256::repeat_byte(0x4d);

/// Regular room, the new account a creation adds, and sixty-four times the history of a few
/// records. The creation is forwarded all but a 64th of the gas, and its caller pays the records
/// from the 64th it keeps, at whatever a byte costs. The limit stays inside the block's.
fn gas_limit() -> u64 {
    let limit = 2_000_000 +
        2 * common::new_account_state_gas() +
        common::body_history(0) +
        64 * mega_evm::write_record_history_gas(4).expect("records have a price");
    assert!(limit <= common::BLOCK_GAS_LIMIT, "{limit} does not fit in the block");
    limit
}

fn event(address: Address) -> Log {
    Log::new_unchecked(address, vec![MARKER], Bytes::new())
}

fn legacy(nonce: u64, to: Address) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 1,
        gas_limit: gas_limit(),
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx, Signature::test_signature(), B256::repeat_byte(0x21));
    Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), CALLER)
}

fn state() -> State<mega_evm::test_utils::MemoryDatabase> {
    let init = BytecodeBuilder::default()
        .push_bytes(MARKER)
        .append_many([PUSH0, PUSH0, LOG1])
        .stop()
        .build();
    let mut db = common::database();
    db.set_account_code(
        DESTRUCTOR,
        BytecodeBuilder::default()
            .push_bytes(MARKER)
            .append_many([PUSH0, PUSH0, LOG1])
            .selfdestruct(BENEFICIARY)
            .build(),
    );
    db.set_account_balance(DESTRUCTOR, U256::from(MOVED));
    db.set_account_balance(BENEFICIARY, U256::from(1));
    db.set_account_code(
        ACTOR,
        BytecodeBuilder::default().create(U256::from(MOVED), init).append(POP).stop().build(),
    );
    db.set_account_balance(ACTOR, U256::from(MOVED));
    db.set_account_nonce(ACTOR, 0);
    State::builder().with_database(db).build()
}

/// The logs a receipt carries, having checked its bloom is theirs.
fn logs_of(receipt: &OpReceiptEnvelope) -> &[Log] {
    let logs = receipt.logs();
    assert_eq!(receipt.bloom(), logs_bloom(logs));
    assert!(logs.iter().any(is_transfer_log));
    let bloom = receipt.bloom();
    assert!(bloom.contains_input(BloomInput::Raw(ETH_TRANSFER_LOG_ADDRESS.as_slice())));
    assert!(bloom.contains_input(BloomInput::Raw(ETH_TRANSFER_LOG_TOPIC.as_slice())));
    logs
}

/// A block's receipts carry the same order the engine outcome does. [S14.3]
///
/// independent: the sequences are the event and the transfer, in the order each program
/// journals them.
#[test]
fn test_a_blocks_receipts_order_a_transfer_log_around_a_contract_event() {
    let destruct_logs =
        [event(DESTRUCTOR), transfer_log(DESTRUCTOR, BENEFICIARY, U256::from(MOVED))];
    let created = ACTOR.create(0);
    let create_logs = [transfer_log(ACTOR, created, U256::from(MOVED)), event(created)];

    let mut state = state();
    let mut executor = executor(&mut state, common::unlimited_ctx());
    executor.apply_pre_execution_changes().expect("the block starts");

    let txs = [legacy(0, DESTRUCTOR), legacy(1, ACTOR)];
    let expected = [destruct_logs.as_slice(), create_logs.as_slice()];
    for (i, tx) in txs.iter().enumerate() {
        let outcome = executor.run_transaction(tx).expect("it executes");
        assert!(outcome.result.is_success(), "transaction {i}: {:?}", outcome.result);
        assert_eq!(outcome.result.logs(), expected[i], "transaction {i}");
        executor.commit_transaction_outcome(outcome).expect("the block has room");
    }
    for (i, receipt) in executor.receipts().iter().enumerate() {
        assert_eq!(logs_of(receipt), expected[i], "receipt {i}");
    }
}
