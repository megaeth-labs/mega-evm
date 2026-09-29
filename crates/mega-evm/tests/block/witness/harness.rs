//! The witness-replay harness: a block is executed once on a database and environments that
//! record every read, then twice more on a strict database and environments that serve exactly a
//! witness and refuse everything else — once on the record of every database read, once on the
//! witness a node builds from its channels — and every replay must produce the block the
//! recording produced.
//!
//! The channel witness is the check a validator's witness must pass: its keys are what the
//! pre-block states and the included transactions' returned states name, its values the chain's,
//! its block hashes and buckets the engine's exports, and its oracle answers the included
//! transactions' own records. The database-level replay is a different check: that the block
//! reads nothing outside its database and environments and computes the same block twice. Both
//! replays run the transactions the recorded block included, and only those: a candidate the
//! builder executed and dropped, or a transaction the block refused, is no validator's to run.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
};

use alloy_consensus::{transaction::Recovered, Signed, TxEip7702, TxLegacy};
use alloy_eips::eip7702::{Authorization, SignedAuthorization};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::{OpAlloyReceiptBuilder, OpReceiptBuilder};
use alloy_primitives::{Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{
        MemoryDatabase, RecordingDatabase, RecordingEnvFactory, SharedWitnessRecord,
        StrictDatabase, StrictEnvFactory, WitnessKeys, WitnessRecord,
    },
    BlockGasCounters, BucketId, ExternalEnvFactory, LimitCheck, LimitUsage, MegaBlockExecutionCtx,
    MegaBlockExecutor, MegaEvmFactory, MegaGasUsage, MegaHaltReason, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope, OracleRead, PreBlockStateSource, ProtocolLimits, SaltEnv,
    TestExternalEnvs,
};
use op_alloy_consensus::TxDeposit;
use revm::{
    context::result::ExecutionResult,
    database::{states::bundle_state::BundleRetention, BundleState, CacheState, State},
    state::EvmState,
    Database,
};

use crate::common::{self, CALLER, CHAIN_ID};

/// A transaction of a block the harness runs.
pub(crate) type Tx = Recovered<MegaTxEnvelope>;

/// The receipt the executor builds.
pub(crate) type Receipt = <OpAlloyReceiptBuilder as OpReceiptBuilder>::Receipt;

/// The environments a case's block runs against: configurable buckets, oracle answers and hint
/// recording, and buckets whose lookup fails.
pub(crate) type Envs = TestExternalEnvs<String>;

/// Which oracle service a replay runs against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Oracle {
    /// The recorded answers, replayed in order: what a validator given the answers would run.
    Recorded,
    /// No service at all: what a validator runs today. Every read is answered from the chain.
    Absent,
}

/// A block to run: its pre-state, environments, schedule, context, block environment and
/// candidate transactions, some of which the builder executes and drops.
pub(crate) struct Case {
    pub name: String,
    pub db: MemoryDatabase,
    pub envs: Envs,
    pub spec: MegaHardforkConfig,
    pub ctx: MegaBlockExecutionCtx,
    pub env: EvmEnv<MegaSpecId>,
    pub txs: Vec<Tx>,
    /// The candidates the builder executes and then does not commit.
    pub dropped: BTreeSet<usize>,
}

/// Why a candidate is not in the recorded block.
const DROPPED: &str = "executed and dropped by the builder";

/// Why a replay did not run a candidate.
const NOT_INCLUDED: &str = "not in the recorded block";

impl Case {
    /// A block over `db` on the tests' chain, with no building policy and no transactions.
    pub(crate) fn new(name: &str, db: MemoryDatabase) -> Self {
        Self {
            name: name.into(),
            db,
            envs: Envs::new(),
            spec: common::chain_spec(),
            ctx: common::unlimited_ctx(),
            env: common::evm_env(),
            txs: Vec::new(),
            dropped: BTreeSet::new(),
        }
    }

    /// Adds a transaction.
    pub(crate) fn tx(mut self, tx: Tx) -> Self {
        self.txs.push(tx);
        self
    }

    /// Has the builder execute the candidate at `index` and drop it: its outcome is not
    /// committed, and it is not in the block.
    pub(crate) fn dropped(mut self, index: usize) -> Self {
        self.dropped.insert(index);
        self
    }

    /// Runs against `envs`.
    pub(crate) fn envs(mut self, envs: Envs) -> Self {
        self.envs = envs;
        self
    }

    /// Holds the block to `limits`.
    pub(crate) fn limits(mut self, limits: ProtocolLimits) -> Self {
        self.spec = common::chain_spec_with(limits);
        self
    }

    /// Runs under `ctx`.
    pub(crate) fn ctx(mut self, ctx: MegaBlockExecutionCtx) -> Self {
        self.ctx = ctx;
        self
    }

    /// Runs in the block environment `env`.
    pub(crate) fn env(mut self, env: EvmEnv<MegaSpecId>) -> Self {
        self.env = env;
        self
    }

    /// Runs the block, recording every read: every candidate is executed, and the ones the
    /// builder drops are not committed.
    pub(crate) fn record(&self) -> Run {
        let record = SharedWitnessRecord::default();
        let state = State::builder()
            .with_database(RecordingDatabase::new(self.db.clone(), record.clone()))
            .with_bundle_update()
            .build();
        let factory = RecordingEnvFactory::new(self.envs.clone(), record.clone());
        let mut run = self.drive(state, factory, |_| true);
        run.record = record.take();
        run
    }

    /// Runs the transactions `included` names again on exactly what `witness` holds, against
    /// `oracle`: the recorded answers are the witness's, in order.
    pub(crate) fn replay(&self, witness: &WitnessRecord, included: &[bool], oracle: Oracle) -> Run {
        let reads = SharedWitnessRecord::default();
        let state = State::builder()
            .with_database(RecordingDatabase::new(
                StrictDatabase::new(witness.clone()),
                reads.clone(),
            ))
            .with_bundle_update()
            .build();
        let strict = match oracle {
            Oracle::Recorded => StrictEnvFactory::<Envs>::replaying(witness),
            Oracle::Absent => StrictEnvFactory::<Envs>::without_oracle(witness),
        };
        let factory = RecordingEnvFactory::new(strict.clone(), reads.clone());
        let mut run = self.drive(state, factory, |index| included[index]);
        run.record = reads.take();
        run.oracle_replayed_exactly = strict.oracle().replayed_exactly();
        run
    }

    /// The witness a node builds from `recorded`'s channels: the keys its pre-block states and
    /// included transactions' states name, resolved against the case's pre-state; the exported
    /// block hashes; the exported buckets with the capacities the case's environments hold; and
    /// the included transactions' own oracle reads, in block order.
    pub(crate) fn channel_witness(&self, recorded: &Run) -> WitnessRecord {
        let buckets = recorded
            .bucket_ids
            .iter()
            .map(|id| (*id, self.envs.get_bucket_capacity(*id)))
            .collect();
        WitnessRecord::from_channels(
            &mut self.db.clone(),
            &recorded.keys,
            recorded.block_hashes.clone(),
            buckets,
            recorded.included_oracle_reads(),
        )
        .expect("the pre-state is readable")
    }

    /// Replays the transactions `recorded` included on the channel witness, against `oracle`.
    pub(crate) fn replay_channels(&self, recorded: &Run, oracle: Oracle) -> Run {
        self.replay(&self.channel_witness(recorded), &recorded.included(), oracle)
    }

    /// Records the block, replays its included transactions on the record of every database
    /// read and on the channel witness, against the included transactions' recorded oracle
    /// answers, and asserts every replay produced the block the recording produced; that the
    /// database-level replay read nothing the record does not hold; that the engine's oracle
    /// records are the service's own view of the block; and that the executor's exports are the
    /// recorded side-channel reads.
    pub(crate) fn run(self) -> Replay {
        let recorded = self.record();
        let included = recorded.included();

        let mut record = recorded.record.clone();
        record.oracle_reads = recorded.included_oracle_reads();
        let replayed = self.replay(&record, &included, Oracle::Recorded);
        assert_same_run(&self.name, &recorded, &replayed);
        assert!(
            recorded.record.covers(&replayed.record),
            "{}: the replay read what the record does not hold: {:?}",
            self.name,
            recorded.record.missing_from(&replayed.record)
        );
        assert!(replayed.oracle_replayed_exactly, "{}: the oracle reads were replayed", self.name);

        let channel = self.replay_channels(&recorded, Oracle::Recorded);
        let name = format!("{} (channel witness)", self.name);
        assert_same_run(&name, &recorded, &channel);
        assert!(channel.oracle_replayed_exactly, "{name}: the oracle reads were replayed");

        assert_eq!(
            recorded.executed_oracle_reads, recorded.record.oracle_reads,
            "{}: the engine's oracle records are the service's view of every execution",
            self.name
        );
        assert_eq!(
            recorded.bucket_ids,
            recorded
                .record
                .buckets
                .iter()
                .filter_map(|(id, answer)| answer.is_ok().then_some(*id))
                .collect::<Vec<_>>(),
            "{}: the exported buckets are the SALT lookups the environment answered",
            self.name
        );
        assert_eq!(
            recorded.block_hashes, recorded.record.block_hashes,
            "{}: the exported block hashes are the block-hash reads the block made",
            self.name
        );
        Replay { recorded, replayed, channel }
    }

    /// Runs the block on `state` with the environments `factory` makes, executing the candidates
    /// `include` admits and committing those the builder does not drop.
    fn drive<DB, F>(&self, mut state: State<DB>, factory: F, include: impl Fn(usize) -> bool) -> Run
    where
        DB: Database<Error: core::error::Error + Send + Sync + 'static> + Debug,
        F: ExternalEnvFactory,
    {
        let evm = MegaEvmFactory::new()
            .with_external_env_factory(factory)
            .create_evm(&mut state, self.env.clone());
        let mut executor = MegaBlockExecutor::new(
            evm,
            self.ctx.clone(),
            self.spec.clone(),
            OpAlloyReceiptBuilder::default(),
        );
        let log = record_pre_block_generic(&mut executor);
        executor.apply_pre_execution_changes().expect("the block starts");
        let pre_block = log.lock().expect("pre-block observer").clone();
        let mut keys = WitnessKeys::default();
        for (_, state) in &pre_block {
            keys.add_state(state);
        }
        let mut txs = Vec::new();
        let mut executed_oracle_reads = Vec::new();
        for (index, tx) in self.txs.iter().enumerate() {
            if !include(index) {
                txs.push(Err(NOT_INCLUDED.into()));
                continue;
            }
            let outcome = match executor.run_transaction(tx) {
                Ok(outcome) => outcome,
                Err(error) => {
                    txs.push(Err(error.to_string()));
                    continue;
                }
            };
            executed_oracle_reads.extend(outcome.inner.oracle_reads.iter().copied());
            if self.dropped.contains(&index) {
                txs.push(Err(DROPPED.into()));
                continue;
            }
            let snapshot = TxSnapshot {
                tx_hash: outcome.tx_hash,
                gas_limit: outcome.gas_limit,
                tx_size: outcome.tx_size,
                da_size: outcome.da_size,
                da_footprint: outcome.da_footprint,
                is_deposit: outcome.is_deposit,
                depositor_nonce: outcome.depositor_nonce,
                result: outcome.inner.result.clone(),
                state: outcome.inner.state.clone(),
                gas: outcome.inner.gas,
                usage: outcome.inner.usage,
                limit_exceeded: outcome.inner.limit_exceeded,
                oracle_reads: outcome.inner.oracle_reads.clone(),
            };
            match executor.commit_transaction_outcome(outcome) {
                Ok(_) => {
                    keys.add_state(&snapshot.state);
                    txs.push(Ok(snapshot));
                }
                Err(error) => txs.push(Err(error.to_string())),
            }
        }
        let block_hashes = executor.get_accessed_block_hashes();
        let bucket_ids = executor.get_accessed_bucket_ids();
        let (evm, result) = executor.finish_with_counters().expect("the block finishes");
        drop(evm);
        state.merge_transitions(BundleRetention::Reverts);
        Run {
            pre_block,
            txs,
            receipts: result.inner.receipts,
            gas_used: result.inner.gas_used,
            blob_gas_used: result.inner.blob_gas_used,
            gas: result.gas,
            usage: result.usage,
            bundle: state.take_bundle(),
            cache: state.cache,
            block_hashes,
            bucket_ids,
            keys,
            executed_oracle_reads,
            record: WitnessRecord::default(),
            oracle_replayed_exactly: true,
        }
    }
}

/// Installs a recording pre-block observer on any executor and returns its log.
fn record_pre_block_generic<E, R, Spec>(
    executor: &mut MegaBlockExecutor<E, R, Spec>,
) -> common::PreBlockLog
where
    R: OpReceiptBuilder,
{
    let log = common::PreBlockLog::default();
    let captured = std::sync::Arc::clone(&log);
    executor.set_pre_block_observer(Some(Box::new(
        move |source: PreBlockStateSource, state: &EvmState| {
            captured.lock().expect("pre-block observer").push((source, state.clone()));
        },
    )));
    log
}

/// What one transaction of a run produced.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TxSnapshot {
    pub tx_hash: B256,
    pub gas_limit: u64,
    pub tx_size: u64,
    pub da_size: u64,
    pub da_footprint: u64,
    pub is_deposit: bool,
    pub depositor_nonce: Option<u64>,
    pub result: ExecutionResult<MegaHaltReason>,
    pub state: EvmState,
    pub gas: MegaGasUsage,
    pub usage: LimitUsage,
    pub limit_exceeded: Option<LimitCheck>,
    /// The reads of the Oracle's storage the transaction recorded, with the service's answers.
    pub oracle_reads: Vec<OracleRead>,
}

/// What one run of a block produced, and what it read.
#[derive(Clone, Debug)]
pub(crate) struct Run {
    /// The pre-block states the observer received, in order.
    pub pre_block: Vec<(PreBlockStateSource, EvmState)>,
    /// Each candidate's outcome when the block includes it, or why it does not: the refusal the
    /// block answered it with, that the builder dropped it, or that a replay did not run it.
    pub txs: Vec<Result<TxSnapshot, String>>,
    pub receipts: Vec<Receipt>,
    pub gas_used: u64,
    pub blob_gas_used: u64,
    pub gas: BlockGasCounters,
    pub usage: LimitUsage,
    /// The state changes the block committed, as a bundle.
    pub bundle: BundleState,
    /// The state cache after the block: everything loaded and committed.
    pub cache: CacheState,
    /// What the executor exported.
    pub block_hashes: BTreeMap<u64, B256>,
    pub bucket_ids: Vec<BucketId>,
    /// The keys the pre-block states and the included transactions' states name: a node's
    /// channel witness, before its values are resolved.
    pub keys: WitnessKeys,
    /// The oracle reads every executed candidate recorded, dropped ones included, in execution
    /// order: what the service saw.
    pub executed_oracle_reads: Vec<OracleRead>,
    /// What the run read.
    pub record: WitnessRecord,
    /// Whether the replay's oracle service answered every recorded read in order.
    pub oracle_replayed_exactly: bool,
}

impl Run {
    /// Which candidates the block included.
    pub(crate) fn included(&self) -> Vec<bool> {
        self.txs.iter().map(Result::is_ok).collect()
    }

    /// The oracle reads the included transactions recorded, in block order: what a validator is
    /// given in place of the service.
    pub(crate) fn included_oracle_reads(&self) -> Vec<OracleRead> {
        self.txs.iter().flatten().flat_map(|tx| tx.oracle_reads.iter().copied()).collect()
    }

    /// The outcomes of the included transactions, in block order.
    fn included_outcomes(&self) -> Vec<&TxSnapshot> {
        self.txs.iter().flatten().collect()
    }

    /// The outcome of the transaction at `index`, which the block did not refuse.
    pub(crate) fn tx(&self, index: usize) -> &TxSnapshot {
        self.txs[index]
            .as_ref()
            .unwrap_or_else(|error| panic!("transaction {index} refused: {error}"))
    }

    /// Why the transaction at `index` is not in the block.
    pub(crate) fn refusal(&self, index: usize) -> &str {
        match &self.txs[index] {
            Ok(_) => panic!("transaction {index} was not refused"),
            Err(error) => error,
        }
    }
}

/// A recorded run and its two replays.
pub(crate) struct Replay {
    pub recorded: Run,
    /// The replay on the record of every database read.
    pub replayed: Run,
    /// The replay on the channel witness.
    pub channel: Run,
}

/// Asserts a replay `b` produced the block the recording `a` produced: the same pre-block
/// states, the same outcome for every included transaction, and the same receipts, header
/// figures, counters, state changes and exports.
///
/// A candidate the block did not include was not run by the replay, so the two agree on which
/// those are and nothing more is compared for them. The recording's state cache and exports hold
/// what every execution loaded and asked about, dropped candidates' included, so they are equal
/// only when the block included every candidate; otherwise the replay's exports must be among
/// the recording's. The oracle reads a transaction recorded are the same reads on both runs, and
/// their answers are the service's, which a replay without a service does not have.
pub(crate) fn assert_same_run(name: &str, a: &Run, b: &Run) {
    assert_eq!(a.pre_block, b.pre_block, "{name}: the pre-block states");
    assert_eq!(a.txs.len(), b.txs.len(), "{name}: the candidate count");
    for (index, (x, y)) in a.txs.iter().zip(&b.txs).enumerate() {
        match (x, y) {
            (Ok(x), Ok(y)) => {
                assert_eq!(x.result, y.result, "{name}: transaction {index}'s result");
                assert_eq!(x.gas, y.gas, "{name}: transaction {index}'s gas ledgers");
                assert_eq!(x.usage, y.usage, "{name}: transaction {index}'s usage");
                assert_eq!(
                    x.limit_exceeded, y.limit_exceeded,
                    "{name}: transaction {index}'s stop"
                );
                assert_eq!(x.state, y.state, "{name}: transaction {index}'s state");
                let slots =
                    |tx: &TxSnapshot| tx.oracle_reads.iter().map(|r| r.slot).collect::<Vec<_>>();
                assert_eq!(slots(x), slots(y), "{name}: transaction {index}'s oracle reads");
                let (mut x, mut y) = (x.clone(), y.clone());
                x.oracle_reads.clear();
                y.oracle_reads.clear();
                assert_eq!(x, y, "{name}: transaction {index}");
            }
            (Err(_), Err(_)) => {}
            (x, y) => panic!("{name}: transaction {index}: {x:?} against {y:?}"),
        }
    }
    assert_eq!(a.receipts, b.receipts, "{name}: the receipts");
    assert_eq!(a.gas_used, b.gas_used, "{name}: the block's gas used");
    assert_eq!(a.blob_gas_used, b.blob_gas_used, "{name}: the block's blob gas used");
    assert_eq!(a.gas, b.gas, "{name}: the block's ledgers");
    assert_eq!(a.usage, b.usage, "{name}: the block's usage");
    assert_eq!(a.bundle, b.bundle, "{name}: the state changes");
    if a.included().iter().all(|included| *included) {
        assert_eq!(a.cache, b.cache, "{name}: the state cache");
        assert_eq!(a.block_hashes, b.block_hashes, "{name}: the exported block hashes");
        assert_eq!(a.bucket_ids, b.bucket_ids, "{name}: the exported buckets");
    } else {
        assert!(
            b.block_hashes.iter().all(|(number, hash)| a.block_hashes.get(number) == Some(hash)),
            "{name}: the replay exported a block hash the recording did not"
        );
        assert!(
            b.bucket_ids.iter().all(|id| a.bucket_ids.contains(id)),
            "{name}: the replay exported a bucket the recording did not"
        );
    }
}

/// Asserts a replay differs from its recording somewhere in what the block produced: an included
/// transaction's outcome, the receipts, the gas used or the state changes.
pub(crate) fn assert_differs(name: &str, a: &Run, b: &Run) {
    let same = a.pre_block == b.pre_block &&
        a.included_outcomes() == b.included_outcomes() &&
        a.receipts == b.receipts &&
        a.gas_used == b.gas_used &&
        a.bundle == b.bundle;
    assert!(!same, "{name}: the replay produced the same block");
}

/* ---------- transactions ---------- */

/// A legacy transaction from `sender` at `nonce`, to `kind`, carrying `value` and `input`.
pub(crate) fn legacy_from(
    sender: Address,
    nonce: u64,
    kind: TxKind,
    value: U256,
    input: Bytes,
    gas_limit: u64,
) -> Tx {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: kind,
        value,
        input,
    };
    let hash = B256::from(alloy_primitives::keccak256(
        [sender.as_slice(), &nonce.to_be_bytes(), &[0u8]].concat(),
    ));
    Recovered::new_unchecked(
        MegaTxEnvelope::Legacy(Signed::new_unchecked(tx, Signature::test_signature(), hash)),
        sender,
    )
}

/// A legacy call from [`CALLER`] at `nonce` to `to` carrying `input`.
pub(crate) fn call(nonce: u64, to: Address, input: Bytes, gas_limit: u64) -> Tx {
    legacy_from(CALLER, nonce, TxKind::Call(to), U256::ZERO, input, gas_limit)
}

/// A legacy call from [`CALLER`] at `nonce` to `to` carrying `value` and `input`.
pub(crate) fn call_with_value(
    nonce: u64,
    to: Address,
    value: U256,
    input: Bytes,
    gas_limit: u64,
) -> Tx {
    legacy_from(CALLER, nonce, TxKind::Call(to), value, input, gas_limit)
}

/// A creation from [`CALLER`] at `nonce` running `init_code`.
pub(crate) fn create(nonce: u64, init_code: Bytes, gas_limit: u64) -> Tx {
    legacy_from(CALLER, nonce, TxKind::Create, U256::ZERO, init_code, gas_limit)
}

/// A deposit from `from` to `to`, minting `mint` and carrying `value` and `input`.
pub(crate) fn deposit(
    from: Address,
    to: TxKind,
    mint: u128,
    value: U256,
    input: Bytes,
    gas_limit: u64,
) -> Tx {
    let deposit = TxDeposit {
        source_hash: B256::from(alloy_primitives::keccak256(
            [from.as_slice(), &mint.to_be_bytes(), input.as_ref()].concat(),
        )),
        from,
        to,
        mint,
        value,
        gas_limit,
        is_system_transaction: false,
        input,
    };
    let hash = B256::from(alloy_primitives::keccak256(deposit.source_hash));
    Recovered::new_unchecked(
        MegaTxEnvelope::Deposit(alloy_consensus::Sealed::new_unchecked(deposit, hash)),
        from,
    )
}

/// An EIP-7702 call from [`CALLER`] at `nonce` to `to` carrying `authorizations`.
pub(crate) fn eip7702(
    nonce: u64,
    to: Address,
    input: Bytes,
    authorizations: Vec<SignedAuthorization>,
    gas_limit: u64,
) -> Tx {
    let tx = TxEip7702 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit,
        max_fee_per_gas: 1_000_000,
        max_priority_fee_per_gas: 0,
        to,
        value: U256::ZERO,
        access_list: Default::default(),
        authorization_list: authorizations,
        input,
    };
    let hash = B256::from(alloy_primitives::keccak256(
        [CALLER.as_slice(), &nonce.to_be_bytes(), &[4u8]].concat(),
    ));
    Recovered::new_unchecked(
        MegaTxEnvelope::Eip7702(Signed::new_unchecked(tx, Signature::test_signature(), hash)),
        CALLER,
    )
}

/// An authorization delegating whoever a fixed signature recovers to — the authority, answered
/// beside it — to `delegate` at `nonce`, valid on any chain. The signature is Nick's Method's:
/// `r = s = 0x2222…22`, over contents nobody holds the key of.
pub(crate) fn authorization(delegate: Address, nonce: u64) -> (SignedAuthorization, Address) {
    let word = U256::from_be_bytes([0x22; 32]);
    let signed = SignedAuthorization::new_unchecked(
        Authorization { chain_id: U256::ZERO, address: delegate, nonce },
        0,
        word,
        word,
    );
    let authority = signed.recover_authority().expect("a Nick's-Method signature recovers");
    (signed, authority)
}
