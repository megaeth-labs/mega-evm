//! Benchmark subjects: one per EVM stack being compared.
//!
//! A [`Subject`] turns a backend-agnostic [`Workload`] into a concrete run on
//! its own stack. The shared loop/assert/measure skeleton lives once in
//! [`run_workload`]; each subject only supplies how to *build* its EVM and how
//! to *execute* one tx, so the success assertion can never drift between stacks.
//!
//! What makes the vanilla rows comparable — the target hardfork and the
//! operator-fee zero-out — is defined once in the "Comparability baseline"
//! section below, not repeated per stack.
//!
//! `_latest` types come from a cargo `package` rename in `Cargo.toml`. Both
//! `_latest` subjects share one revm tree because the chosen `op-revm` version
//! already pulls the chosen `revm` version transitively.

use alloy_primitives::{Address, Bytes, U256};
use criterion::black_box;
use mega_evm::{
    revm::inspector::NoOpInspector, test_utils::MemoryDatabase, EmptyExternalEnv, MegaContext,
    MegaEvm, MegaSpecId, MegaTransaction,
};
use op_revm::{
    DefaultOp as _, OpBuilder as _, OpContext as OpContextPinned, OpSpecId as OpSpecIdPinned,
    OpTransaction as OpTransactionPinned,
};
use op_revm_latest::{
    DefaultOp as _, OpBuilder as _, OpContext as OpContextLatest, OpSpecId as OpSpecIdLatest,
    OpTransaction as OpTransactionLatest,
};
use revm::{
    context::{tx::TxEnvBuilder, TxEnv},
    database::EmptyDB as EmptyDBPinned,
    primitives::hardfork::SpecId as SpecIdPinned,
    Context as ContextPinned, ExecuteEvm, MainBuilder as _, MainContext as _,
};
use revm_latest::{
    bytecode::Bytecode as BytecodeLatest,
    context::{tx::TxEnvBuilder as TxEnvBuilderLatest, TxEnv as TxEnvLatest},
    database::{CacheDB as CacheDBLatest, EmptyDB as EmptyDBLatest},
    primitives::hardfork::SpecId as SpecIdLatest,
    Context as ContextLatest, ExecuteEvm as _, MainBuilder as _, MainContext as _,
};

use super::workload::{Account, TxSpec, Workload};

//
// ============================================================================
// Comparability baseline — single source of truth for what makes the vanilla
// rows comparable. Change a value here and every baseline moves together; CI's
// baseline-gap table assumes the rows all sit on one fork.
// ============================================================================
//

/// Target hardfork for the vanilla `revm` rows. Cancun keeps every baseline on
/// one fork and predates EIP-7825's `tx_gas_limit_cap` (2^24), so the
/// multi-gigagas `gas_limit` workloads are not truncated. (`revm_latest`'s
/// `MainContext::mainnet()` would otherwise default to Osaka and trip the cap.)
const REVM_FORK: SpecIdPinned = SpecIdPinned::CANCUN;
const REVM_FORK_LATEST: SpecIdLatest = SpecIdLatest::CANCUN;

/// Target hardfork for the op rows. Holocene maps to eth Cancun, matching the
/// `revm` rows above. Needed because `DefaultOp::op()` hard-codes `BEDROCK`
/// (eth Merge) regardless of the enum default — without this the op rows would
/// sit on a different fork and the op-vs-revm gap would reflect a hardfork
/// difference rather than a version one.
const OP_FORK: OpSpecIdPinned = OpSpecIdPinned::HOLOCENE;
const OP_FORK_LATEST: OpSpecIdLatest = OpSpecIdLatest::HOLOCENE;

/// Zero the operator fee so the op and mega rows are comparable to the revm
/// rows, which carry no such fee. A macro rather than a fn: the three `chain`
/// types come from distinct crates (op-revm pinned/latest, mega-evm) and share
/// no common trait — only the field names line up.
macro_rules! zero_operator_fee {
    ($chain:expr) => {{
        $chain.operator_fee_scalar = Some(U256::ZERO);
        $chain.operator_fee_constant = Some(U256::ZERO);
    }};
}

//
// ============================================================================
// Subject trait + shared run skeleton.
// ============================================================================
//

/// One row of a criterion group — a specific EVM stack at a specific config.
pub trait Subject {
    /// Row name as it appears in the criterion group (e.g. `revm_pinned`).
    fn name(&self) -> &str;
    /// Build a fresh DB + EVM from the workload, run every tx on that one
    /// reused instance, and (unless the workload opts out) assert each
    /// succeeds. Implementations delegate to [`run_workload`].
    fn run(&self, workload: &Workload);
}

/// The one place the per-tx loop, success assertion, and `black_box` live.
///
/// `build` constructs the (stack-specific) EVM once; `exec` runs a single tx
/// and returns its `is_success()`. Keeping the skeleton here means the success
/// check can never drift or be forgotten on one stack — the bug that the old
/// per-stack `run` bodies were prone to. The generic `E` is inferred from
/// `build`, so no stack has to spell out its verbose revm `Evm<…>` type.
///
/// On failure the panic names the row and tx index; the concrete result is not
/// surfaced because `exec` has already abstracted away the stack's distinct
/// `ExecutionResult` type (it `black_box`es the result for the optimizer).
fn run_workload<E>(
    name: &str,
    workload: &Workload,
    build: impl FnOnce() -> E,
    exec: impl Fn(&mut E, &TxSpec) -> bool,
) {
    let mut evm = build();
    for (i, tx) in workload.txs.iter().enumerate() {
        let success = exec(&mut evm, tx);
        if workload.assert_success {
            assert!(success, "{name} tx #{i} should succeed");
        }
    }
}

//
// ============================================================================
// Database builders — written once, fed to every subject of each stack.
// ============================================================================
//

/// Seed a pinned-revm `MemoryDatabase` from the workload accounts. Shared by
/// the `*_pinned` baselines and every `mega_*` subject.
fn build_pinned_db(accounts: &[Account]) -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    for account in accounts {
        db = db.account_balance(account.address, account.balance);
        if let Some(code) = &account.code {
            db = db.account_code(account.address, code.clone());
        }
        for (slot, value) in &account.storage {
            db = db.account_storage(account.address, *slot, *value);
        }
    }
    db
}

/// Seed a latest-revm `CacheDB` from the workload accounts. Shared by both
/// `*_latest` baselines.
fn build_latest_db(accounts: &[Account]) -> CacheDBLatest<EmptyDBLatest> {
    let mut builder = LatestDbBuilder::new();
    for account in accounts {
        builder = builder.account_balance(account.address, account.balance);
        if let Some(code) = &account.code {
            builder = builder.account_code(account.address, code.clone());
        }
        for (slot, value) in &account.storage {
            builder = builder.account_storage(account.address, *slot, *value);
        }
    }
    builder.build()
}

/// Translate a [`TxSpec`] into a pinned-revm `TxEnv`, used by the `revm_pinned`,
/// `op_revm_pinned`, and `mega_*` subjects (mega-evm re-exports the same pinned
/// revm crate).
fn pinned_tx_env(tx: &TxSpec) -> TxEnv {
    TxEnvBuilder::new()
        .caller(tx.caller)
        .call(tx.target)
        .gas_limit(tx.gas_limit)
        .value(tx.value)
        .data(tx.data.clone())
        .build_fill()
}

/// Translate a [`TxSpec`] into a latest-revm `TxEnv`.
fn latest_tx_env(tx: &TxSpec) -> TxEnvLatest {
    TxEnvBuilderLatest::new()
        .caller(tx.caller)
        .call(tx.target)
        .gas_limit(tx.gas_limit)
        .value(tx.value)
        .data(tx.data.clone())
        .build_fill()
}

//
// ============================================================================
// Mega subject.
// ============================================================================
//

/// `MegaEvm` at a single spec. One instance per benchmarked spec.
pub struct Mega {
    pub name: &'static str,
    pub spec: MegaSpecId,
}

impl Subject for Mega {
    fn name(&self) -> &str {
        self.name
    }

    fn run(&self, workload: &Workload) {
        let spec = self.spec;
        run_workload(
            self.name,
            workload,
            || {
                let mut context = MegaContext::new(build_pinned_db(&workload.accounts), spec);
                context.modify_chain(|chain| zero_operator_fee!(chain));
                MegaEvm::<_, NoOpInspector, EmptyExternalEnv>::new(context)
            },
            |evm, tx| {
                // Wrap into a `MegaTransaction` with an empty envelope, matching
                // what the production tx-pool would attach.
                let mut mega_tx = MegaTransaction::new(pinned_tx_env(tx));
                mega_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(mega_tx).expect("mega transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}

//
// ============================================================================
// Baseline subjects. Fork pins and the operator-fee zero-out come from the
// Comparability baseline section above.
// ============================================================================
//

/// Vanilla `revm` at the version mega-evm currently pins (fork: [`REVM_FORK`]).
pub struct RevmPinned;

impl Subject for RevmPinned {
    fn name(&self) -> &str {
        "revm_pinned"
    }

    fn run(&self, workload: &Workload) {
        run_workload(
            self.name(),
            workload,
            || {
                ContextPinned::mainnet()
                    .modify_cfg_chained(|cfg| cfg.spec = REVM_FORK)
                    .with_db(build_pinned_db(&workload.accounts))
                    .build_mainnet()
            },
            |evm, tx| {
                let r = evm.transact(pinned_tx_env(tx)).expect("revm_pinned transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}

/// Vanilla `revm` at the latest crates.io release (fork: [`REVM_FORK_LATEST`]).
pub struct RevmLatest;

impl Subject for RevmLatest {
    fn name(&self) -> &str {
        "revm_latest"
    }

    fn run(&self, workload: &Workload) {
        run_workload(
            self.name(),
            workload,
            || {
                ContextLatest::mainnet()
                    .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(REVM_FORK_LATEST))
                    .with_db(build_latest_db(&workload.accounts))
                    .build_mainnet()
            },
            |evm, tx| {
                let r = evm.transact(latest_tx_env(tx)).expect("revm_latest transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}

/// `op-revm` at the version mega-evm currently pins (fork: [`OP_FORK`],
/// operator fee = 0).
pub struct OpRevmPinned;

impl Subject for OpRevmPinned {
    fn name(&self) -> &str {
        "op_revm_pinned"
    }

    fn run(&self, workload: &Workload) {
        run_workload(
            self.name(),
            workload,
            || {
                let mut ctx = <OpContextPinned<EmptyDBPinned>>::op()
                    .with_db(build_pinned_db(&workload.accounts));
                ctx.modify_cfg(|cfg| cfg.spec = OP_FORK);
                ctx.modify_chain(|chain| zero_operator_fee!(chain));
                ctx.build_op()
            },
            |evm, tx| {
                let mut op_tx = OpTransactionPinned::new(pinned_tx_env(tx));
                op_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(op_tx).expect("op_revm_pinned transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}

/// `op-revm` at the latest crates.io release (fork: [`OP_FORK_LATEST`],
/// operator fee = 0).
pub struct OpRevmLatest;

impl Subject for OpRevmLatest {
    fn name(&self) -> &str {
        "op_revm_latest"
    }

    fn run(&self, workload: &Workload) {
        run_workload(
            self.name(),
            workload,
            || {
                let mut ctx = <OpContextLatest<EmptyDBLatest>>::op()
                    .with_db(build_latest_db(&workload.accounts));
                ctx.modify_cfg(|cfg| cfg.spec = OP_FORK_LATEST);
                ctx.modify_chain(|chain| zero_operator_fee!(chain));
                ctx.build_op()
            },
            |evm, tx| {
                let mut op_tx = OpTransactionLatest::new(latest_tx_env(tx));
                op_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(op_tx).expect("op_revm_latest transact");
                let success = r.result.is_success();
                black_box(r);
                success
            },
        );
    }
}

//
// ============================================================================
// CacheDB builder for the latest revm stack.
// ============================================================================
//

/// Fluent builder for a `CacheDB` from the latest revm stack, mirroring
/// `MemoryDatabase`'s API so [`build_latest_db`] can seed it the same way
/// [`build_pinned_db`] seeds the pinned database.
#[derive(Default)]
struct LatestDbBuilder {
    db: CacheDBLatest<EmptyDBLatest>,
}

impl LatestDbBuilder {
    fn new() -> Self {
        Self { db: CacheDBLatest::new(EmptyDBLatest::default()) }
    }

    fn account_code(mut self, address: Address, code: Bytes) -> Self {
        let bytecode = BytecodeLatest::new_legacy(code);
        let code_hash = bytecode.hash_slow();
        let entry = self.db.cache.accounts.entry(address).or_default();
        entry.info.code = Some(bytecode);
        entry.info.code_hash = code_hash;
        self
    }

    fn account_balance(mut self, address: Address, balance: U256) -> Self {
        let entry = self.db.cache.accounts.entry(address).or_default();
        entry.info.balance = balance;
        self
    }

    fn account_storage(mut self, address: Address, slot: U256, value: U256) -> Self {
        let entry = self.db.cache.accounts.entry(address).or_default();
        entry.storage.insert(slot, value);
        self
    }

    fn build(self) -> CacheDBLatest<EmptyDBLatest> {
        self.db
    }
}
