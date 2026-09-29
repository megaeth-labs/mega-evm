//! Replaying a fixture from the witness of its own execution.
//!
//! [`check_replay`] executes an entry once on a database and environments that record every read
//! ([`RecordingDatabase`], [`RecordingEnvFactory`]), then again on a strict database and
//! environments that serve exactly the record and refuse everything else ([`StrictDatabase`],
//! [`StrictEnvFactory`]), and holds the second run to the first: the result, the state, the gas
//! by ledger, the usage and the stop, the state changes committed, and the buckets and block
//! hashes the engine exported. A replay that reads what the record does not hold fails on the
//! read; one that computes something else fails on the comparison. The record is the witness a
//! stateless validator would be given for the transaction.

use std::collections::BTreeMap;

use mega_evm::{
    alloy_evm::Database,
    revm::{
        database::{
            states::bundle_state::BundleRetention, BundleState, CacheDB, CacheState, EmptyDB, State,
        },
        primitives::B256,
        state::{AccountInfo, Bytecode},
        DatabaseCommit,
    },
    test_utils::{
        RecordingDatabase, RecordingEnvFactory, SharedWitnessRecord, StrictDatabase,
        StrictEnvFactory, WitnessRecord,
    },
    BucketId, EmptyExternalEnv, ExternalEnvFactory, MegaTransactionOutcome,
};

use crate::{
    runner::{prepare, Ready},
    skips::SkipReason,
    types::{Test, TestUnit},
    Fork, Mode,
};

/// What one run of an entry produced.
struct Run {
    /// The transaction's outcome, or the error it was rejected with.
    outcome: Result<MegaTransactionOutcome, String>,
    /// The state changes committed.
    bundle: BundleState,
    /// The state cache after the run: everything loaded and committed.
    cache: CacheState,
    /// The buckets the engine exported.
    bucket_ids: Vec<BucketId>,
    /// The block hashes the engine exported.
    block_hashes: BTreeMap<u64, B256>,
}

/// The pre-state of `unit` as a database: every account with its code and storage.
fn prestate(unit: &TestUnit) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    for (address, account) in &unit.pre {
        let code = Bytecode::new_raw_checked(account.code.clone())
            .unwrap_or_else(|_| Bytecode::new_legacy(account.code.clone()));
        let info = AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: code.hash_slow(),
            code: Some(code),
            ..Default::default()
        };
        db.insert_account_info(*address, info);
        for (key, value) in &account.storage {
            db.insert_account_storage(*address, *key, *value).expect("an in-memory insert");
        }
    }
    db
}

/// Runs the entry on `db` with the environments `factory` makes, in `mode`.
fn run<DB, F>(
    mode: Mode,
    fork: Fork,
    db: DB,
    factory: F,
    block: mega_evm::revm::context::BlockEnv,
    chain_id: u64,
    tx: mega_evm::MegaTransaction,
) -> Run
where
    DB: Database,
    F: ExternalEnvFactory,
{
    let mut state = State::builder().with_database(db).with_bundle_update().build();
    let envs = factory.external_envs(block.number.saturating_to());
    let (outcome, bucket_ids, block_hashes) = {
        let mut evm = mode.evm_with_envs(fork, &mut state, block, chain_id, envs);
        let outcome = evm.execute_transaction(tx).map_err(|error| error.to_string());
        (outcome, evm.get_accessed_bucket_ids(), evm.get_accessed_block_hashes())
    };
    if let Ok(outcome) = &outcome {
        state.commit(outcome.state.clone());
    }
    state.merge_transitions(BundleRetention::Reverts);
    Run { outcome, bundle: state.take_bundle(), cache: state.cache, bucket_ids, block_hashes }
}

/// Executes the entry `test` of `unit` on `fork` in `mode`, records what it read, replays it on
/// exactly the record, and compares the two runs.
///
/// `Ok(None)` when the replay produced the same result, state, ledgers, usage, stop, state
/// changes and exports, having read nothing the record does not hold; `Ok(Some(reason))` when
/// the entry is not executed, for the reason the reference runner shares; `Err` with what
/// differed, or what the fixture lacks, otherwise.
pub fn check_replay(
    mode: Mode,
    fork: Fork,
    unit: &TestUnit,
    test: &Test,
) -> Result<Option<SkipReason>, String> {
    let Ready { chain_id, block, tx } =
        match prepare(fork, unit, test).map_err(|failure| failure.detail)? {
            Ok(ready) => ready,
            Err(reason) => return Ok(Some(reason)),
        };

    let record = SharedWitnessRecord::default();
    let recorded = run(
        mode,
        fork,
        RecordingDatabase::new(prestate(unit), record.clone()),
        RecordingEnvFactory::new(EmptyExternalEnv, record.clone()),
        block.clone(),
        chain_id,
        tx.clone(),
    );
    let record: WitnessRecord = record.take();

    let reads = SharedWitnessRecord::default();
    let strict = StrictEnvFactory::<EmptyExternalEnv>::replaying(&record);
    let replayed = run(
        mode,
        fork,
        RecordingDatabase::new(StrictDatabase::new(record.clone()), reads.clone()),
        RecordingEnvFactory::new(strict.clone(), reads.clone()),
        block,
        chain_id,
        tx,
    );
    let reads: WitnessRecord = reads.take();

    compare(&recorded, &replayed)?;
    if !record.covers(&reads) {
        return Err(format!(
            "the replay read what the record does not hold: {:?}",
            record.missing_from(&reads)
        ));
    }
    if !strict.oracle().replayed_exactly() {
        return Err("the oracle reads were not replayed in order".into());
    }
    Ok(None)
}

/// What differs between two runs of one entry, if anything.
fn compare(a: &Run, b: &Run) -> Result<(), String> {
    match (&a.outcome, &b.outcome) {
        (Ok(x), Ok(y)) => {
            if x.result != y.result {
                return Err(format!("result: {:?} against {:?}", x.result, y.result));
            }
            if x.gas != y.gas {
                return Err(format!("gas: {:?} against {:?}", x.gas, y.gas));
            }
            if x.usage != y.usage {
                return Err(format!("usage: {:?} against {:?}", x.usage, y.usage));
            }
            if x.limit_exceeded != y.limit_exceeded {
                return Err(format!("stop: {:?} against {:?}", x.limit_exceeded, y.limit_exceeded));
            }
            if x.state != y.state {
                return Err("the transaction's state differs".into());
            }
        }
        (Err(x), Err(y)) => {
            if x != y {
                return Err(format!("rejection: {x} against {y}"));
            }
        }
        (x, y) => {
            return Err(format!(
                "one run executed and the other was rejected: {:?} against {:?}",
                x.as_ref().map(|o| &o.result),
                y.as_ref().map(|o| &o.result)
            ))
        }
    }
    if a.bundle != b.bundle {
        return Err("the state changes differ".into());
    }
    if a.cache != b.cache {
        return Err("the state cache differs".into());
    }
    if a.bucket_ids != b.bucket_ids {
        return Err(format!("buckets: {:?} against {:?}", a.bucket_ids, b.bucket_ids));
    }
    if a.block_hashes != b.block_hashes {
        return Err(format!("block hashes: {:?} against {:?}", a.block_hashes, b.block_hashes));
    }
    Ok(())
}
