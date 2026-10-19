//! The KV count: the write records a transaction keeps, which its outcome reports.
//!
//! The KV count is the write-record count the common execution layer keeps: one record per
//! account or storage write the transaction keeps, deduplicated per frame, taken back with a slot
//! written back to its original value and with the frame that fails. The sender's account and the
//! accounts fees are credited to are part of the transaction body, not records, so a transaction
//! that writes nothing counts none.
//!
//! No limit holds the count: every record is 40 bytes of data size, which the data-size limit
//! holds, and pays its history gas.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitUsage, MegaEvm, MegaTransaction, MegaTransactionOutcome, TX_BODY_SIZE,
    WRITE_RECORD_SIZE,
};
use revm::bytecode::opcode::{CALL, GAS, POP, PUSH0, PUSH1, SELFDESTRUCT};

use crate::common::{call, call_with_data, context};

const CALLER: Address = address!("0000000000000000000000000000000000500000");
const A: Address = address!("0000000000000000000000000000000000500001");
const B: Address = address!("0000000000000000000000000000000000500002");

/// Enough gas for a few fresh slots and the calls around them.
const GAS_LIMIT: u64 = 100_000_000;

fn funded() -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(A, U256::from(1_000))
}

/// Appends writes of the fresh slots `0..n`.
fn write_slots(mut builder: BytecodeBuilder, n: u64) -> BytecodeBuilder {
    for slot in 0..n {
        builder = builder.sstore(U256::from(slot), U256::from(slot + 1));
    }
    builder
}

/// Code that writes the fresh slots `0..n` and stops.
fn slots(n: u64) -> Bytes {
    write_slots(BytecodeBuilder::default(), n).stop().build()
}

/// Appends a `CALL` to `target` carrying `value` with all the gas, dropping its success flag.
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

fn run_under(
    db: MemoryDatabase,
    tx: MegaTransaction,
    limits: EvmTxRuntimeLimits,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits)).execute_transaction(tx).unwrap()
}

/// `n` records and the body, with nothing else counted.
const fn records(n: u64) -> LimitUsage {
    LimitUsage { data_size: TX_BODY_SIZE + n * WRITE_RECORD_SIZE, write_records: n }
}

/// The KV count is the write records kept, by the rules every record follows: a slot counts on
/// its first change and not again, a slot written back counts nothing, writing zero to an empty
/// slot is no change, and the sender is the body's.
#[test]
fn test_the_kv_count_is_the_write_records_kept() {
    let cases: [(&str, Bytes, u64, u64); 7] = [
        ("an empty call", Bytes::new(), 0, 0),
        ("three fresh slots", slots(3), 3, 0),
        (
            "zero written to an empty slot",
            BytecodeBuilder::default().sstore(U256::ZERO, U256::ZERO).stop().build(),
            0,
            0,
        ),
        (
            "one slot written twice",
            BytecodeBuilder::default()
                .sstore(U256::ZERO, U256::from(1))
                .sstore(U256::ZERO, U256::from(2))
                .stop()
                .build(),
            1,
            0,
        ),
        (
            "one slot written and written back",
            BytecodeBuilder::default()
                .sstore(U256::ZERO, U256::from(1))
                .sstore(U256::ZERO, U256::ZERO)
                .stop()
                .build(),
            0,
            0,
        ),
        (
            "a value transfer: the frame's account and the recipient",
            then_call(BytecodeBuilder::default(), B, 1).stop().build(),
            2,
            1,
        ),
        (
            "a destruction that moves value: its beneficiary",
            BytecodeBuilder::default().push_address(B).append(SELFDESTRUCT).build(),
            1,
            1,
        ),
    ];
    for (name, code, kept, transfer_logs) in cases {
        let outcome = run_under(
            funded().account_code(A, code),
            call(CALLER, A, U256::ZERO, GAS_LIMIT),
            EvmTxRuntimeLimits::no_limits(),
        );
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        let transfer_log_bytes = transfer_logs * mega_evm::TRANSFER_LOG_SIZE;
        let expected = records(kept);
        assert_eq!(
            outcome.usage,
            LimitUsage { data_size: expected.data_size + transfer_log_bytes, ..expected },
            "{name}: the records, and the transfer log of the value it moved"
        );
    }

    let transfer = run_under(
        funded(),
        call(CALLER, B, U256::from(1), GAS_LIMIT),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(transfer.result.is_success());
    assert_eq!(
        transfer.usage,
        LimitUsage {
            data_size: records(1).data_size + mega_evm::TRANSFER_LOG_SIZE,
            write_records: 1
        },
        "a value transaction records its recipient alone, beside its transfer log"
    );
}

/// Calldata is data size and never a record: however much of it a transaction carries, its KV
/// count is what it writes.
#[test]
fn test_calldata_is_not_a_record() {
    let outcome = run_under(
        funded().account_code(A, slots(1)),
        call_with_data(CALLER, A, Bytes::from(vec![0xab; 1_000]), GAS_LIMIT),
        EvmTxRuntimeLimits::no_limits(),
    );
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.usage.write_records, 1);
}
