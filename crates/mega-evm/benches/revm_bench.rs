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
//! 5. `mega_<spec>`     — `MegaEvm` at `EQUIVALENCE` / `MINI_REX` / `REX4` / `REX5`.
//!
//! Ported workloads:
//! - **snailtracer**: CPU-intensive ray tracer exercising many opcodes
//! - **analysis**: ERC20-like contract bytecode execution
//! - **subcall**: Multi-level contract call performance (1000 iterations)
//! - **`transfer_multi`**: Batch transaction execution (1000 transfers)
//!
//! New workloads must avoid touching mega-evm's per-tx resource limits
//! (compute gas / data size / KV updates / state growth) under any spec, or the
//! `mega_*` row would halt early and skew the baseline comparison.
//! Likewise, every vanilla baseline is pinned to a Cancun-equivalent fork
//! (`revm` rows to Cancun, `op-revm` rows to Holocene) in `common::subject`, so
//! a new workload must execute successfully under Cancun semantics on every row.

#![allow(missing_docs)]

use alloy_primitives::{address, bytes, Address, Bytes, U256};
use criterion::{criterion_group, criterion_main, Criterion};
use revm::bytecode::opcode;

mod common;
use common::{register_all, Account, TxSpec, Workload};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");

const SUBCALL_TARGET_A: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const SUBCALL_TARGET_B: Address = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

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

    let workload = Workload::single(
        vec![Account::new(CONTRACT).code(bytecode), Account::new(CALLER).balance(caller_balance)],
        TxSpec::call(CALLER, CONTRACT).gas_limit(1_000_000_000).data(calldata),
    );
    register_all(&mut group, &workload);
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

    let workload = Workload::single(
        vec![Account::new(CONTRACT).code(bytecode), Account::new(CALLER).balance(caller_balance)],
        TxSpec::call(CALLER, CONTRACT).gas_limit(10_000_000_000).data(calldata),
    );
    register_all(&mut group, &workload);
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

/// Benchmark one subcall variant across the 4 baselines and mega specs. The
/// contract at `CONTRACT` drives the loop, so the tx itself just calls it with
/// a high gas limit and no value.
fn bench_subcall_variant(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    accounts: Vec<Account>,
) {
    const SUBCALL_GAS_LIMIT: u64 = 10_000_000_000;
    let workload =
        Workload::single(accounts, TxSpec::call(CALLER, CONTRACT).gas_limit(SUBCALL_GAS_LIMIT));
    register_all(group, &workload);
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
            vec![
                Account::new(CALLER).balance(caller_balance),
                Account::new(CONTRACT).code(loop_code),
                Account::new(SUBCALL_TARGET_A).code(stop_code),
            ],
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
            vec![
                Account::new(CALLER).balance(caller_balance),
                Account::new(CONTRACT).code(loop_code),
                Account::new(SUBCALL_TARGET_A).code(stop_code),
            ],
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
            vec![
                Account::new(CALLER).balance(caller_balance),
                Account::new(CONTRACT).code(loop_code),
                Account::new(SUBCALL_TARGET_A).code(subcall_code),
                Account::new(SUBCALL_TARGET_B).code(stop_code),
            ],
        );
        group.finish();
    }
}

//
// ============================================================================
// Transfer Multi Benchmark
// ============================================================================
//
// Measures batch transaction execution: 1000 sequential ether transfers run on
// one reused EVM (a subject loops the workload's tx list internally).
//

fn bench_transfer_multi(c: &mut Criterion) {
    let mut group = c.benchmark_group("transfer_multi_1000");
    group.sample_size(10);

    let base = U256::from(10_000);
    let targets: Vec<Address> =
        (0..1000u64).map(|i| Address::from_word((base + U256::from(i)).into())).collect();
    let caller_balance = U256::from(3_000_000_000u64);
    let target_balance = U256::from(3_000_000_000u64);

    let mut accounts = vec![Account::new(CALLER).balance(caller_balance)];
    accounts.extend(targets.iter().map(|t| Account::new(*t).balance(target_balance)));

    let txs: Vec<TxSpec> = targets
        .iter()
        .map(|t| TxSpec::call(CALLER, *t).value(U256::from(1)).gas_limit(100_000))
        .collect();

    let workload = Workload::new(accounts, txs);
    register_all(&mut group, &workload);
    group.finish();
}

criterion_group!(benches, bench_snailtracer, bench_analysis, bench_subcall, bench_transfer_multi,);
criterion_main!(benches);
