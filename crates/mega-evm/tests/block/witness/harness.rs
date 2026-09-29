//! The witness-replay harness: a block is executed once on a database and environments that
//! record every read, then again on a strict database and environments that serve exactly what
//! was recorded and refuse everything else; the two runs must agree on everything the block
//! produced, and the replay's reads must be among the recorded ones.

use std::{collections::BTreeMap, fmt::Debug};

use alloy_consensus::{transaction::Recovered, Signed, TxEip7702, TxLegacy};
use alloy_eips::eip7702::{Authorization, SignedAuthorization};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::{OpAlloyReceiptBuilder, OpReceiptBuilder};
use alloy_primitives::{Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{
        MemoryDatabase, RecordingDatabase, RecordingEnvFactory, SharedWitnessRecord,
        StrictDatabase, StrictEnvFactory, WitnessRecord,
    },
    BlockGasCounters, BucketId, ExternalEnvFactory, LimitCheck, LimitUsage, MegaBlockExecutionCtx,
    MegaBlockExecutor, MegaEvmFactory, MegaGasUsage, MegaHaltReason, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope, PreBlockStateSource, ProtocolLimits, TestExternalEnvs,
};
use op_alloy_consensus::TxDeposit;
use revm::{
    context::result::ExecutionResult,
    database::{states::bundle_state::BundleRetention, BundleState, CacheState, State},
    state::EvmState,
    Database,
};

use crate::common::{self, record_pre_block, CALLER, CHAIN_ID};

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
/// transactions.
pub(crate) struct Case {
    pub name: String,
    pub db: MemoryDatabase,
    pub envs: Envs,
    pub spec: MegaHardforkConfig,
    pub ctx: MegaBlockExecutionCtx,
    pub env: EvmEnv<MegaSpecId>,
    pub txs: Vec<Tx>,
}

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
        }
    }

    /// Adds a transaction.
    pub(crate) fn tx(mut self, tx: Tx) -> Self {
        self.txs.push(tx);
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

    /// Runs the block, recording every read.
    pub(crate) fn record(&self) -> Run {
        let record = SharedWitnessRecord::default();
        let state = State::builder()
            .with_database(RecordingDatabase::new(self.db.clone(), record.clone()))
            .with_bundle_update()
            .build();
        let factory = RecordingEnvFactory::new(self.envs.clone(), record.clone());
        let mut run = self.drive(state, factory);
        run.record = record.take();
        run
    }

    /// Runs the block again on exactly what `record` holds, against `oracle`.
    pub(crate) fn replay(&self, record: &WitnessRecord, oracle: Oracle) -> Run {
        let reads = SharedWitnessRecord::default();
        let state = State::builder()
            .with_database(RecordingDatabase::new(
                StrictDatabase::new(record.clone()),
                reads.clone(),
            ))
            .with_bundle_update()
            .build();
        let strict = match oracle {
            Oracle::Recorded => StrictEnvFactory::<Envs>::replaying(record),
            Oracle::Absent => StrictEnvFactory::<Envs>::without_oracle(record),
        };
        let factory = RecordingEnvFactory::new(strict.clone(), reads.clone());
        let mut run = self.drive(state, factory);
        run.record = reads.take();
        run.oracle_replayed_exactly = strict.oracle().replayed_exactly();
        run
    }

    /// Records the block, replays it from the record against the recorded oracle answers, and
    /// asserts the two runs agree on everything, that the replay read nothing the record does not
    /// hold, and that the executor's exports are the recorded side-channel reads.
    pub(crate) fn run(self) -> Replay {
        let recorded = self.record();
        let replayed = self.replay(&recorded.record, Oracle::Recorded);
        assert_same_run(&self.name, &recorded, &replayed);
        assert!(
            recorded.record.covers(&replayed.record),
            "{}: the replay read what the record does not hold: {:?}",
            self.name,
            recorded.record.missing_from(&replayed.record)
        );
        assert!(replayed.oracle_replayed_exactly, "{}: the oracle reads were replayed", self.name);
        assert_eq!(
            recorded.bucket_ids,
            recorded.record.buckets.keys().copied().collect::<Vec<_>>(),
            "{}: the exported buckets are the SALT lookups the block made",
            self.name
        );
        assert_eq!(
            recorded.block_hashes, recorded.record.block_hashes,
            "{}: the exported block hashes are the block-hash reads the block made",
            self.name
        );
        Replay { recorded, replayed }
    }

    /// Runs the block on `state` with the environments `factory` makes.
    fn drive<DB, F>(&self, mut state: State<DB>, factory: F) -> Run
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
        let mut txs = Vec::new();
        for tx in &self.txs {
            let outcome = match executor.run_transaction(tx) {
                Ok(outcome) => outcome,
                Err(error) => {
                    txs.push(Err(error.to_string()));
                    continue;
                }
            };
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
            };
            match executor.commit_transaction_outcome(outcome) {
                Ok(_) => txs.push(Ok(snapshot)),
                Err(error) => txs.push(Err(error.to_string())),
            }
        }
        let block_hashes = executor.get_accessed_block_hashes();
        let bucket_ids = executor.get_accessed_bucket_ids();
        let (evm, result) = executor.finish_with_counters().expect("the block finishes");
        drop(evm);
        state.merge_transitions(BundleRetention::Reverts);
        let pre_block = log.lock().expect("pre-block observer").clone();
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
    let _ = record_pre_block; // the block tests' helper for the common executor type
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
}

/// What one run of a block produced, and what it read.
#[derive(Clone, Debug)]
pub(crate) struct Run {
    /// The pre-block states the observer received, in order.
    pub pre_block: Vec<(PreBlockStateSource, EvmState)>,
    /// Each transaction's outcome, or the refusal the block answered it with.
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
    /// What the run read.
    pub record: WitnessRecord,
    /// Whether the replay's oracle service answered every recorded read in order.
    pub oracle_replayed_exactly: bool,
}

impl Run {
    /// The outcome of the transaction at `index`, which the block did not refuse.
    pub(crate) fn tx(&self, index: usize) -> &TxSnapshot {
        self.txs[index]
            .as_ref()
            .unwrap_or_else(|error| panic!("transaction {index} refused: {error}"))
    }

    /// The refusal of the transaction at `index`.
    pub(crate) fn refusal(&self, index: usize) -> &str {
        match &self.txs[index] {
            Ok(_) => panic!("transaction {index} was not refused"),
            Err(error) => error,
        }
    }
}

/// A recorded run and its replay.
pub(crate) struct Replay {
    pub recorded: Run,
    pub replayed: Run,
}

/// Asserts two runs of one block produced the same block: the same pre-block states, transaction
/// outcomes, receipts, header figures, counters, state changes and exports.
pub(crate) fn assert_same_run(name: &str, a: &Run, b: &Run) {
    assert_eq!(a.pre_block, b.pre_block, "{name}: the pre-block states");
    assert_eq!(a.txs.len(), b.txs.len(), "{name}: the transaction count");
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
                assert_eq!(x, y, "{name}: transaction {index}");
            }
            (Err(x), Err(y)) => assert_eq!(x, y, "{name}: transaction {index}'s refusal"),
            (x, y) => panic!("{name}: transaction {index}: {x:?} against {y:?}"),
        }
    }
    assert_eq!(a.receipts, b.receipts, "{name}: the receipts");
    assert_eq!(a.gas_used, b.gas_used, "{name}: the block's gas used");
    assert_eq!(a.blob_gas_used, b.blob_gas_used, "{name}: the block's blob gas used");
    assert_eq!(a.gas, b.gas, "{name}: the block's ledgers");
    assert_eq!(a.usage, b.usage, "{name}: the block's usage");
    assert_eq!(a.bundle, b.bundle, "{name}: the state changes");
    assert_eq!(a.cache, b.cache, "{name}: the state cache");
    assert_eq!(a.block_hashes, b.block_hashes, "{name}: the exported block hashes");
    assert_eq!(a.bucket_ids, b.bucket_ids, "{name}: the exported buckets");
}

/// Asserts a replay differs from its recording somewhere in what the block produced.
pub(crate) fn assert_differs(name: &str, a: &Run, b: &Run) {
    let same = a.pre_block == b.pre_block &&
        a.txs == b.txs &&
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
