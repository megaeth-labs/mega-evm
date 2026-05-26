//! Shared baseline adapters and `CacheDB` builder used by multiple bench
//! files to compare mega-evm against vanilla revm and op-revm at both the
//! pinned (currently depended) and latest crates.io versions.
//!
//! Each bench file pulls this in with:
//! ```ignore
//! #[path = "common/baseline_adapters.rs"]
//! mod common;
//! ```
//!
//! Because each criterion bench target compiles as its own binary, this
//! module gets compiled once per target. The adapter set is small enough
//! that the overhead is negligible.
//!
//! `_latest` aliases come from cargo `package` rename in `Cargo.toml`; both
//! `_latest` adapters share one revm tree (revm 38 + op-revm 20) because
//! op-revm 20 transitively depends on revm 38.

#![allow(dead_code)] // each bench target picks the subset of adapters it needs
#![allow(unreachable_pub)] // included via `#[path = "..."]`, so `pub` items appear unreachable in lint terms

use alloy_primitives::{Address, Bytes, U256};
use criterion::black_box;
use mega_evm::test_utils::MemoryDatabase;
use op_revm::{
    DefaultOp as _, OpBuilder as _, OpContext as OpContextPinned,
    OpTransaction as OpTransactionPinned,
};
use op_revm_latest::{
    DefaultOp as _, OpBuilder as _, OpContext as OpContextLatest,
    OpTransaction as OpTransactionLatest,
};
use revm::{
    context::tx::TxEnvBuilder, database::EmptyDB as EmptyDBPinned, Context as ContextPinned,
    ExecuteEvm, MainBuilder as _, MainContext as _,
};
use revm_latest::{
    bytecode::Bytecode as BytecodeLatest,
    context::tx::TxEnvBuilder as TxEnvBuilderLatest,
    database::{CacheDB as CacheDBLatest, EmptyDB as EmptyDBLatest},
    primitives::hardfork::SpecId as SpecIdLatest,
    Context as ContextLatest, ExecuteEvm as _, MainBuilder as _, MainContext as _,
};

/// Parameters describing a single `transact()` call. Used by every adapter to
/// avoid a long positional signature and to let callers fill only what they
/// need via `..Default::default()`.
#[derive(Clone)]
pub struct CallParams {
    pub caller: Address,
    pub target: Address,
    pub gas_limit: u64,
    pub value: U256,
    pub data: Bytes,
}

impl Default for CallParams {
    fn default() -> Self {
        Self {
            caller: Address::ZERO,
            target: Address::ZERO,
            gas_limit: 30_000_000,
            value: U256::ZERO,
            data: Bytes::new(),
        }
    }
}

/// Vanilla `revm` at the version mega-evm currently pins (27.1.0).
pub fn transact_call_revm_pinned(db: MemoryDatabase, p: &CallParams) {
    let mut evm = ContextPinned::mainnet().with_db(db).build_mainnet();
    let tx = TxEnvBuilder::new()
        .caller(p.caller)
        .call(p.target)
        .gas_limit(p.gas_limit)
        .value(p.value)
        .data(p.data.clone())
        .build_fill();
    let r = evm.transact(tx).expect("revm_pinned transact");
    assert!(r.result.is_success(), "revm_pinned should succeed: {:?}", r.result);
    black_box(r);
}

/// `op-revm` at the pinned version (8.1.0), operator fee = 0.
pub fn transact_call_op_revm_pinned(db: MemoryDatabase, p: &CallParams) {
    let mut ctx = <OpContextPinned<EmptyDBPinned>>::op().with_db(db);
    ctx.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = ctx.build_op();
    let tx_env = TxEnvBuilder::new()
        .caller(p.caller)
        .call(p.target)
        .gas_limit(p.gas_limit)
        .value(p.value)
        .data(p.data.clone())
        .build_fill();
    let mut op_tx = OpTransactionPinned::new(tx_env);
    op_tx.enveloped_tx = Some(Bytes::new());
    let r = evm.transact(op_tx).expect("op_revm_pinned transact");
    assert!(r.result.is_success(), "op_revm_pinned should succeed: {:?}", r.result);
    black_box(r);
}

/// Vanilla `revm` at the latest crates.io version (revm 38).
///
/// Spec is pinned to Cancun so the workload's multi-gigagas `gas_limit` does
/// not trip the EIP-7825 `tx_gas_limit_cap` (2^24) that `MainContext::mainnet()`
/// inherits from its default Osaka spec.
pub fn transact_call_revm_latest(db: CacheDBLatest<EmptyDBLatest>, p: &CallParams) {
    let mut evm = ContextLatest::mainnet()
        .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(SpecIdLatest::CANCUN))
        .with_db(db)
        .build_mainnet();
    let tx = TxEnvBuilderLatest::new()
        .caller(p.caller)
        .call(p.target)
        .gas_limit(p.gas_limit)
        .value(p.value)
        .data(p.data.clone())
        .build_fill();
    let r = evm.transact(tx).expect("revm_latest transact");
    assert!(r.result.is_success(), "revm_latest should succeed: {:?}", r.result);
    black_box(r);
}

/// `op-revm` at the latest crates.io version (20.0.0), operator fee = 0.
pub fn transact_call_op_revm_latest(db: CacheDBLatest<EmptyDBLatest>, p: &CallParams) {
    let mut ctx = <OpContextLatest<EmptyDBLatest>>::op().with_db(db);
    ctx.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = ctx.build_op();
    let tx_env = TxEnvBuilderLatest::new()
        .caller(p.caller)
        .call(p.target)
        .gas_limit(p.gas_limit)
        .value(p.value)
        .data(p.data.clone())
        .build_fill();
    let mut op_tx = OpTransactionLatest::new(tx_env);
    op_tx.enveloped_tx = Some(Bytes::new());
    let r = evm.transact(op_tx).expect("op_revm_latest transact");
    assert!(r.result.is_success(), "op_revm_latest should succeed: {:?}", r.result);
    black_box(r);
}

/// Fluent builder for a `CacheDB` from the latest revm stack, mirroring
/// `MemoryDatabase`'s API so the same per-iteration seed code can produce
/// parallel DBs for both pinned and latest baselines.
#[derive(Default)]
pub struct LatestDbBuilder {
    db: CacheDBLatest<EmptyDBLatest>,
}

impl LatestDbBuilder {
    pub fn new() -> Self {
        Self { db: CacheDBLatest::new(EmptyDBLatest::default()) }
    }

    pub fn account_code(mut self, address: Address, code: Bytes) -> Self {
        let bytecode = BytecodeLatest::new_legacy(code);
        let code_hash = bytecode.hash_slow();
        let entry = self.db.cache.accounts.entry(address).or_default();
        entry.info.code = Some(bytecode);
        entry.info.code_hash = code_hash;
        self
    }

    pub fn account_balance(mut self, address: Address, balance: U256) -> Self {
        let entry = self.db.cache.accounts.entry(address).or_default();
        entry.info.balance = balance;
        self
    }

    pub fn account_storage(mut self, address: Address, slot: U256, value: U256) -> Self {
        let entry = self.db.cache.accounts.entry(address).or_default();
        entry.storage.insert(slot, value);
        self
    }

    pub fn build(self) -> CacheDBLatest<EmptyDBLatest> {
        self.db
    }
}

/// Register the 4 baseline rows on a criterion group, sharing the same
/// per-iteration seed inputs.
///
/// `make_pinned_db` feeds both `*_pinned` adapters (they share revm 27's
/// `Database` trait); `make_latest_db` feeds both `*_latest` adapters.
pub fn add_baseline_rows<FP, FL>(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    params: &CallParams,
    make_pinned_db: &FP,
    make_latest_db: &FL,
) where
    FP: Fn() -> MemoryDatabase,
    FL: Fn() -> CacheDBLatest<EmptyDBLatest>,
{
    add_baseline_rows_suffixed(group, "", params, make_pinned_db, make_latest_db);
}

/// Same as [`add_baseline_rows`] but appends a `/variant` suffix to each row
/// name. Used by benches that organise rows as `<spec_or_baseline>/<variant>`
/// (e.g. `revm_pinned/log0_32b`) so all 7 implementations share a single
/// variant axis.
pub fn add_baseline_rows_suffixed<FP, FL>(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    variant: &str,
    params: &CallParams,
    make_pinned_db: &FP,
    make_latest_db: &FL,
) where
    FP: Fn() -> MemoryDatabase,
    FL: Fn() -> CacheDBLatest<EmptyDBLatest>,
{
    let with = |base: &str| -> String {
        if variant.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{variant}")
        }
    };
    group.bench_function(with("revm_pinned"), |b| {
        b.iter(|| transact_call_revm_pinned(make_pinned_db(), params))
    });
    group.bench_function(with("revm_latest"), |b| {
        b.iter(|| transact_call_revm_latest(make_latest_db(), params))
    });
    group.bench_function(with("op_revm_pinned"), |b| {
        b.iter(|| transact_call_op_revm_pinned(make_pinned_db(), params))
    });
    group.bench_function(with("op_revm_latest"), |b| {
        b.iter(|| transact_call_op_revm_latest(make_latest_db(), params))
    });
}
