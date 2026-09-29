//! A witness of what execution read, and the databases and environments that record one and
//! replay from one.
//!
//! A stateless validator re-executes a block from a witness: the accounts, slots, code and block
//! hashes the block read, as the chain held them, and the SALT bucket capacities its state charges
//! were priced with. [`WitnessRecord`] is that set, and a [`StrictDatabase`] and a
//! [`StrictEnvFactory`] serve it back and nothing else: a replay that asks for a key the record
//! does not hold fails with a [`WitnessError`], the way a validator's database fails on a missing
//! code or bucket, and the way it must not silently answer a missing account or slot.
//!
//! A record is built two ways, and the two are different checks.
//!
//! - **From the channels a node has**, with [`WitnessRecord::from_channels`]: the accounts and
//!   slots the pre-block states and the included transactions' returned states name, collected by
//!   [`WitnessKeys`] and resolved against the state the block ran on; the code those states carry
//!   and the chain holds for those accounts; the block hashes and buckets the engine exported; and
//!   the oracle reads the included transactions recorded. That is the witness a node builds, and a
//!   replay on it is the check a validator's witness must pass: a read the engine makes outside
//!   every state and export is a key the record lacks, and the replay fails on it.
//! - **From every read the database served**, with a [`RecordingDatabase`] and the
//!   [`RecordingEnvFactory`]'s environments around the block: what a recorder at the database level
//!   sees. A replay on it shows the block reads nothing outside its database and environments and
//!   computes the same block twice; it cannot show a read is missing from the channels, because the
//!   recorder sees every read wherever it lands.
//!
//! The recording database serves code lazily, as a node's database does: an account comes back
//! without its bytecode, and the bytecode is served by hash on request. So every code the engine
//! needs travels through `code_by_hash`, and both records hold what a validator, which serves
//! code by hash, must be given.
//!
//! The oracle service is the one source a replay cannot take from the chain: the recording
//! environment keeps every answer the service gave, in order, the engine records each
//! transaction's own on its outcome, and the [`ReplayingOracleEnv`] answers the same reads the
//! same way — or answers nothing, as a validator without an oracle service does, to show what a
//! replay then depends on.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{
    cell::{Cell, RefCell},
    marker::PhantomData,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    format,
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};

use alloy_primitives::{Address, BlockNumber, Bytes, B256, U256};
use revm::{
    database::DBErrorMarker,
    primitives::{HashMap, StorageKey, StorageValue},
    state::{AccountInfo, Bytecode, EvmState},
    Database, DatabaseCommit,
};

use crate::{
    BucketId, ExternalEnvFactory, ExternalEnvTypes, ExternalEnvs, OracleEnv, OracleRead,
    RecordedHint, SaltEnv,
};

/// What execution read outside the transactions and the header, as the sources answered it.
///
/// An absent account is recorded as `None` and a zero slot as zero, so a replay that asks for
/// either is answered as the original run was, and one that asks for a key not here is refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WitnessRecord {
    /// Every account asked for, with the answer: `None` for one the database does not hold. The
    /// bytecode is not carried here; it is served by hash from [`codes`](Self::codes).
    pub accounts: BTreeMap<Address, Option<AccountInfo>>,
    /// Every slot asked for, with its value, zeroes included.
    pub storage: BTreeMap<(Address, StorageKey), StorageValue>,
    /// Every code asked for, by hash.
    pub codes: BTreeMap<B256, Bytecode>,
    /// Every block hash asked for, by number.
    pub block_hashes: BTreeMap<u64, B256>,
    /// Every SALT bucket asked for, with the capacity the environment answered or the message it
    /// failed with.
    pub buckets: BTreeMap<BucketId, Result<u64, String>>,
    /// Every read of the oracle service, in order: the slot and what the service answered.
    pub oracle_reads: Vec<OracleRead>,
    /// Every hint the oracle service received, in order.
    pub hints: Vec<RecordedHint>,
}

/// The keys a node's witness builder collects from its channels: every account and slot the
/// pre-block states and the included transactions' returned states name, and the bytecode those
/// states carry for the accounts the engine loaded with their code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WitnessKeys {
    /// Every account a state names.
    pub accounts: BTreeSet<Address>,
    /// Every slot a state names, with its account.
    pub slots: BTreeSet<(Address, StorageKey)>,
    /// The bytecode the states carry, by hash: the code of every account loaded with its code,
    /// and the code the block deployed.
    pub codes: BTreeMap<B256, Bytecode>,
}

impl WitnessKeys {
    /// Adds every account and slot `state` names, and the code it carries.
    pub fn add_state(&mut self, state: &EvmState) {
        for (address, account) in state {
            self.accounts.insert(*address);
            for key in account.storage.keys() {
                self.slots.insert((*address, *key));
            }
            if let Some(code) = &account.info.code {
                if !code.is_empty() {
                    self.codes.insert(account.info.code_hash, code.clone());
                }
            }
        }
    }
}

impl WitnessRecord {
    /// The witness a node builds from its channels, resolved against `chain`, the state the block
    /// ran on: every account `keys` names as the chain holds it, an absent one recorded as absent,
    /// with the code the chain holds for it beside the code the states carried; every slot `keys`
    /// names of an account the chain holds, zeroes included — the slots of an absent account are
    /// zero without a read, so none is recorded; and the block hashes, the buckets and the
    /// included transactions' oracle reads as given. Hints are not in it: a validator has no
    /// service to hand them to.
    ///
    /// # Errors
    ///
    /// The chain's, when a read fails.
    pub fn from_channels<DB: Database>(
        chain: &mut DB,
        keys: &WitnessKeys,
        block_hashes: BTreeMap<u64, B256>,
        buckets: BTreeMap<BucketId, Result<u64, String>>,
        oracle_reads: Vec<OracleRead>,
    ) -> Result<Self, DB::Error> {
        let mut record = Self {
            codes: keys.codes.clone(),
            block_hashes,
            buckets,
            oracle_reads,
            ..Self::default()
        };
        for address in &keys.accounts {
            let info = chain.basic(*address)?.map(|mut info| {
                if let Some(code) = info.code.take() {
                    if !code.is_empty() {
                        record.codes.insert(info.code_hash, code);
                    }
                }
                info
            });
            record.accounts.insert(*address, info);
        }
        for (address, key) in &keys.slots {
            if record.accounts.get(address).is_some_and(Option::is_some) {
                let value = chain.storage(*address, *key)?;
                record.storage.insert((*address, *key), value);
            }
        }
        Ok(record)
    }

    /// Whether every key `other` holds is in this record with the same answer: what a replay
    /// read is what the original run read.
    pub fn covers(&self, other: &Self) -> bool {
        self.missing_from(other).is_empty()
    }

    /// The keys `other` holds that this record does not hold with the same answer, described.
    pub fn missing_from(&self, other: &Self) -> Vec<String> {
        let mut missing = Vec::new();
        for (address, info) in &other.accounts {
            if self.accounts.get(address) != Some(info) {
                missing.push(format!("account {address}"));
            }
        }
        for ((address, key), value) in &other.storage {
            if self.storage.get(&(*address, *key)) != Some(value) {
                missing.push(format!("slot {key} of {address}"));
            }
        }
        for (hash, code) in &other.codes {
            if self.codes.get(hash) != Some(code) {
                missing.push(format!("code {hash}"));
            }
        }
        for (number, hash) in &other.block_hashes {
            if self.block_hashes.get(number) != Some(hash) {
                missing.push(format!("hash of block {number}"));
            }
        }
        for (bucket, answer) in &other.buckets {
            if self.buckets.get(bucket) != Some(answer) {
                missing.push(format!("bucket {bucket}"));
            }
        }
        missing
    }
}

/// A record several recorders write into.
pub type SharedWitnessRecord = Rc<RefCell<WitnessRecord>>;

/// A witness that does not hold what execution asked for.
#[derive(Clone, Debug, PartialEq, Eq, derive_more::Display, derive_more::Error)]
pub enum WitnessError {
    /// No account was recorded at this address.
    #[display("the witness holds no account {_0}")]
    Account(#[error(not(source))] Address),
    /// No slot was recorded at this key of this address.
    #[display("the witness holds no slot {_1} of {_0}")]
    Slot(Address, StorageKey),
    /// No code was recorded under this hash.
    #[display("the witness holds no code {_0}")]
    Code(#[error(not(source))] B256),
    /// No hash was recorded for this block.
    #[display("the witness holds no hash of block {_0}")]
    BlockHash(#[error(not(source))] u64),
    /// No capacity was recorded for this bucket.
    #[display("the witness holds no bucket {_0}")]
    Bucket(#[error(not(source))] BucketId),
    /// The recorded lookup of this bucket failed, with this message.
    #[display("the witness recorded bucket {_0} failing: {_1}")]
    BucketFailed(BucketId, String),
}

impl DBErrorMarker for WitnessError {}

/// A database that records every read it serves, answering from the database it wraps.
///
/// Code is served lazily: `basic` answers without the bytecode and keeps the bytecode the wrapped
/// database handed it with the account, and `code_by_hash` serves it from there, or from the
/// wrapped database for a hash it never saw. Every code the engine loads is then a recorded
/// `code_by_hash` read, as it is on a node whose database serves code by hash.
#[derive(Debug)]
pub struct RecordingDatabase<DB> {
    inner: DB,
    record: SharedWitnessRecord,
    /// The bytecode the wrapped database handed out with its accounts, by hash.
    codes: HashMap<B256, Bytecode>,
}

impl<DB> RecordingDatabase<DB> {
    /// Wraps `inner`, recording into `record`.
    pub fn new(inner: DB, record: SharedWitnessRecord) -> Self {
        Self { inner, record, codes: HashMap::default() }
    }

    /// The wrapped database.
    pub const fn inner(&self) -> &DB {
        &self.inner
    }

    /// The record this database writes into.
    pub const fn record(&self) -> &SharedWitnessRecord {
        &self.record
    }
}

impl<DB: Database> Database for RecordingDatabase<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let info = self.inner.basic(address)?.map(|mut info| {
            if let Some(code) = info.code.take() {
                self.codes.insert(info.code_hash, code);
            }
            info
        });
        self.record.borrow_mut().accounts.insert(address, info.clone());
        Ok(info)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let code = match self.codes.get(&code_hash) {
            Some(code) => code.clone(),
            None => self.inner.code_by_hash(code_hash)?,
        };
        self.record.borrow_mut().codes.insert(code_hash, code.clone());
        Ok(code)
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        let value = self.inner.storage(address, index)?;
        self.record.borrow_mut().storage.insert((address, index), value);
        Ok(value)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        let hash = self.inner.block_hash(number)?;
        self.record.borrow_mut().block_hashes.insert(number, hash);
        Ok(hash)
    }
}

impl<DB: DatabaseCommit> DatabaseCommit for RecordingDatabase<DB> {
    fn commit(&mut self, changes: EvmState) {
        self.inner.commit(changes);
    }
}

/// A database that serves exactly a recorded witness and refuses every other key.
///
/// It is what a validator's database is, made strict: an account the record holds as absent is
/// answered `None` and a slot it holds as zero is answered zero, as recorded, and a key the record
/// does not hold at all is an error rather than an answer nobody recorded.
#[derive(Clone, Debug)]
pub struct StrictDatabase {
    record: WitnessRecord,
}

impl StrictDatabase {
    /// A database serving `record`.
    pub const fn new(record: WitnessRecord) -> Self {
        Self { record }
    }
}

impl Database for StrictDatabase {
    type Error = WitnessError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.record.accounts.get(&address).cloned().ok_or(WitnessError::Account(address))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.record.codes.get(&code_hash).cloned().ok_or(WitnessError::Code(code_hash))
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.record
            .storage
            .get(&(address, index))
            .copied()
            .ok_or(WitnessError::Slot(address, index))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.record.block_hashes.get(&number).copied().ok_or(WitnessError::BlockHash(number))
    }
}

/// A SALT environment that records every capacity it is asked for, answering from the
/// environment it wraps.
#[derive(Clone, Debug)]
pub struct RecordingSaltEnv<S> {
    inner: S,
    record: SharedWitnessRecord,
}

impl<S: SaltEnv> SaltEnv for RecordingSaltEnv<S> {
    type Error = S::Error;

    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        let answer = self.inner.get_bucket_capacity(bucket_id);
        let recorded = answer.as_ref().map(|capacity| *capacity).map_err(ToString::to_string);
        self.record.borrow_mut().buckets.insert(bucket_id, recorded);
        answer
    }

    fn bucket_id_for_account(account: Address) -> BucketId {
        S::bucket_id_for_account(account)
    }

    fn bucket_id_for_slot(address: Address, key: U256) -> BucketId {
        S::bucket_id_for_slot(address, key)
    }
}

/// An oracle environment that records every read and hint, answering from the environment it
/// wraps.
#[derive(Clone, Debug)]
pub struct RecordingOracleEnv<O> {
    inner: O,
    record: SharedWitnessRecord,
}

impl<O: OracleEnv> OracleEnv for RecordingOracleEnv<O> {
    fn get_oracle_storage(&self, slot: U256) -> Option<U256> {
        let answer = self.inner.get_oracle_storage(slot);
        self.record.borrow_mut().oracle_reads.push(OracleRead { slot, answer });
        answer
    }

    fn on_hint(&self, from: Address, topic: B256, data: Bytes) {
        self.record.borrow_mut().hints.push(RecordedHint { from, topic, data: data.clone() });
        self.inner.on_hint(from, topic, data);
    }
}

/// A factory whose environments record what they answer into one record.
#[derive(Clone, Debug)]
pub struct RecordingEnvFactory<F> {
    inner: F,
    record: SharedWitnessRecord,
}

impl<F> RecordingEnvFactory<F> {
    /// Wraps `inner`, recording into `record`.
    pub const fn new(inner: F, record: SharedWitnessRecord) -> Self {
        Self { inner, record }
    }
}

impl<F: ExternalEnvFactory> ExternalEnvFactory for RecordingEnvFactory<F> {
    type EnvTypes = (
        RecordingSaltEnv<<F::EnvTypes as ExternalEnvTypes>::SaltEnv>,
        RecordingOracleEnv<<F::EnvTypes as ExternalEnvTypes>::OracleEnv>,
    );

    fn external_envs(&self, block: BlockNumber) -> ExternalEnvs<Self::EnvTypes> {
        let ExternalEnvs { salt_env, oracle_env } = self.inner.external_envs(block);
        ExternalEnvs {
            salt_env: RecordingSaltEnv { inner: salt_env, record: self.record.clone() },
            oracle_env: RecordingOracleEnv { inner: oracle_env, record: self.record.clone() },
        }
    }
}

/// A SALT environment serving exactly the recorded buckets, with the hashing of `S`, and refusing
/// every other: what a validator's environment does with the bucket proofs it was given.
#[derive(Clone, Debug)]
pub struct StrictSaltEnv<S> {
    buckets: BTreeMap<BucketId, Result<u64, String>>,
    hasher: PhantomData<S>,
}

impl<S: SaltEnv> SaltEnv for StrictSaltEnv<S> {
    type Error = WitnessError;

    fn get_bucket_capacity(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        match self.buckets.get(&bucket_id) {
            Some(Ok(capacity)) => Ok(*capacity),
            Some(Err(message)) => Err(WitnessError::BucketFailed(bucket_id, message.clone())),
            None => Err(WitnessError::Bucket(bucket_id)),
        }
    }

    fn bucket_id_for_account(account: Address) -> BucketId {
        S::bucket_id_for_account(account)
    }

    fn bucket_id_for_slot(address: Address, key: U256) -> BucketId {
        S::bucket_id_for_slot(address, key)
    }
}

/// An oracle service replaying recorded answers: the n-th read gets the n-th recorded answer when
/// it asks for the recorded slot, and `None` otherwise, which is noted as a mismatch. Hints are
/// taken and dropped, as a validator has no service to hand them to.
///
/// [`absent`](Self::absent) is the service a validator without one runs: every read is answered
/// `None`, and none is a mismatch.
#[derive(Clone, Debug)]
pub struct ReplayingOracleEnv {
    reads: Rc<Vec<OracleRead>>,
    next: Rc<Cell<usize>>,
    mismatched: Rc<Cell<bool>>,
    absent: bool,
}

impl ReplayingOracleEnv {
    /// A service replaying `reads` in order.
    pub fn new(reads: Vec<OracleRead>) -> Self {
        Self {
            reads: Rc::new(reads),
            next: Rc::new(Cell::new(0)),
            mismatched: Rc::new(Cell::new(false)),
            absent: false,
        }
    }

    /// The service a validator without an oracle service runs: it answers nothing.
    pub fn absent() -> Self {
        Self { absent: true, ..Self::new(Vec::new()) }
    }

    /// Whether every recorded read was replayed, in order, and none was answered out of order or
    /// past the record. Always true for an absent service.
    pub fn replayed_exactly(&self) -> bool {
        self.absent || (!self.mismatched.get() && self.next.get() == self.reads.len())
    }
}

impl OracleEnv for ReplayingOracleEnv {
    fn get_oracle_storage(&self, slot: U256) -> Option<U256> {
        if self.absent {
            return None;
        }
        let index = self.next.get();
        self.next.set(index + 1);
        match self.reads.get(index) {
            Some(read) if read.slot == slot => read.answer,
            _ => {
                self.mismatched.set(true);
                None
            }
        }
    }
}

/// A factory whose environments serve exactly a recorded witness: the buckets it holds and the
/// oracle answers it holds, in order, or an absent oracle service.
#[derive(Clone, Debug)]
pub struct StrictEnvFactory<S> {
    salt: StrictSaltEnv<S>,
    oracle: ReplayingOracleEnv,
}

impl<S: SaltEnv> StrictEnvFactory<S> {
    /// Serves `record`'s buckets and replays its oracle answers.
    pub fn replaying(record: &WitnessRecord) -> Self {
        Self {
            salt: StrictSaltEnv { buckets: record.buckets.clone(), hasher: PhantomData },
            oracle: ReplayingOracleEnv::new(record.oracle_reads.clone()),
        }
    }

    /// Serves `record`'s buckets with no oracle service, as today's validator runs.
    pub fn without_oracle(record: &WitnessRecord) -> Self {
        Self {
            salt: StrictSaltEnv { buckets: record.buckets.clone(), hasher: PhantomData },
            oracle: ReplayingOracleEnv::absent(),
        }
    }

    /// The oracle service the environments share, to ask whether it replayed exactly.
    pub const fn oracle(&self) -> &ReplayingOracleEnv {
        &self.oracle
    }
}

impl<S: SaltEnv + Clone> ExternalEnvFactory for StrictEnvFactory<S> {
    type EnvTypes = (StrictSaltEnv<S>, ReplayingOracleEnv);

    fn external_envs(&self, _block: BlockNumber) -> ExternalEnvs<Self::EnvTypes> {
        ExternalEnvs { salt_env: self.salt.clone(), oracle_env: self.oracle.clone() }
    }
}
