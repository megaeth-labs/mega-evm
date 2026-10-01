//! Replaying a fixture from the witness of its own execution.
//!
//! [`check_replay`] executes an entry once on a database and environments that record every read
//! ([`RecordingDatabase`], [`RecordingEnvFactory`]), then twice more on a strict database and
//! environments that serve exactly a witness and refuse everything else ([`StrictDatabase`],
//! [`StrictEnvFactory`]): once on the record of every database read, once on the witness a node
//! builds from its channels — the accounts and slots the transaction's returned state names, as
//! the pre-state holds them, with the code the pre-state holds for those accounts, the block
//! hashes and buckets the engine exported, and the oracle reads the transaction recorded. Each
//! replay is held to the first run: the result, the state, the gas by ledger, the usage and the
//! stop, the state changes committed, and the buckets and block hashes the engine exported. A
//! replay that reads what its witness does not hold fails on the read; one that computes
//! something else fails on the comparison.
//!
//! The channel witness is what a stateless validator is given for the transaction, so its replay
//! is the check the witness must pass; the database-level replay shows the transaction reads
//! nothing outside its database and environments. An entry the engine rejects is not replayed on
//! the channels: it has no returned state, and is in no block.

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
        StrictEnvFactory, WitnessKeys, WitnessRecord,
    },
    BucketId, EmptyExternalEnv, ExternalEnvFactory, MegaTransactionOutcome, SaltEnv,
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
/// exactly the record and, when the engine executed it, on the witness a node builds from the
/// transaction's returned state and the engine's exports, and compares each replay to the first
/// run.
///
/// `Ok(None)` when every replay produced the same result, state, ledgers, usage, stop, state
/// changes and exports, having read nothing its witness does not hold; `Ok(Some(reason))` when
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

    // A replay that asks for a key its witness does not hold is refused the read and fails with
    // it, so the comparison is what catches a missing key.
    let strict = StrictEnvFactory::<EmptyExternalEnv>::replaying(&record);
    let replayed = run(
        mode,
        fork,
        StrictDatabase::new(record),
        strict.clone(),
        block.clone(),
        chain_id,
        tx.clone(),
    );

    compare(&recorded, &replayed)?;
    if !strict.oracle().replayed_exactly() {
        return Err("the oracle reads were not replayed in order".into());
    }

    // The channel witness: what a node builds for a transaction it includes, so only for an
    // entry the engine executed.
    let Ok(outcome) = &recorded.outcome else {
        return Ok(None);
    };
    let mut keys = WitnessKeys::default();
    keys.add_state(&outcome.state);
    let buckets = recorded
        .bucket_ids
        .iter()
        .map(|id| {
            (*id, EmptyExternalEnv.get_bucket_capacity(*id).map_err(|error| error.to_string()))
        })
        .collect();
    let witness = WitnessRecord::from_channels(
        &mut prestate(unit),
        &keys,
        recorded.block_hashes.clone(),
        buckets,
        outcome.oracle_reads.clone(),
    )
    .map_err(|error| format!("the pre-state could not be read: {error}"))?;
    let strict = StrictEnvFactory::<EmptyExternalEnv>::replaying(&witness);
    let channel =
        run(mode, fork, StrictDatabase::new(witness), strict.clone(), block, chain_id, tx);
    compare(&recorded, &channel).map_err(|why| format!("on the channel witness: {why}"))?;
    if !strict.oracle().replayed_exactly() {
        return Err("the oracle reads were not replayed in order on the channel witness".into());
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
