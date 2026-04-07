//! Benchmarks ported from upstream revm.
//!
//! These benchmarks are adapted from revm's revme bench suite to measure equivalent
//! workloads through mega-evm's execution pipeline, enabling performance comparison
//! between vanilla revm and mega-evm.
//!
//! Ported benchmarks:
//! - **snailtracer**: CPU-intensive ray tracer exercising many opcodes
//! - **analysis**: ERC20-like contract bytecode execution
//! - **subcall**: Multi-level contract call performance (1000 iterations)
//! - **`transfer_multi`**: Batch transaction execution (1000 transfers)

#![allow(missing_docs)]

use alloy_primitives::{address, bytes, Address, Bytes, U256};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use mega_evm::{test_utils::MemoryDatabase, MegaContext, MegaEvm, MegaSpecId, MegaTransaction};
use revm::{bytecode::opcode, context::tx::TxEnvBuilder, ExecuteEvm};

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

    let mut group = c.benchmark_group("snailtracer");
    group.sample_size(10);

    for &(name, spec) in SPEC_IDS {
        group.bench_function(name, |b| {
            b.iter(|| {
                let db = MemoryDatabase::default()
                    .account_code(CONTRACT, bytecode.clone())
                    .account_balance(CALLER, U256::from(10).pow(U256::from(18)));
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

    let mut group = c.benchmark_group("analysis");

    for &(name, spec) in SPEC_IDS {
        group.bench_function(name, |b| {
            b.iter(|| {
                let db = MemoryDatabase::default()
                    .account_code(CONTRACT, bytecode.clone())
                    .account_balance(CALLER, U256::from(10).pow(U256::from(18)));
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

/// Helper to benchmark a subcall variant across specs.
fn bench_subcall_variant(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    db_setup: impl Fn() -> MemoryDatabase,
) {
    for &(name, spec) in SPEC_IDS {
        group.bench_function(name, |b| {
            b.iter(|| {
                let db = db_setup();
                let mut context = MegaContext::new(db, spec);
                context.modify_chain(|chain| {
                    chain.operator_fee_scalar = Some(U256::from(0));
                    chain.operator_fee_constant = Some(U256::from(0));
                });
                let mut evm = MegaEvm::new(context);
                let tx = TxEnvBuilder::new()
                    .caller(CALLER)
                    .call(CONTRACT)
                    .gas_limit(10_000_000_000)
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
    // Variant 1: 1000 subcalls each transferring 1 wei
    {
        let loop_code = make_loop_call_bytecode(SUBCALL_TARGET_A, 1);
        let stop_code = make_stop_bytecode();

        let mut group = c.benchmark_group("subcall_1000_transfer_1wei");
        group.sample_size(10);
        bench_subcall_variant(&mut group, || {
            MemoryDatabase::default()
                .account_balance(CALLER, U256::from(u128::MAX))
                .account_code(CONTRACT, loop_code.clone())
                .account_code(SUBCALL_TARGET_A, stop_code.clone())
        });
        group.finish();
    }

    // Variant 2: 1000 subcalls with no value transfer
    {
        let loop_code = make_loop_call_bytecode(SUBCALL_TARGET_A, 0);
        let stop_code = make_stop_bytecode();

        let mut group = c.benchmark_group("subcall_1000_no_value");
        group.sample_size(10);
        bench_subcall_variant(&mut group, || {
            MemoryDatabase::default()
                .account_balance(CALLER, U256::from(u128::MAX))
                .account_code(CONTRACT, loop_code.clone())
                .account_code(SUBCALL_TARGET_A, stop_code.clone())
        });
        group.finish();
    }

    // Variant 3: 1000 nested subcalls (each target does another subcall)
    {
        let loop_code = make_loop_call_bytecode(SUBCALL_TARGET_A, 0);
        let subcall_code = make_subcall_bytecode(SUBCALL_TARGET_B);
        let stop_code = make_stop_bytecode();

        let mut group = c.benchmark_group("subcall_1000_nested");
        group.sample_size(10);
        bench_subcall_variant(&mut group, || {
            MemoryDatabase::default()
                .account_balance(CALLER, U256::from(u128::MAX))
                .account_code(CONTRACT, loop_code.clone())
                .account_code(SUBCALL_TARGET_A, subcall_code.clone())
                .account_code(SUBCALL_TARGET_B, stop_code.clone())
        });
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

    for &(name, spec) in SPEC_IDS {
        let targets = targets.clone();
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut db =
                    MemoryDatabase::default().account_balance(CALLER, U256::from(3_000_000_000u64));
                for target in &targets {
                    db = db.account_balance(*target, U256::from(3_000_000_000u64));
                }

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
