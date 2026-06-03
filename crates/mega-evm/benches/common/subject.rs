//! Benchmark subjects: one per EVM stack being compared.
//!
//! A [`Subject`] turns a backend-agnostic [`Workload`] into a concrete run on
//! its own stack — building the database, constructing the EVM, executing every
//! transaction on one reused instance, and asserting success (unless the
//! workload opts out). Each subject absorbs its own quirks: every vanilla
//! baseline is pinned to the Cancun hardfork so its row differs from the others
//! only by crate and version (not by fork), the op rows additionally zero the
//! operator fee, and the mega rows wrap each tx in a `MegaTransaction` envelope.
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

use super::workload::{Account, Workload};

/// One row of a criterion group — a specific EVM stack at a specific config.
pub trait Subject {
    /// Row name as it appears in the criterion group (e.g. `revm_pinned`).
    fn name(&self) -> &str;
    /// Build a fresh DB from the workload's accounts, construct the EVM, run
    /// every tx on one reused instance, and (unless the workload opts out)
    /// assert each succeeds.
    fn run(&self, workload: &Workload);
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

/// Translate a [`TxSpec`](super::workload::TxSpec) into a pinned-revm `TxEnv`,
/// used by the `revm_pinned`, `op_revm_pinned`, and `mega_*` subjects (mega-evm
/// re-exports the same pinned revm crate).
fn pinned_tx_env(tx: &super::workload::TxSpec) -> TxEnv {
    TxEnvBuilder::new()
        .caller(tx.caller)
        .call(tx.target)
        .gas_limit(tx.gas_limit)
        .value(tx.value)
        .data(tx.data.clone())
        .build_fill()
}

/// Translate a [`TxSpec`](super::workload::TxSpec) into a latest-revm `TxEnv`.
fn latest_tx_env(tx: &super::workload::TxSpec) -> TxEnvLatest {
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
        // Operator fee scalar/constant zeroed so the mega rows stay comparable
        // against the vanilla baselines.
        let mut context = MegaContext::new(build_pinned_db(&workload.accounts), self.spec);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = MegaEvm::<_, NoOpInspector, EmptyExternalEnv>::new(context);
        for tx in &workload.txs {
            // Wrap into a `MegaTransaction` with an empty envelope, matching
            // what the production tx-pool would attach.
            let mut mega_tx = MegaTransaction::new(pinned_tx_env(tx));
            mega_tx.enveloped_tx = Some(Bytes::new());
            let r = evm.transact(mega_tx).expect("mega transact");
            assert_result(workload, self.name, r.result.is_success(), &r.result);
            black_box(&r);
        }
    }
}

//
// ============================================================================
// Baseline subjects.
// ============================================================================
//

/// Vanilla `revm` at the version mega-evm currently pins.
///
/// Spec pinned to Cancun so its row sits on the same hardfork as the other
/// baselines (see [`RevmLatest`]); the pinned mainnet default is otherwise a
/// later fork.
pub struct RevmPinned;

impl Subject for RevmPinned {
    fn name(&self) -> &str {
        "revm_pinned"
    }

    fn run(&self, workload: &Workload) {
        let mut evm = ContextPinned::mainnet()
            .modify_cfg_chained(|cfg| cfg.spec = SpecIdPinned::CANCUN)
            .with_db(build_pinned_db(&workload.accounts))
            .build_mainnet();
        for tx in &workload.txs {
            let r = evm.transact(pinned_tx_env(tx)).expect("revm_pinned transact");
            assert_result(workload, "revm_pinned", r.result.is_success(), &r.result);
            black_box(&r);
        }
    }
}

/// Vanilla `revm` at the latest crates.io release.
///
/// Spec pinned to Cancun for two reasons: it keeps this row on the same
/// hardfork as the other baselines, and it avoids the EIP-7825
/// `tx_gas_limit_cap` (2^24) that `MainContext::mainnet()` would otherwise
/// inherit from its default Osaka spec and that the multi-gigagas `gas_limit`
/// workloads would trip.
pub struct RevmLatest;

impl Subject for RevmLatest {
    fn name(&self) -> &str {
        "revm_latest"
    }

    fn run(&self, workload: &Workload) {
        let mut evm = ContextLatest::mainnet()
            .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(SpecIdLatest::CANCUN))
            .with_db(build_latest_db(&workload.accounts))
            .build_mainnet();
        for tx in &workload.txs {
            let r = evm.transact(latest_tx_env(tx)).expect("revm_latest transact");
            assert_result(workload, "revm_latest", r.result.is_success(), &r.result);
            black_box(&r);
        }
    }
}

/// `op-revm` at the version mega-evm currently pins, operator fee = 0.
///
/// Pinned to Holocene (eth Cancun) to match the other baselines: `DefaultOp::op()`
/// hard-codes `OpSpecId::BEDROCK` (eth Merge) regardless of the enum default, so
/// without this the op row would sit on a different hardfork than the revm rows.
pub struct OpRevmPinned;

impl Subject for OpRevmPinned {
    fn name(&self) -> &str {
        "op_revm_pinned"
    }

    fn run(&self, workload: &Workload) {
        let mut ctx =
            <OpContextPinned<EmptyDBPinned>>::op().with_db(build_pinned_db(&workload.accounts));
        ctx.modify_cfg(|cfg| cfg.spec = OpSpecIdPinned::HOLOCENE);
        ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = ctx.build_op();
        for tx in &workload.txs {
            let mut op_tx = OpTransactionPinned::new(pinned_tx_env(tx));
            op_tx.enveloped_tx = Some(Bytes::new());
            let r = evm.transact(op_tx).expect("op_revm_pinned transact");
            assert_result(workload, "op_revm_pinned", r.result.is_success(), &r.result);
            black_box(&r);
        }
    }
}

/// `op-revm` at the latest crates.io release, operator fee = 0.
///
/// Pinned to Holocene (eth Cancun) to match the other baselines: `DefaultOp::op()`
/// hard-codes `OpSpecId::BEDROCK` (eth Merge) regardless of the enum default, so
/// without this the op row would sit on a different hardfork than the revm rows.
/// Holocene (eth Cancun) also predates the EIP-7825 `tx_gas_limit_cap`, so the
/// multi-gigagas `gas_limit` workloads pass.
pub struct OpRevmLatest;

impl Subject for OpRevmLatest {
    fn name(&self) -> &str {
        "op_revm_latest"
    }

    fn run(&self, workload: &Workload) {
        let mut ctx =
            <OpContextLatest<EmptyDBLatest>>::op().with_db(build_latest_db(&workload.accounts));
        ctx.modify_cfg(|cfg| cfg.spec = OpSpecIdLatest::HOLOCENE);
        ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = ctx.build_op();
        for tx in &workload.txs {
            let mut op_tx = OpTransactionLatest::new(latest_tx_env(tx));
            op_tx.enveloped_tx = Some(Bytes::new());
            let r = evm.transact(op_tx).expect("op_revm_latest transact");
            assert_result(workload, "op_revm_latest", r.result.is_success(), &r.result);
            black_box(&r);
        }
    }
}

/// Assert tx success unless the workload opted out of the check.
///
/// `expect()` on `transact()` only catches internal errors; a transaction that
/// halts (out-of-gas, a per-tx resource limit exceeded under a mega spec, …)
/// returns `Ok` with `is_success() == false`. Asserting here keeps the bench
/// from silently measuring partially-halted runs that are incomparable across
/// rows. `is_success()` is read at the call site because the pinned and latest
/// stacks return distinct `ExecutionResult` types from separate crate versions;
/// `result` is taken as `impl Debug` only for the failure message.
fn assert_result(workload: &Workload, row: &str, success: bool, result: &impl core::fmt::Debug) {
    if workload.assert_success {
        assert!(success, "{row} should succeed: {result:?}");
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
