//! The canonical record of a transaction's whole outcome, and its hash.
//!
//! A fixture's verdict says whether the post-state root and the logs hash match the fixture's; it
//! says nothing of the gas a transaction spent on each ledger, of the bytes its limits counted, of
//! how it stopped or of the state beneath the root. A change that moves gas between ledgers and
//! keeps the receipt's total, or changes `usage` alone, leaves every verdict as it was. The digest
//! is the record such a change cannot leave alone: everything [`MegaTransactionOutcome`] holds,
//! rendered in one fixed encoding, so two runs of the same fixtures at two revisions can be
//! compared line by line, and a mismatch shows which field differs.
//!
//! The encoding is canonical: a JSON value whose objects list their keys in sorted order, whose
//! 256-bit numbers are `0x`-prefixed lowercase hex without leading zeros, and whose hashes,
//! addresses and byte strings are full-width lowercase hex. Maps are sorted by key before they
//! are written, so nothing in it depends on a hash map's iteration order or on a memory address.
//! The one place that renders a type through `Debug` is a halt or success reason and a rejected
//! transaction's error, whose names are the types' own; a dependency changing those names moves
//! the digest of the transactions that end that way, and the full form shows it.
//!
//! The outcome says nothing of what a transaction read on its way to it. A change that keeps every
//! outcome and reads other state — a code it no longer loads, a block hash, a SALT bucket it now
//! asks for, an account a rejected transaction no longer loads — moves no digest, and changes the
//! witness a stateless validator must be given. [`reads`] is the record of what the engine read
//! below its journal, in the same encoding: every account, code, storage slot and block hash the
//! database served, with the answers, every SALT bucket and Oracle slot the external environment
//! answered, as a recording database and environment collect them ([`WitnessRecord`]). Its hash is
//! the read-set digest two runs are compared by, as they are by the outcome's.

use std::collections::BTreeMap;

use mega_evm::{
    revm::{
        context::result::{ExecutionResult, ResultGas},
        primitives::{keccak256, Address, Log, B256, U256},
        state::{Account, AccountInfo, EvmState},
    },
    test_utils::WitnessRecord,
    LimitCheck, LimitUsage, MegaGasUsage, MegaHaltReason, MegaTransactionOutcome, OracleRead,
};
use serde_json::{Map, Value};

/// What a transaction ended in: its outcome, or the error it was rejected with.
pub type Executed<'a> = Result<&'a MegaTransactionOutcome, &'a str>;

/// A JSON object whose keys are listed in sorted order, whichever order the map keeps.
fn object(entries: impl IntoIterator<Item = (String, Value)>) -> Value {
    let sorted: BTreeMap<String, Value> = entries.into_iter().collect();
    let mut map = Map::new();
    for (key, value) in sorted {
        map.insert(key, value);
    }
    Value::Object(map)
}

fn fields<const N: usize>(entries: [(&str, Value); N]) -> Value {
    object(entries.into_iter().map(|(key, value)| (key.to_string(), value)))
}

/// A 256-bit number as `0x`-prefixed lowercase hex without leading zeros.
fn number(value: U256) -> Value {
    Value::String(format!("{value:#x}"))
}

fn hex(bytes: impl AsRef<[u8]>) -> Value {
    Value::String(format!("0x{}", to_hex(bytes.as_ref())))
}

fn to_hex(bytes: &[u8]) -> String {
    use core::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        write!(out, "{byte:02x}").expect("writing to a string cannot fail");
        out
    })
}

fn address(address: &Address) -> String {
    format!("0x{}", to_hex(address.as_slice()))
}

/// A result's gas, with the reservoir the transaction left beside its other fields: revm's
/// [`ResultGas`] does not carry it, the outcome does ([`MegaGasUsage::reservoir_remaining`]).
fn result_gas(gas: &ResultGas, reservoir_remaining: u64) -> Value {
    fields([
        ("block_regular_gas_used", gas.block_regular_gas_used().into()),
        ("block_state_gas_used", gas.block_state_gas_used().into()),
        ("final_refunded", gas.final_refunded().into()),
        ("floor_gas", gas.floor_gas().into()),
        ("inner_refunded", gas.inner_refunded().into()),
        ("reservoir_remaining", reservoir_remaining.into()),
        ("state_gas_spent_final", gas.state_gas_spent_final().into()),
        ("total_gas_spent", gas.total_gas_spent().into()),
        ("tx_gas_used", gas.tx_gas_used().into()),
    ])
}

fn gas_usage(gas: &MegaGasUsage) -> Value {
    let MegaGasUsage {
        regular,
        state,
        history,
        history_bytes,
        reservoir_remaining,
        floor,
        gas_used,
    } = *gas;
    fields([
        ("floor", floor.into()),
        ("gas_used", gas_used.into()),
        ("history", history.into()),
        ("history_bytes", history_bytes.into()),
        ("regular", regular.into()),
        ("reservoir_remaining", reservoir_remaining.into()),
        ("state", state.into()),
    ])
}

fn usage(usage: &LimitUsage) -> Value {
    let LimitUsage { data_size, write_records, state_growth } = *usage;
    fields([
        ("data_size", data_size.into()),
        ("state_growth", state_growth.into()),
        ("write_records", write_records.into()),
    ])
}

fn log(log: &Log) -> Value {
    fields([
        ("address", address(&log.address).into()),
        ("data", hex(&log.data.data)),
        ("topics", Value::Array(log.data.topics().iter().map(hex).collect())),
    ])
}

fn logs(logs: &[Log]) -> Value {
    Value::Array(logs.iter().map(log).collect())
}

fn limit_check(check: &LimitCheck) -> Value {
    match check {
        LimitCheck::WithinLimit => fields([("check", "within-limit".into())]),
        LimitCheck::ExceedsLimit { kind, limit, used, frame_local } => fields([
            ("check", "exceeds-limit".into()),
            ("frame_local", (*frame_local).into()),
            ("kind", format!("{kind:?}").into()),
            ("limit", (*limit).into()),
            ("used", (*used).into()),
        ]),
    }
}

fn oracle_read(read: &OracleRead) -> Value {
    fields([("answer", read.answer.map_or(Value::Null, number)), ("slot", number(read.slot))])
}

fn account(account: &Account) -> Value {
    let storage = object(account.storage.iter().map(|(key, slot)| {
        (
            format!("{key:#066x}"),
            fields([
                ("original", number(slot.original_value)),
                ("present", number(slot.present_value)),
            ]),
        )
    }));
    fields([
        ("balance", number(account.info.balance)),
        ("code_hash", hex(account.info.code_hash)),
        ("nonce", account.info.nonce.into()),
        ("status", account.status.bits().into()),
        (
            "status_flags",
            Value::Array(account.status.iter_names().map(|(name, _)| name.into()).collect()),
        ),
        ("storage", storage),
    ])
}

fn state(state: &EvmState) -> Value {
    object(state.iter().map(|(addr, account_)| (address(addr), account(account_))))
}

fn result(result: &ExecutionResult<MegaHaltReason>, reservoir_remaining: u64) -> Value {
    match result {
        ExecutionResult::Success { reason, gas, logs: l, output } => fields([
            ("created", output.address().map_or(Value::Null, |a| address(a).into())),
            ("gas", result_gas(gas, reservoir_remaining)),
            ("logs", logs(l)),
            ("output", hex(output.data())),
            ("reason", format!("{reason:?}").into()),
            ("status", "success".into()),
        ]),
        ExecutionResult::Revert { gas, logs: l, output } => fields([
            ("gas", result_gas(gas, reservoir_remaining)),
            ("logs", logs(l)),
            ("output", hex(output)),
            ("status", "revert".into()),
        ]),
        ExecutionResult::Halt { reason, gas, logs: l } => fields([
            ("gas", result_gas(gas, reservoir_remaining)),
            ("logs", logs(l)),
            ("reason", format!("{reason:?}").into()),
            ("status", "halt".into()),
        ]),
    }
}

/// The canonical record of an executed transaction.
pub fn canonical(executed: Executed<'_>) -> Value {
    match executed {
        Ok(outcome) => fields([
            ("gas", gas_usage(&outcome.gas)),
            ("limit_exceeded", outcome.limit_exceeded.as_ref().map_or(Value::Null, limit_check)),
            ("oracle_reads", Value::Array(outcome.oracle_reads.iter().map(oracle_read).collect())),
            ("result", result(&outcome.result, outcome.gas.reservoir_remaining)),
            ("state", state(&outcome.state)),
            ("usage", usage(&outcome.usage)),
        ]),
        Err(error) => fields([("rejected", error.into())]),
    }
}

/// The record of a test that panicked.
pub fn panicked(message: &str) -> Value {
    fields([("panic", message.into())])
}

/// What the database answered for an account: its balance, code hash and nonce, or nothing for
/// one it does not hold. The code itself is read by hash, and is in the record's codes.
fn account_answer(info: Option<&AccountInfo>) -> Value {
    info.map_or(Value::Null, |info| {
        fields([
            ("balance", number(info.balance)),
            ("code_hash", hex(info.code_hash)),
            ("nonce", info.nonce.into()),
        ])
    })
}

/// The canonical record of what the engine read below its journal: the database's accounts,
/// codes, storage slots and block hashes, with the answers it served, and the external
/// environment's SALT buckets and Oracle slots, with its answers. A key read twice is listed
/// once, every list but the Oracle reads in key order; the Oracle reads keep the order they were
/// made in, which a replay answers them in. The hints the Oracle service received are not reads,
/// and are not in it: the outcome's data size counts them.
pub fn reads(record: &WitnessRecord) -> Value {
    let WitnessRecord { accounts, storage, codes, block_hashes, buckets, oracle_reads, hints: _ } =
        record;
    let mut slots: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for ((owner, key), value) in storage {
        slots.entry(address(owner)).or_default().insert(format!("{key:#066x}"), number(*value));
    }
    fields([
        (
            "accounts",
            object(
                accounts.iter().map(|(addr, info)| (address(addr), account_answer(info.as_ref()))),
            ),
        ),
        (
            "block_hashes",
            Value::Array(
                block_hashes
                    .iter()
                    .map(|(block, hash)| fields([("hash", hex(hash)), ("number", (*block).into())]))
                    .collect(),
            ),
        ),
        (
            "buckets",
            Value::Array(
                buckets
                    .iter()
                    .map(|(bucket, answer)| {
                        let answer = match answer {
                            Ok(capacity) => fields([("capacity", (*capacity).into())]),
                            Err(error) => fields([("error", error.as_str().into())]),
                        };
                        fields([("answer", answer), ("bucket", (*bucket).into())])
                    })
                    .collect(),
            ),
        ),
        ("codes", Value::Array(codes.keys().map(hex).collect())),
        ("oracle_reads", Value::Array(oracle_reads.iter().map(oracle_read).collect())),
        ("storage", object(slots.into_iter().map(|(owner, slots)| (owner, object(slots))))),
    ])
}

/// The digest of a canonical record: the Keccak-256 of its JSON text.
pub fn hash(canonical: &Value) -> B256 {
    keccak256(text(canonical))
}

/// A canonical record's JSON text, on one line.
pub fn text(canonical: &Value) -> String {
    serde_json::to_string(canonical).expect("a JSON value serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::revm::{
        context::result::{Output, ResultAndState, SuccessReason},
        primitives::Bytes,
        state::{AccountInfo, EvmStorageSlot},
    };

    fn gas() -> ResultGas {
        ResultGas::default()
            .with_total_gas_spent(100_000)
            .with_state_gas_spent(30_000)
            .with_refunded(4_000)
            .with_floor_gas(21_000)
    }

    fn outcome() -> MegaTransactionOutcome {
        let mut state = EvmState::default();
        let mut account = Account::from(AccountInfo { nonce: 3, ..Default::default() });
        account.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::from(7), U256::from(9), Default::default()),
        );
        account.mark_touch();
        state.insert(Address::repeat_byte(0xaa), account);
        state.insert(Address::repeat_byte(0x11), Account::default());
        MegaTransactionOutcome {
            result_and_state: ResultAndState::new(
                ExecutionResult::Success {
                    reason: SuccessReason::Stop,
                    gas: gas(),
                    logs: Vec::new(),
                    output: Output::Call(Bytes::from_static(&[1, 2])),
                },
                state,
            ),
            gas: MegaGasUsage::new(&gas(), 10_000, 125, 5_000),
            usage: LimitUsage { data_size: 500, write_records: 3, state_growth: 0 },
            limit_exceeded: None,
            oracle_reads: vec![OracleRead { slot: U256::from(4), answer: Some(U256::from(5)) }],
        }
    }

    fn digest(outcome: &MegaTransactionOutcome) -> B256 {
        hash(&canonical(Ok(outcome)))
    }

    /// The same outcome, built twice, has the same text and the same hash, whatever order its
    /// maps were filled in.
    #[test]
    fn test_the_digest_is_stable() {
        let (a, b) = (outcome(), outcome());
        assert_eq!(text(&canonical(Ok(&a))), text(&canonical(Ok(&b))));
        assert_eq!(digest(&a), digest(&b));

        let mut reordered = outcome();
        let accounts: Vec<_> = reordered.state.drain().collect();
        for (addr, account) in accounts.into_iter().rev() {
            reordered.state.insert(addr, account);
        }
        assert_eq!(digest(&a), digest(&reordered));
    }

    /// The text names its keys in sorted order, whichever order the JSON map keeps.
    #[test]
    fn test_the_text_lists_keys_in_sorted_order() {
        let text = text(&canonical(Ok(&outcome())));
        assert!(text.starts_with("{\"gas\":{\"floor\":"), "{text}");
        let markers = [
            ",\"limit_exceeded\":",
            ",\"oracle_reads\":",
            ",\"result\":{",
            ",\"state\":{\"0x11",
            ",\"usage\":",
        ];
        let positions: Vec<_> = markers.iter().map(|m| text.find(m).expect(m)).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{text}");
    }

    /// Moving history gas to regular gas with the total kept changes the digest, though the
    /// receipt's figures stay the same.
    #[test]
    fn test_one_gas_field_moves_the_digest() {
        let base = outcome();
        let mut moved = outcome();
        moved.gas.history -= 100;
        moved.gas.regular += 100;
        assert_eq!(base.gas.regular + base.gas.history, moved.gas.regular + moved.gas.history);
        assert_eq!(base.gas.gas_used, moved.gas.gas_used);
        assert_ne!(digest(&base), digest(&moved));
    }

    /// Each of `usage`'s counts moves the digest on its own.
    #[test]
    fn test_usage_alone_moves_the_digest() {
        let base = outcome();
        let mut data = outcome();
        data.usage.data_size += 1;
        let mut records = outcome();
        records.usage.write_records += 1;
        let mut growth = outcome();
        growth.usage.state_growth += 1;
        assert_ne!(digest(&base), digest(&data));
        assert_ne!(digest(&base), digest(&records));
        assert_ne!(digest(&base), digest(&growth));
    }

    /// A slot's original value alone, its present value kept, moves the digest.
    #[test]
    fn test_an_original_storage_value_alone_moves_the_digest() {
        let base = outcome();
        let mut moved = outcome();
        moved
            .state
            .get_mut(&Address::repeat_byte(0xaa))
            .unwrap()
            .storage
            .get_mut(&U256::from(1))
            .unwrap()
            .original_value = U256::from(8);
        assert_ne!(digest(&base), digest(&moved));
    }

    /// A rejection and a panic are records of their own, which differ from an outcome's and from
    /// each other.
    #[test]
    fn test_a_rejection_and_a_panic_have_their_own_records() {
        let rejected = hash(&canonical(Err("nonce too low")));
        assert_ne!(rejected, digest(&outcome()));
        assert_ne!(rejected, hash(&panicked("nonce too low")));
    }

    /// A record of one read on each channel: an account the database holds and one it does not,
    /// a code, a slot, a block hash, a bucket it answered and one it failed, and an Oracle read.
    fn record() -> WitnessRecord {
        let mut record = WitnessRecord::default();
        record.accounts.insert(
            Address::repeat_byte(0xaa),
            Some(AccountInfo { nonce: 1, balance: U256::from(5), ..Default::default() }),
        );
        record.accounts.insert(Address::repeat_byte(0x11), None);
        record.codes.insert(B256::repeat_byte(0xc0), Default::default());
        record.storage.insert((Address::repeat_byte(0xaa), U256::from(1)), U256::from(7));
        record.block_hashes.insert(9, B256::repeat_byte(0x09));
        record.buckets.insert(3, Ok(256));
        record.buckets.insert(4, Err("unavailable".into()));
        record.oracle_reads.push(OracleRead { slot: U256::from(2), answer: None });
        record
    }

    fn read_digest(record: &WitnessRecord) -> B256 {
        hash(&reads(record))
    }

    /// The record lists its channels and each channel's keys in sorted order, and keeps the order
    /// of the Oracle reads, which a replay answers in order.
    #[test]
    fn test_the_read_set_lists_keys_in_order_and_keeps_the_oracle_order() {
        let text = text(&reads(&record()));
        let markers = [
            "{\"accounts\":{\"0x1111",
            ",\"0xaaaa",
            "},\"block_hashes\":",
            ",\"buckets\":[{\"answer\":{\"capacity\":256},\"bucket\":3}",
            ",{\"answer\":{\"error\":\"unavailable\"},\"bucket\":4}",
            ",\"codes\":",
            ",\"oracle_reads\":",
            ",\"storage\":{\"0xaaaa",
        ];
        let positions: Vec<_> = markers.iter().map(|m| text.find(m).expect(m)).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{text}");

        let mut two = record();
        two.oracle_reads.push(OracleRead { slot: U256::from(8), answer: Some(U256::ONE) });
        let mut swapped = two.clone();
        swapped.oracle_reads.swap(0, 1);
        assert_ne!(read_digest(&two), read_digest(&swapped));
    }

    /// One more read on any channel moves the record's digest: an account, absent or held, a
    /// code, a slot, a block hash, a bucket and an Oracle read.
    #[test]
    fn test_one_more_read_moves_the_read_set() {
        let base = read_digest(&record());
        let more: [fn(&mut WitnessRecord); 7] = [
            |r| {
                r.accounts.insert(Address::repeat_byte(0x22), None);
            },
            |r| {
                r.accounts.insert(Address::repeat_byte(0x33), Some(AccountInfo::default()));
            },
            |r| {
                r.codes.insert(B256::repeat_byte(0xc1), Default::default());
            },
            |r| {
                r.storage.insert((Address::repeat_byte(0xaa), U256::from(2)), U256::ZERO);
            },
            |r| {
                r.block_hashes.insert(10, B256::repeat_byte(0x0a));
            },
            |r| {
                r.buckets.insert(5, Ok(256));
            },
            |r| r.oracle_reads.push(OracleRead { slot: U256::from(2), answer: None }),
        ];
        for add in more {
            let mut record = record();
            add(&mut record);
            assert_ne!(read_digest(&record), base, "{}", text(&reads(&record)));
        }
    }

    /// What a source answered for a key, not only the key, is in the record.
    #[test]
    fn test_an_answer_alone_moves_the_read_set() {
        let base = read_digest(&record());
        let mut balance = record();
        balance.accounts.get_mut(&Address::repeat_byte(0xaa)).unwrap().as_mut().unwrap().balance =
            U256::from(6);
        let mut slot = record();
        slot.storage.insert((Address::repeat_byte(0xaa), U256::from(1)), U256::from(8));
        let mut bucket = record();
        bucket.buckets.insert(3, Ok(512));
        let mut oracle = record();
        oracle.oracle_reads[0].answer = Some(U256::ZERO);
        for moved in [balance, slot, bucket, oracle] {
            assert_ne!(read_digest(&moved), base);
        }
    }
}
