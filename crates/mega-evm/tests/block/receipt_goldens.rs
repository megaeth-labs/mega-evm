//! One block's receipts, pinned as the bytes a node hashes into the receipts root.
//!
//! A receipt's `cumulative_gas_used` is the running sum of [`mega_evm::MegaGasUsage::gas_used`].
//! That figure is what the transaction spent from both pools — regular, state and history gas
//! together — after the refund, and at least the EIP-7623 floor. It is also the block header's
//! gas used. The execution-gas ledger the block limit counts, `max(regular, floor)`, is not it.
//!
//! The encoded receipt carries the status, that cumulative gas, the logs bloom and the logs.
//! It does not carry revert data. The limit stop's `MegaLimitExceeded` payload is on the
//! execution outcome, and this test pins that payload beside the failed receipt.
//!
//! The exact bytes are the spec's own byte prices. At any other price the same block is still
//! checked: each bloom is its logs', each cumulative step is that transaction's `gas_used`, and
//! the receipts root is the ordered trie of the encodings.

use alloy_consensus::{transaction::Recovered, Sealed, Signed, TxLegacy, TxReceipt};
use alloy_eips::eip2718::Encodable2718;
use alloy_evm::block::BlockExecutor;
use alloy_primitives::{address, hex, logs_bloom, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::is_transfer_log, LimitCheck, LimitKind, MegaLimitExceeded, MegaTxEnvelope,
    ProtocolLimits, TX_BODY_SIZE,
};
use op_alloy_consensus::{OpReceiptEnvelope, TxDeposit};
use revm::{context::result::ExecutionResult, database::State};

use crate::common::{self, body_history, new_account_state_gas, system_tx, CALLER, CONTRACT};

/// The data-size limit the block holds every transaction to.
///
/// Above what the other five transactions keep, and below the stop transaction's body.
const DATA_SIZE_LIMIT: u64 = 2_000;

/// Calldata long enough that the stop transaction's body, and nothing it does afterwards, crosses
/// [`DATA_SIZE_LIMIT`].
const STOP_CALLDATA: usize = 1_700;

/// The account the value transfer pays, absent from the pre-state so the transfer creates it.
const PAYEE: Address = address!("0x1000000000000000000000000000000000000003");

/// The account whose code reverts, so one receipt is a failed transaction.
const REVERTER: Address = address!("0x1000000000000000000000000000000000000004");

/// The depositor, distinct from [`CALLER`], so the deposit does not spend the caller's nonce.
const DEPOSITOR: Address = address!("0x2000000000000000000000000000000000000003");

/// Init code that deploys empty runtime code: `PUSH0 PUSH0 RETURN`.
const EMPTY_INIT: &[u8] = &[0x5f, 0x5f, 0xf3];

/// Code that reverts with empty data: `PUSH0 PUSH0 REVERT`.
const REVERT_CODE: &[u8] = &[0x5f, 0x5f, 0xfd];

/// Whether this process prices bytes at the constants the spec fixes.
///
/// The exact receipt bytes move with those prices. A measurement build still runs the block and
/// checks the shape of the receipts; it does not compare the spec's bytes.
fn at_spec_prices() -> bool {
    if mega_evm::active_satin_prices().is_constants() {
        return true;
    }
    mega_evm::test_utils::note_price_guard(
        "receipt byte goldens are pinned at the spec's byte prices",
    );
    false
}

/// Gas for a legacy call or creation that keeps `records` write records and pays `state_gas`,
/// with `calldata` bytes of calldata, at the byte prices in effect.
fn room(calldata: u64, records: u64, state_gas: u64) -> u64 {
    1_000_000 +
        body_history(calldata) +
        mega_evm::write_record_history_gas(records).expect("the records have a price") +
        state_gas
}

/// A legacy transaction from [`CALLER`] with its own value and calldata.
fn legacy(
    nonce: u64,
    to: TxKind,
    value: u64,
    input: Bytes,
    gas_limit: u64,
    hash_byte: u8,
) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(common::CHAIN_ID),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to,
        value: U256::from(value),
        input,
    };
    Recovered::new_unchecked(
        MegaTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            Signature::test_signature(),
            B256::repeat_byte(hash_byte),
        )),
        CALLER,
    )
}

/// A deposit from [`DEPOSITOR`] to [`CONTRACT`].
fn depositor_tx() -> Recovered<MegaTxEnvelope> {
    let deposit = TxDeposit {
        source_hash: B256::repeat_byte(0xd1),
        from: DEPOSITOR,
        to: TxKind::Call(CONTRACT),
        mint: 0,
        value: U256::ZERO,
        gas_limit: room(0, 2, 0),
        is_system_transaction: false,
        input: Bytes::new(),
    };
    Recovered::new_unchecked(
        MegaTxEnvelope::Deposit(Sealed::new_unchecked(deposit, B256::repeat_byte(0xd1))),
        DEPOSITOR,
    )
}

/// The state the block runs on: a funded depositor and a contract that reverts.
fn state() -> State<mega_evm::test_utils::MemoryDatabase> {
    let mut db = common::database();
    db.set_account_balance(DEPOSITOR, U256::from(1_000_000_000_000_000_u64));
    db.set_account_code(REVERTER, Bytes::from_static(REVERT_CODE));
    State::builder().with_database(db).build()
}

/// What one included transaction contributed, read before its receipt is built.
struct Kept {
    /// [`mega_evm::MegaGasUsage::gas_used`], the figure the receipt adds.
    gas_used: u64,
    /// The limit stop's revert data, when this transaction is the stop.
    stop: Option<Bytes>,
}

/// The receipts root of the six encodings at the spec's byte prices.
const RECEIPTS_ROOT: &str = "708f8df343aed051aa13edc905fe25603761aeb16dc431260497b6ebf479caa1";

/// `cumulative_gas_used` of each receipt at the spec's byte prices.
const CUMULATIVE_GAS: [u64; 6] = [15_000, 250_400, 469_480, 511_764, 750_482, 951_664];

/// The logs bloom of a receipt that carries no logs.
const EMPTY_BLOOM: &str = "00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

/// The logs bloom of the value transfer, whose one log is the EIP-7708 transfer.
const TRANSFER_BLOOM: &str = "00000000000000000000080000000000000000000000000004000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000008000000000000000000000000000000000000001000400000000000000004000000000000000000000002000000000000000000000000";

/// Each receipt's logs bloom at the spec's byte prices.
const BLOOMS: [&str; 6] =
    [EMPTY_BLOOM, TRANSFER_BLOOM, EMPTY_BLOOM, EMPTY_BLOOM, EMPTY_BLOOM, EMPTY_BLOOM];

/// Each receipt's EIP-2718 encoding at the spec's byte prices.
const ENCODINGS: [&str; 6] = [
    // the deposit
    "7ef9010a01823a98b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c08001",
    // the value transfer
    "f901a7018303d220b9010000000000000000000000080000000000000000000000000004000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000008000000000000000000000000000000000000001000400000000000000004000000000000000000000002000000000000000000000000f89df89b94fffffffffffffffffffffffffffffffffffffffef863a0ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3efa00000000000000000000000002000000000000000000000000000000000000002a00000000000000000000000001000000000000000000000000000000000000003a00000000000000000000000000000000000000000000000000000000000000001",
    // the limit stop
    "f9010980830729e8b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0",
    // the failed call
    "f90109808307cf14b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0",
    // the creation
    "f9010901830b7392b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0",
    // the system-address transaction
    "f9010901830e8570b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c0",
];

/// The six receipts of one block, and the receipts root of their encodings.
#[test]
fn test_a_blocks_receipts_pin_their_bytes_bloom_and_cumulative_gas() {
    let mut state = state();
    let mut executor = common::executor_with_limits(
        &mut state,
        ProtocolLimits::loosest()
            .with_tx_runtime_limits(common::loosest_tx().with_tx_data_size_limit(DATA_SIZE_LIMIT)),
    );
    executor.apply_pre_execution_changes().expect("the block starts");

    let stop_input = Bytes::from(vec![0xab; STOP_CALLDATA]);
    assert!(
        TX_BODY_SIZE + STOP_CALLDATA as u64 > DATA_SIZE_LIMIT,
        "the stop's body crosses the limit before it runs",
    );
    let txs = [
        depositor_tx(),
        legacy(0, TxKind::Call(PAYEE), 1, Bytes::new(), room(0, 4, new_account_state_gas()), 0x11),
        legacy(1, TxKind::Call(CONTRACT), 0, stop_input, room(STOP_CALLDATA as u64, 0, 0), 0x22),
        legacy(2, TxKind::Call(REVERTER), 0, Bytes::new(), room(0, 2, 0), 0x33),
        legacy(
            3,
            TxKind::Create,
            0,
            Bytes::from_static(EMPTY_INIT),
            room(EMPTY_INIT.len() as u64, 4, new_account_state_gas()),
            0x44,
        ),
        system_tx(),
    ];

    let mut kept = Vec::with_capacity(txs.len());
    for (index, tx) in txs.iter().enumerate() {
        let outcome = executor
            .run_transaction(tx)
            .unwrap_or_else(|err| panic!("transaction {index} executes: {err}"));
        let stop = match &outcome.inner.result {
            ExecutionResult::Revert { output, .. } => Some(output.clone()),
            ExecutionResult::Success { .. } => None,
            other => panic!("transaction {index} halted: {other:?}"),
        };
        if index == 2 {
            assert_eq!(
                outcome.inner.limit_exceeded,
                Some(LimitCheck::ExceedsLimit {
                    kind: LimitKind::DataSize,
                    limit: DATA_SIZE_LIMIT,
                    used: TX_BODY_SIZE + STOP_CALLDATA as u64,
                    frame_local: false,
                }),
                "the body crossed the data-size limit",
            );
            let output = stop.clone().expect("the stop reverts");
            assert_eq!(
                output.as_ref(),
                MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: DATA_SIZE_LIMIT }
                    .abi_encode(),
                "the stop's revert data is MegaLimitExceeded",
            );
        } else {
            assert!(
                outcome.inner.limit_exceeded.is_none(),
                "transaction {index} is not a limit stop: {:?}",
                outcome.inner.limit_exceeded,
            );
        }
        kept.push(Kept { gas_used: outcome.inner.gas.gas_used, stop });
        executor
            .commit_transaction_outcome(outcome)
            .unwrap_or_else(|err| panic!("transaction {index} is included: {err}"));
    }

    let (_, result) = executor.finish_with_counters().expect("the block finishes");
    let receipts = result.receipts();
    assert_eq!(receipts.len(), 6);

    let mut cumulative = 0_u64;
    let mut encoded = Vec::with_capacity(receipts.len());
    for (index, (receipt, kept)) in receipts.iter().zip(&kept).enumerate() {
        cumulative += kept.gas_used;
        assert_eq!(
            receipt.cumulative_gas_used(),
            cumulative,
            "receipt {index} adds this transaction's gas_used",
        );
        assert_eq!(
            receipt.bloom(),
            logs_bloom(receipt.logs()),
            "receipt {index}'s bloom is its logs'",
        );
        encoded.push(receipt.encoded_2718());
    }

    assert!(receipts[0].status(), "the deposit succeeds");
    match &receipts[0] {
        OpReceiptEnvelope::Deposit(deposit) => {
            assert_eq!(
                deposit.receipt.deposit_nonce,
                Some(0),
                "the depositor's nonce before the deposit",
            );
            assert_eq!(
                deposit.receipt.deposit_receipt_version,
                Some(1),
                "Canyon's receipt version",
            );
            assert!(deposit.receipt.inner.logs.is_empty(), "a zero-value deposit emits no log");
        }
        other => panic!("the deposit's receipt is a deposit receipt, got {other:?}"),
    }

    assert!(receipts[1].status(), "the value transfer succeeds");
    let transfer_logs = receipts[1].logs();
    assert_eq!(transfer_logs.len(), 1, "the value transfer emits its EIP-7708 log");
    assert!(is_transfer_log(&transfer_logs[0]), "that log is a transfer");

    assert!(!receipts[2].status(), "the limit stop is a failed receipt");
    assert!(receipts[2].logs().is_empty(), "the stop reverts before any log");
    assert!(kept[2].stop.is_some(), "the outcome carried MegaLimitExceeded");

    assert!(!receipts[3].status(), "the reverting call is a failed receipt");
    assert!(receipts[3].logs().is_empty());
    assert!(
        kept[3].stop.as_ref().is_some_and(|data| data.is_empty()),
        "the contract reverted with no data",
    );

    assert!(receipts[4].status(), "the creation succeeds");
    assert!(receipts[4].logs().is_empty(), "a zero-value creation emits no transfer log");

    assert!(receipts[5].status(), "the system-address transaction succeeds");
    assert!(
        !matches!(receipts[5], OpReceiptEnvelope::Deposit(_)),
        "the block receipt follows the legacy envelope, before the engine promotes it",
    );

    let root = alloy_trie::root::ordered_trie_root_with_encoder(&encoded, |receipt, buf| {
        buf.extend_from_slice(receipt);
    });

    if !at_spec_prices() {
        return;
    }
    assert_eq!(hex::encode(root), RECEIPTS_ROOT, "the receipts root");
    for (index, (receipt, bytes)) in receipts.iter().zip(&encoded).enumerate() {
        assert_eq!(
            receipt.cumulative_gas_used(),
            CUMULATIVE_GAS[index],
            "receipt {index} cumulative gas",
        );
        assert_eq!(hex::encode(receipt.bloom()), BLOOMS[index], "receipt {index} bloom");
        assert_eq!(hex::encode(bytes), ENCODINGS[index], "receipt {index} bytes");
    }
}
