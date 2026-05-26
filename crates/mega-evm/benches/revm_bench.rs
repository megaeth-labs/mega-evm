//! Benchmarks ported from upstream revm.
//!
//! These benchmarks are adapted from revm's revme bench suite to measure equivalent
//! workloads through mega-evm's execution pipeline, enabling performance comparison
//! between vanilla revm, op-revm, and mega-evm.
//!
//! Each workload runs against five layers so a single `cargo bench` produces a
//! comparable table:
//!
//! 1. `revm_pinned`     — vanilla `revm::Evm` at the version mega-evm currently pins.
//! 2. `revm_latest`     — vanilla `revm::Evm` at the latest crates.io release.
//! 3. `op_revm_pinned`  — `op_revm::OpEvm` at the version mega-evm currently pins (operator fee =
//!    0).
//! 4. `op_revm_latest`  — `op_revm::OpEvm` at the latest crates.io release (operator fee = 0).
//! 5. `mega_<spec>`     — `MegaEvm` at `EQUIVALENCE` / `MINI_REX` / `REX4`.
//!
//! Ported workloads:
//! - **snailtracer**: CPU-intensive ray tracer exercising many opcodes
//! - **analysis**: ERC20-like contract bytecode execution
//! - **subcall**: Multi-level contract call performance (1000 iterations)
//! - **`transfer_multi`**: Batch transaction execution (1000 transfers)
//!
//! New workloads must avoid touching mega-evm's per-tx resource limits
//! (compute gas / data size / KV updates / state growth) under any spec, or the
//! `mega_*` row will halt early and the baseline comparison will be skewed.
//! Likewise, both revm versions must be able to execute the workload with their
//! respective default hardforks — if the latest stack diverges (e.g. moves to
//! Prague gas schedule), the `*_latest` rows have to pin spec explicitly.

#![allow(missing_docs)]

use alloy_primitives::{address, bytes, Address, Bytes, U256};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use mega_evm::{test_utils::MemoryDatabase, MegaContext, MegaEvm, MegaSpecId, MegaTransaction};
use revm::{bytecode::opcode, context::tx::TxEnvBuilder, ExecuteEvm};

// Shared baseline adapters (revm_pinned / revm_latest / op_revm_pinned /
// op_revm_latest) live in benches/common/baseline_adapters.rs so other bench
// files can pull them in via the same `#[path = ...]` declaration.
#[path = "common/baseline_adapters.rs"]
mod common;
use common::{add_baseline_rows, CallParams, LatestDbBuilder};

// Imports below are only needed by the inline transfer_multi loop, which holds
// an EVM instance and runs 1000 transactions per iteration — too custom to fit
// into the single-shot `transact_call_*` adapters.
use op_revm::{
    DefaultOp as _, OpBuilder as _, OpContext as OpContextPinned,
    OpTransaction as OpTransactionPinned,
};
use op_revm_latest::{
    DefaultOp as _, OpBuilder as _, OpContext as OpContextLatest,
    OpTransaction as OpTransactionLatest,
};
use revm::{
    database::EmptyDB as EmptyDBPinned, Context as ContextPinned, MainBuilder as _,
    MainContext as _,
};
use revm_latest::{
    context::tx::TxEnvBuilder as TxEnvBuilderLatest, database::EmptyDB as EmptyDBLatest,
    primitives::hardfork::SpecId as SpecIdLatest, Context as ContextLatest, ExecuteEvm as _,
    MainBuilder as _, MainContext as _,
};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");

const SUBCALL_TARGET_A: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const SUBCALL_TARGET_B: Address = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

/// Specs to benchmark against.
const SPEC_IDS: &[(&str, MegaSpecId)] = &[
    ("equivalence", MegaSpecId::EQUIVALENCE),
    ("mini_rex", MegaSpecId::MINI_REX),
    ("rex4", MegaSpecId::REX4),
];

//
// ============================================================================
// Common Utility Functions
// ============================================================================
//

/// Execute a call to CONTRACT with the given spec, gas limit, and calldata.
fn transact_call(spec: MegaSpecId, db: MemoryDatabase, gas_limit: u64, data: Bytes) {
    let mut context = MegaContext::new(db, spec);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);

    let tx = TxEnvBuilder::new()
        .caller(CALLER)
        .call(CONTRACT)
        .gas_limit(gas_limit)
        .data(data)
        .build_fill();
    let mut mega_tx = MegaTransaction::new(tx);
    mega_tx.enveloped_tx = Some(Bytes::new());

    let r = evm.transact(mega_tx).expect("transaction should succeed");
    assert!(r.result.is_success(), "transaction should succeed: {:?}", r.result);
    black_box(r);
}

/// Build a latest-revm `CacheDB` for the snailtracer / analysis shape:
/// one contract account holding `code`, and one caller account holding `caller_balance`.
fn make_latest_db_call(
    contract: Address,
    code: Bytes,
    caller: Address,
    caller_balance: U256,
) -> revm_latest::database::CacheDB<EmptyDBLatest> {
    LatestDbBuilder::new()
        .account_code(contract, code)
        .account_balance(caller, caller_balance)
        .build()
}

//
// ============================================================================
// Snailtracer Benchmark
// ============================================================================
//
// A CPU-intensive ray tracer that exercises many opcodes.
// This is the standard EVM performance benchmark from upstream revm.
//

const SNAILTRACER_BYTES: &str = include_str!("data/snailtracer.hex");

fn bench_snailtracer(c: &mut Criterion) {
    let bytecode = Bytes::from(hex::decode(SNAILTRACER_BYTES).unwrap());
    let calldata = bytes!("30627b7c");
    let caller_balance = U256::from(10).pow(U256::from(18));

    let mut group = c.benchmark_group("snailtracer");
    group.sample_size(10);

    let make_pinned_db = || {
        MemoryDatabase::default()
            .account_code(CONTRACT, bytecode.clone())
            .account_balance(CALLER, caller_balance)
    };
    let make_latest_db = || make_latest_db_call(CONTRACT, bytecode.clone(), CALLER, caller_balance);

    let params = CallParams {
        caller: CALLER,
        target: CONTRACT,
        gas_limit: 1_000_000_000,
        data: calldata.clone(),
        ..Default::default()
    };
    add_baseline_rows(&mut group, &params, &make_pinned_db, &make_latest_db);

    for &(name, spec) in SPEC_IDS {
        group.bench_function(name, |b| {
            b.iter(|| {
                let db = make_pinned_db();
                transact_call(black_box(spec), db, 1_000_000_000, calldata.clone());
            })
        });
    }
    group.finish();
}

//
// ============================================================================
// Analysis Benchmark
// ============================================================================
//
// Exercises ERC20-like contract bytecode execution.
// Measures bytecode analysis and execution performance.
//

const ANALYSIS_BYTES: &str = include_str!("data/analysis.hex");

fn bench_analysis(c: &mut Criterion) {
    let bytecode = Bytes::from(hex::decode(ANALYSIS_BYTES).unwrap());
    let calldata = bytes!("8035F0CE");
    let caller_balance = U256::from(10).pow(U256::from(18));

    let mut group = c.benchmark_group("analysis");

    let make_pinned_db = || {
        MemoryDatabase::default()
            .account_code(CONTRACT, bytecode.clone())
            .account_balance(CALLER, caller_balance)
    };
    let make_latest_db = || make_latest_db_call(CONTRACT, bytecode.clone(), CALLER, caller_balance);

    let params = CallParams {
        caller: CALLER,
        target: CONTRACT,
        gas_limit: 10_000_000_000,
        data: calldata.clone(),
        ..Default::default()
    };
    add_baseline_rows(&mut group, &params, &make_pinned_db, &make_latest_db);

    for &(name, spec) in SPEC_IDS {
        group.bench_function(name, |b| {
            b.iter(|| {
                let db = make_pinned_db();
                transact_call(black_box(spec), db, 10_000_000_000, calldata.clone());
            })
        });
    }
    group.finish();
}

//
// ============================================================================
// Subcall Benchmarks
// ============================================================================
//
// Measures contract CALL overhead with three variants:
// - 1000 subcalls with 1 wei value transfer
// - 1000 subcalls with no value (same account)
// - 1000 nested subcalls (each target does another subcall)
//

/// Constructs bytecode that loops 1000 times, each iteration doing a CALL to `target`
/// with the given `value` (0 or 1 wei).
fn make_loop_call_bytecode(target: Address, value: u8) -> Bytes {
    let mut code = vec![
        opcode::PUSH2,
        0x03,
        0xE8,             // PUSH2 1000 — loop counter
        opcode::JUMPDEST, // loop_start at offset 3
        opcode::PUSH1,
        0x00, // retSize
        opcode::PUSH1,
        0x00, // retOffset
        opcode::PUSH1,
        0x00, // argsSize
        opcode::PUSH1,
        0x00, // argsOffset
        opcode::PUSH1,
        value,          // value
        opcode::PUSH20, // target address
    ];
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[
        opcode::GAS, // forward all remaining gas
        opcode::CALL,
        opcode::POP, // discard success/failure
        opcode::PUSH1,
        0x01, // decrement counter
        opcode::SWAP1,
        opcode::SUB,
        opcode::DUP1, // duplicate counter for JUMPI check
        opcode::PUSH1,
        0x03,          // jump target (JUMPDEST offset)
        opcode::JUMPI, // jump back if counter != 0
        opcode::POP,   // clean up remaining counter (0)
        opcode::STOP,
    ]);
    code.into()
}

/// Minimal contract that just STOPs.
fn make_stop_bytecode() -> Bytes {
    Bytes::from_static(&[opcode::STOP])
}

/// Constructs bytecode that does a single CALL (no value) to `target`, then STOPs.
fn make_subcall_bytecode(target: Address) -> Bytes {
    let mut code = vec![
        opcode::PUSH1,
        0x00, // retSize
        opcode::PUSH1,
        0x00, // retOffset
        opcode::PUSH1,
        0x00, // argsSize
        opcode::PUSH1,
        0x00, // argsOffset
        opcode::PUSH1,
        0x00,           // value (no transfer)
        opcode::PUSH20, // target address
    ];
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[opcode::GAS, opcode::CALL, opcode::POP, opcode::STOP]);
    Bytes::from(code)
}

/// Helper to benchmark a subcall variant across the 4 baselines and mega specs.
///
/// `pinned_db_setup` is used by `MegaEvm` rows AND by both `*_pinned` baselines
/// (they all share the pinned revm `Database` trait). `latest_db_setup` produces
/// the latest-version `CacheDB` for both `*_latest` rows.
fn bench_subcall_variant(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    pinned_db_setup: impl Fn() -> MemoryDatabase,
    latest_db_setup: impl Fn() -> revm_latest::database::CacheDB<EmptyDBLatest>,
) {
    const SUBCALL_GAS_LIMIT: u64 = 10_000_000_000;

    let params = CallParams {
        caller: CALLER,
        target: CONTRACT,
        gas_limit: SUBCALL_GAS_LIMIT,
        ..Default::default()
    };
    add_baseline_rows(group, &params, &pinned_db_setup, &latest_db_setup);

    for &(name, spec) in SPEC_IDS {
        group.bench_function(name, |b| {
            b.iter(|| {
                let db = pinned_db_setup();
                let mut context = MegaContext::new(db, spec);
                context.modify_chain(|chain| {
                    chain.operator_fee_scalar = Some(U256::from(0));
                    chain.operator_fee_constant = Some(U256::from(0));
                });
                let mut evm = MegaEvm::new(context);
                let tx = TxEnvBuilder::new()
                    .caller(CALLER)
                    .call(CONTRACT)
                    .gas_limit(SUBCALL_GAS_LIMIT)
                    .build_fill();
                let mut mega_tx = MegaTransaction::new(tx);
                mega_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(mega_tx).expect("should succeed");
                assert!(r.result.is_success(), "subcall should succeed");
                black_box(r)
            })
        });
    }
}

fn bench_subcall(c: &mut Criterion) {
    let caller_balance = U256::from(u128::MAX);

    // Variant 1: 1000 subcalls each transferring 1 wei
    {
        let loop_code = make_loop_call_bytecode(SUBCALL_TARGET_A, 1);
        let stop_code = make_stop_bytecode();

        let mut group = c.benchmark_group("subcall_1000_transfer_1wei");
        group.sample_size(10);
        bench_subcall_variant(
            &mut group,
            || {
                MemoryDatabase::default()
                    .account_balance(CALLER, caller_balance)
                    .account_code(CONTRACT, loop_code.clone())
                    .account_code(SUBCALL_TARGET_A, stop_code.clone())
            },
            || {
                LatestDbBuilder::new()
                    .account_balance(CALLER, caller_balance)
                    .account_code(CONTRACT, loop_code.clone())
                    .account_code(SUBCALL_TARGET_A, stop_code.clone())
                    .build()
            },
        );
        group.finish();
    }

    // Variant 2: 1000 subcalls with no value transfer
    {
        let loop_code = make_loop_call_bytecode(SUBCALL_TARGET_A, 0);
        let stop_code = make_stop_bytecode();

        let mut group = c.benchmark_group("subcall_1000_no_value");
        group.sample_size(10);
        bench_subcall_variant(
            &mut group,
            || {
                MemoryDatabase::default()
                    .account_balance(CALLER, caller_balance)
                    .account_code(CONTRACT, loop_code.clone())
                    .account_code(SUBCALL_TARGET_A, stop_code.clone())
            },
            || {
                LatestDbBuilder::new()
                    .account_balance(CALLER, caller_balance)
                    .account_code(CONTRACT, loop_code.clone())
                    .account_code(SUBCALL_TARGET_A, stop_code.clone())
                    .build()
            },
        );
        group.finish();
    }

    // Variant 3: 1000 nested subcalls (each target does another subcall)
    {
        let loop_code = make_loop_call_bytecode(SUBCALL_TARGET_A, 0);
        let subcall_code = make_subcall_bytecode(SUBCALL_TARGET_B);
        let stop_code = make_stop_bytecode();

        let mut group = c.benchmark_group("subcall_1000_nested");
        group.sample_size(10);
        bench_subcall_variant(
            &mut group,
            || {
                MemoryDatabase::default()
                    .account_balance(CALLER, caller_balance)
                    .account_code(CONTRACT, loop_code.clone())
                    .account_code(SUBCALL_TARGET_A, subcall_code.clone())
                    .account_code(SUBCALL_TARGET_B, stop_code.clone())
            },
            || {
                LatestDbBuilder::new()
                    .account_balance(CALLER, caller_balance)
                    .account_code(CONTRACT, loop_code.clone())
                    .account_code(SUBCALL_TARGET_A, subcall_code.clone())
                    .account_code(SUBCALL_TARGET_B, stop_code.clone())
                    .build()
            },
        );
        group.finish();
    }
}

//
// ============================================================================
// Transfer Multi Benchmark
// ============================================================================
//
// Measures batch transaction execution: 1000 sequential ether transfers.
//

fn bench_transfer_multi(c: &mut Criterion) {
    let mut group = c.benchmark_group("transfer_multi_1000");
    group.sample_size(10);

    let base = U256::from(10_000);
    let targets: Vec<Address> =
        (0..1000u64).map(|i| Address::from_word((base + U256::from(i)).into())).collect();
    let caller_balance = U256::from(3_000_000_000u64);
    let target_balance = U256::from(3_000_000_000u64);

    let make_pinned_db = || {
        let mut db = MemoryDatabase::default().account_balance(CALLER, caller_balance);
        for target in &targets {
            db = db.account_balance(*target, target_balance);
        }
        db
    };
    let make_latest_db = || {
        let mut builder = LatestDbBuilder::new().account_balance(CALLER, caller_balance);
        for target in &targets {
            builder = builder.account_balance(*target, target_balance);
        }
        builder.build()
    };

    group.bench_function("revm_pinned", |b| {
        b.iter(|| {
            let mut evm = ContextPinned::mainnet().with_db(make_pinned_db()).build_mainnet();
            for target in &targets {
                let tx = TxEnvBuilder::new()
                    .caller(CALLER)
                    .call(*target)
                    .value(U256::from(1))
                    .gas_limit(100_000)
                    .build_fill();
                let r = evm.transact(tx).expect("revm_pinned transfer");
                assert!(
                    r.result.is_success(),
                    "revm_pinned transfer should succeed: {:?}",
                    r.result
                );
                black_box(&r);
            }
        })
    });

    group.bench_function("revm_latest", |b| {
        b.iter(|| {
            let mut evm = ContextLatest::mainnet()
                .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(SpecIdLatest::CANCUN))
                .with_db(make_latest_db())
                .build_mainnet();
            for target in &targets {
                let tx = TxEnvBuilderLatest::new()
                    .caller(CALLER)
                    .call(*target)
                    .value(U256::from(1))
                    .gas_limit(100_000)
                    .build_fill();
                let r = evm.transact(tx).expect("revm_latest transfer");
                assert!(
                    r.result.is_success(),
                    "revm_latest transfer should succeed: {:?}",
                    r.result
                );
                black_box(&r);
            }
        })
    });

    group.bench_function("op_revm_pinned", |b| {
        b.iter(|| {
            let mut ctx = <OpContextPinned<EmptyDBPinned>>::op().with_db(make_pinned_db());
            ctx.modify_chain(|chain| {
                chain.operator_fee_scalar = Some(U256::ZERO);
                chain.operator_fee_constant = Some(U256::ZERO);
            });
            let mut evm = ctx.build_op();
            for target in &targets {
                let tx_env = TxEnvBuilder::new()
                    .caller(CALLER)
                    .call(*target)
                    .value(U256::from(1))
                    .gas_limit(100_000)
                    .build_fill();
                let mut op_tx = OpTransactionPinned::new(tx_env);
                op_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(op_tx).expect("op_revm_pinned transfer");
                assert!(
                    r.result.is_success(),
                    "op_revm_pinned transfer should succeed: {:?}",
                    r.result
                );
                black_box(&r);
            }
        })
    });

    group.bench_function("op_revm_latest", |b| {
        b.iter(|| {
            let mut ctx = <OpContextLatest<EmptyDBLatest>>::op().with_db(make_latest_db());
            ctx.modify_chain(|chain| {
                chain.operator_fee_scalar = Some(U256::ZERO);
                chain.operator_fee_constant = Some(U256::ZERO);
            });
            let mut evm = ctx.build_op();
            for target in &targets {
                let tx_env = TxEnvBuilderLatest::new()
                    .caller(CALLER)
                    .call(*target)
                    .value(U256::from(1))
                    .gas_limit(100_000)
                    .build_fill();
                let mut op_tx = OpTransactionLatest::new(tx_env);
                op_tx.enveloped_tx = Some(Bytes::new());
                let r = evm.transact(op_tx).expect("op_revm_latest transfer");
                assert!(
                    r.result.is_success(),
                    "op_revm_latest transfer should succeed: {:?}",
                    r.result
                );
                black_box(&r);
            }
        })
    });

    for &(name, spec) in SPEC_IDS {
        let targets = targets.clone();
        group.bench_function(name, |b| {
            b.iter(|| {
                let db = make_pinned_db();

                let mut context = MegaContext::new(db, spec);
                context.modify_chain(|chain| {
                    chain.operator_fee_scalar = Some(U256::from(0));
                    chain.operator_fee_constant = Some(U256::from(0));
                });
                let mut evm = MegaEvm::new(context);

                for target in &targets {
                    let tx = TxEnvBuilder::new()
                        .caller(CALLER)
                        .call(*target)
                        .value(U256::from(1))
                        .gas_limit(100_000)
                        .build_fill();
                    let mut mega_tx = MegaTransaction::new(tx);
                    mega_tx.enveloped_tx = Some(Bytes::new());
                    let r = evm.transact(mega_tx).expect("transfer should succeed");
                    black_box(&r);
                }
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_snailtracer, bench_analysis, bench_subcall, bench_transfer_multi,);
criterion_main!(benches);
