//! Benchmarks for CPU-intensive EVM opcodes and precompiles.
//!
//! This benchmark suite measures the computational cost of various EVM operations
//! to understand their real-world performance characteristics across different
//! EVM specifications (`EQUIVALENCE` vs `MINI_REX`).
//!
//! The benchmarks are organized into several categories:
//! - Cryptographic hash operations (KECCAK256, SHA256, BLAKE2F)
//! - Arithmetic operations (EXP, ADDMOD, MULMOD, DIV, etc.)
//! - Elliptic curve precompiles (ECRECOVER, ECADD, ECMUL, ECPAIRING)
//! - Modular exponentiation (MODEXP)

#![allow(missing_docs)]

use alloy_primitives::{address, bytes, Address, Bytes, U256};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    DefaultExternalEnvs, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    bytecode::opcode::{
        ADD, ADDMOD, ADDRESS, AND, BALANCE, BASEFEE, BLOBBASEFEE, BLOBHASH, BLOCKHASH, BYTE,
        CALLDATACOPY, CALLDATALOAD, CALLDATASIZE, CALLER, CALLVALUE, CHAINID, CLZ, CODECOPY,
        CODESIZE, COINBASE, DIFFICULTY, DIV, DUP1, EQ, EXP, EXTCODECOPY, EXTCODEHASH, EXTCODESIZE,
        GAS, GASLIMIT, GASPRICE, GT, ISZERO, JUMP, JUMPDEST, JUMPI, KECCAK256, LT, MCOPY, MLOAD,
        MOD, MSIZE, MSTORE, MSTORE8, MUL, MULMOD, NOT, NUMBER, OPCODE_INFO, OR, ORIGIN, PC, POP,
        RETURNDATACOPY, RETURNDATASIZE, SAR, SDIV, SELFBALANCE, SGT, SHL, SHR, SIGNEXTEND, SLOAD,
        SLT, SMOD, SSTORE, STATICCALL, STOP, SUB, TIMESTAMP, TLOAD, TSTORE, XOR,
    },
    context::{
        result::{ExecResultAndState, ExecutionResult},
        tx::TxEnvBuilder,
    },
    precompile::{
        blake2, bls12_381, bn128,
        hash::{RIPEMD160, SHA256},
        kzg_point_evaluation, modexp,
        secp256k1::ECRECOVER,
    },
    ExecuteEvm,
};
use sha2::{Digest, Sha256 as Sha256Hash};

const CALLER_ADDR: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");

/// Number of iterations for opcode benchmarks
const ITERATIONS: usize = 10000;

/// Specification IDs to benchmark against
const SPEC_IDS: &[(&str, MegaSpecId)] = &[
    // ("equivalence", MegaSpecId::EQUIVALENCE),
    ("mini_rex", MegaSpecId::MINI_REX),
];

//
// ============================================================================
// Common Utility Functions
// ============================================================================
//

/// Execute bytecode with the given spec and return the transaction execution result.
fn execute_bytecode(
    bytecode: &Bytes,
    spec: MegaSpecId,
) -> ExecResultAndState<ExecutionResult<MegaHaltReason>> {
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, bytecode.clone())
        .account_balance(CALLER_ADDR, U256::from(10).pow(U256::from(18)));

    let mut context = MegaContext::new(db, spec, DefaultExternalEnvs::default());
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);

    let tx = TxEnvBuilder::new()
        .caller(CALLER_ADDR)
        .call(CONTRACT)
        .gas_limit(10_000_000_000u64)
        .build_fill();
    let mut mega_tx = MegaTransaction::new(tx);
    mega_tx.enveloped_tx = Some(Bytes::new());

    let r = evm.transact(mega_tx).expect("transaction should succeed");
    // assert!(r.result.is_success(), "transaction should succeed: {:?}", r.result);
    assert!(r.result.gas_used() >= 21000, "transaction should spend at least 21000 gas");
    r
}

/// Execute bytecode with the given spec and return gas used.
fn execute_and_get_gas(bytecode: &Bytes, spec: MegaSpecId) -> u64 {
    execute_bytecode(bytecode, spec).result.gas_used()
}

//
// ============================================================================
// Simple Opcodes
// ============================================================================
//

/// Helper function to benchmark a simple opcode with bytecode.
fn bench_pure_opcode(c: &mut Criterion, name: &str, iterations: usize, bytecode: Bytes) {
    let mut group = c.benchmark_group(name);

    // Run once to collect gas consumption before benchmarking
    println!("\n=== Gas consumption for {} ({}x iterations) ===", name, iterations);
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

/// Generic helper to generate bytecode for arithmetic opcodes using OPCODE_INFO metadata.
fn generate_pure_op_bytecode(opcode: u8, iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Get opcode metadata to determine number of inputs
    let opcode_info = OPCODE_INFO[opcode as usize].expect("opcode should be valid");
    let num_inputs = opcode_info.inputs();

    // Test values for stack inputs
    let test_values = [
        12523853482567130975039247523u128,
        43896787985476289348724928347u128,
        638957894375983475u128,
        98357u128,
        493867489376934876236045u128,
    ];

    for _ in 0..iterations {
        // Push the required number of inputs onto the stack
        for i in 0..num_inputs {
            builder = builder.push_number(test_values[i as usize]);
        }
        // Execute the opcode
        builder = builder.append(opcode);
        // Pop the result
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark ADD opcode.
fn bench_add_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "add", ITERATIONS, generate_pure_op_bytecode(ADD, ITERATIONS));
}

/// Benchmark ADDRESS opcode.
fn bench_address_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "address", ITERATIONS, generate_pure_op_bytecode(ADDRESS, ITERATIONS));
}

/// Benchmark SDIV opcode.
fn bench_sdiv_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "sdiv", ITERATIONS, generate_pure_op_bytecode(SDIV, ITERATIONS));
}

/// Benchmark DIV opcode.
fn bench_div_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "div", ITERATIONS, generate_pure_op_bytecode(DIV, ITERATIONS));
}

/// Benchmark MUL opcode.
fn bench_mul_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "mul", ITERATIONS, generate_pure_op_bytecode(MUL, ITERATIONS));
}

/// Benchmark MULMOD opcode.
fn bench_mulmod_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "mulmod", ITERATIONS, generate_pure_op_bytecode(MULMOD, ITERATIONS));
}

/// Benchmark ADDMOD opcode.
fn bench_addmod_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "addmod", ITERATIONS, generate_pure_op_bytecode(ADDMOD, ITERATIONS));
}

/// Benchmark MOD opcode.
fn bench_mod_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "mod", ITERATIONS, generate_pure_op_bytecode(MOD, ITERATIONS));
}

/// Benchmark SMOD opcode.
fn bench_smod_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "smod", ITERATIONS, generate_pure_op_bytecode(SMOD, ITERATIONS));
}

/// Benchmark SUB opcode.
fn bench_sub_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "sub", ITERATIONS, generate_pure_op_bytecode(SUB, ITERATIONS));
}

/// Benchmark EXP opcode.
fn bench_exp_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "exp", ITERATIONS, generate_pure_op_bytecode(EXP, ITERATIONS));
}

/// Benchmark SIGNEXTEND opcode.
fn bench_signextend_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "signextend",
        ITERATIONS,
        generate_pure_op_bytecode(SIGNEXTEND, ITERATIONS),
    );
}

/// Benchmark LT opcode.
fn bench_lt_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "lt", ITERATIONS, generate_pure_op_bytecode(LT, ITERATIONS));
}

/// Benchmark GT opcode.
fn bench_gt_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "gt", ITERATIONS, generate_pure_op_bytecode(GT, ITERATIONS));
}

/// Benchmark SLT opcode.
fn bench_slt_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "slt", ITERATIONS, generate_pure_op_bytecode(SLT, ITERATIONS));
}

/// Benchmark SGT opcode.
fn bench_sgt_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "sgt", ITERATIONS, generate_pure_op_bytecode(SGT, ITERATIONS));
}

/// Benchmark EQ opcode.
fn bench_eq_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "eq", ITERATIONS, generate_pure_op_bytecode(EQ, ITERATIONS));
}

/// Benchmark ISZERO opcode.
fn bench_iszero_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "iszero", ITERATIONS, generate_pure_op_bytecode(ISZERO, ITERATIONS));
}

/// Benchmark AND opcode.
fn bench_and_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "and", ITERATIONS, generate_pure_op_bytecode(AND, ITERATIONS));
}

/// Benchmark OR opcode.
fn bench_or_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "or", ITERATIONS, generate_pure_op_bytecode(OR, ITERATIONS));
}

/// Benchmark XOR opcode.
fn bench_xor_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "xor", ITERATIONS, generate_pure_op_bytecode(XOR, ITERATIONS));
}

/// Benchmark NOT opcode.
fn bench_not_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "not", ITERATIONS, generate_pure_op_bytecode(NOT, ITERATIONS));
}

/// Benchmark BYTE opcode.
fn bench_byte_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "byte", ITERATIONS, generate_pure_op_bytecode(BYTE, ITERATIONS));
}

/// Benchmark SHL opcode.
fn bench_shl_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "shl", ITERATIONS, generate_pure_op_bytecode(SHL, ITERATIONS));
}

/// Benchmark SHR opcode.
fn bench_shr_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "shr", ITERATIONS, generate_pure_op_bytecode(SHR, ITERATIONS));
}

/// Benchmark SAR opcode.
fn bench_sar_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "sar", ITERATIONS, generate_pure_op_bytecode(SAR, ITERATIONS));
}

/// Benchmark CLZ opcode.
fn bench_clz_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "clz", ITERATIONS, generate_pure_op_bytecode(CLZ, ITERATIONS));
}

/// Benchmark STOP opcode.
fn bench_stop_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "stop", ITERATIONS, generate_pure_op_bytecode(STOP, ITERATIONS));
}

/// Benchmark BALANCE opcode.
fn bench_balance_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "balance", ITERATIONS, generate_pure_op_bytecode(BALANCE, ITERATIONS));
}

/// Benchmark ORIGIN opcode.
fn bench_origin_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "origin", ITERATIONS, generate_pure_op_bytecode(ORIGIN, ITERATIONS));
}

/// Benchmark CALLER opcode.
fn bench_caller_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "caller", ITERATIONS, generate_pure_op_bytecode(CALLER, ITERATIONS));
}

/// Benchmark CALLVALUE opcode.
fn bench_callvalue_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "callvalue", ITERATIONS, generate_pure_op_bytecode(CALLVALUE, ITERATIONS));
}

/// Benchmark CALLDATALOAD opcode.
fn bench_calldataload_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "calldataload",
        ITERATIONS,
        generate_pure_op_bytecode(CALLDATALOAD, ITERATIONS),
    );
}

/// Benchmark CALLDATASIZE opcode.
fn bench_calldatasize_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "calldatasize",
        ITERATIONS,
        generate_pure_op_bytecode(CALLDATASIZE, ITERATIONS),
    );
}

/// Benchmark CALLDATACOPY opcode.
fn bench_calldatacopy_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "calldatacopy",
        ITERATIONS,
        generate_pure_op_bytecode(CALLDATACOPY, ITERATIONS),
    );
}

/// Benchmark CODESIZE opcode.
fn bench_codesize_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "codesize", ITERATIONS, generate_pure_op_bytecode(CODESIZE, ITERATIONS));
}

/// Benchmark CODECOPY opcode.
fn bench_codecopy_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "codecopy", ITERATIONS, generate_pure_op_bytecode(CODECOPY, ITERATIONS));
}

/// Benchmark GASPRICE opcode.
fn bench_gasprice_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "gasprice", ITERATIONS, generate_pure_op_bytecode(GASPRICE, ITERATIONS));
}

/// Benchmark BLOBHASH opcode.
fn bench_blobhash_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "blobhash", ITERATIONS, generate_pure_op_bytecode(BLOBHASH, ITERATIONS));
}

/// Benchmark EXTCODESIZE opcode.
fn bench_extcodesize_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "extcodesize",
        ITERATIONS,
        generate_pure_op_bytecode(EXTCODESIZE, ITERATIONS),
    );
}

/// Benchmark EXTCODECOPY opcode.
fn bench_extcodecopy_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "extcodecopy",
        ITERATIONS,
        generate_pure_op_bytecode(EXTCODECOPY, ITERATIONS),
    );
}

/// Benchmark EXTCODEHASH opcode.
fn bench_extcodehash_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "extcodehash",
        ITERATIONS,
        generate_pure_op_bytecode(EXTCODEHASH, ITERATIONS),
    );
}

/// Benchmark BLOCKHASH opcode.
fn bench_blockhash_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "blockhash", ITERATIONS, generate_pure_op_bytecode(BLOCKHASH, ITERATIONS));
}

/// Benchmark SELFBALANCE opcode.
fn bench_selfbalance_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "selfbalance",
        ITERATIONS,
        generate_pure_op_bytecode(SELFBALANCE, ITERATIONS),
    );
}

/// Benchmark SLOAD opcode.
fn bench_sload_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "sload", ITERATIONS, generate_pure_op_bytecode(SLOAD, ITERATIONS));
}

/// Benchmark SSTORE opcode.
fn bench_sstore_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "sstore", ITERATIONS, generate_pure_op_bytecode(SSTORE, ITERATIONS));
}

/// Benchmark TLOAD opcode.
fn bench_tload_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "tload", ITERATIONS, generate_pure_op_bytecode(TLOAD, ITERATIONS));
}

/// Benchmark COINBASE opcode.
fn bench_coinbase_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "coinbase", ITERATIONS, generate_pure_op_bytecode(COINBASE, ITERATIONS));
}

/// Benchmark TIMESTAMP opcode.
fn bench_timestamp_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "timestamp", ITERATIONS, generate_pure_op_bytecode(TIMESTAMP, ITERATIONS));
}

/// Benchmark NUMBER opcode.
fn bench_number_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "number", ITERATIONS, generate_pure_op_bytecode(NUMBER, ITERATIONS));
}

/// Benchmark DIFFICULTY opcode.
fn bench_difficulty_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "difficulty",
        ITERATIONS,
        generate_pure_op_bytecode(DIFFICULTY, ITERATIONS),
    );
}

/// Benchmark GASLIMIT opcode.
fn bench_gaslimit_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "gaslimit", ITERATIONS, generate_pure_op_bytecode(GASLIMIT, ITERATIONS));
}

/// Benchmark CHAINID opcode.
fn bench_chainid_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "chainid", ITERATIONS, generate_pure_op_bytecode(CHAINID, ITERATIONS));
}

/// Benchmark BASEFEE opcode.
fn bench_basefee_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "basefee", ITERATIONS, generate_pure_op_bytecode(BASEFEE, ITERATIONS));
}

/// Benchmark BLOBBASEFEE opcode.
fn bench_blobbasefee_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "blobbasefee",
        ITERATIONS,
        generate_pure_op_bytecode(BLOBBASEFEE, ITERATIONS),
    );
}

/// Benchmark POP opcode.
fn bench_pop_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "pop", ITERATIONS, generate_pure_op_bytecode(POP, ITERATIONS));
}

/// Benchmark MLOAD opcode.
fn bench_mload_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "mload", ITERATIONS, generate_pure_op_bytecode(MLOAD, ITERATIONS));
}

/// Benchmark MSTORE opcode.
fn bench_mstore_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "mstore", ITERATIONS, generate_pure_op_bytecode(MSTORE, ITERATIONS));
}

/// Benchmark MSTORE8 opcode.
fn bench_mstore8_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "mstore8", ITERATIONS, generate_pure_op_bytecode(MSTORE8, ITERATIONS));
}

/// Benchmark MSIZE opcode.
fn bench_msize_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "msize", ITERATIONS, generate_pure_op_bytecode(MSIZE, ITERATIONS));
}

/// Benchmark MCOPY opcode.
fn bench_mcopy_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "mcopy", ITERATIONS, generate_pure_op_bytecode(MCOPY, ITERATIONS));
}

/// Benchmark PC opcode.
fn bench_pc_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "pc", ITERATIONS, generate_pure_op_bytecode(PC, ITERATIONS));
}

/// Benchmark JUMPDEST opcode.
fn bench_jumpdest_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "jumpdest", ITERATIONS, generate_pure_op_bytecode(JUMPDEST, ITERATIONS));
}

/// Benchmark GAS opcode.
fn bench_gas_opcode(c: &mut Criterion) {
    bench_pure_opcode(c, "gas", ITERATIONS, generate_pure_op_bytecode(GAS, ITERATIONS));
}

/// Benchmark RETURNDATASIZE opcode.
fn bench_returndatasize_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "returndatasize",
        ITERATIONS,
        generate_pure_op_bytecode(RETURNDATASIZE, ITERATIONS),
    );
}

/// Benchmark RETURNDATACOPY opcode.
fn bench_returndatacopy_opcode(c: &mut Criterion) {
    bench_pure_opcode(
        c,
        "returndatacopy",
        ITERATIONS,
        generate_pure_op_bytecode(RETURNDATACOPY, ITERATIONS),
    );
}

//
// ============================================================================
// TSTORE Benchmarks
// ============================================================================
//

/// Generate bytecode that benchmarks TSTORE opcode.
fn generate_tstore_bytecode() -> Bytes {
    let mut builder = BytecodeBuilder::default().push_number(1u64);
    let jump_dest = builder.len();
    builder = builder
        .append(JUMPDEST)
        .append(DUP1)
        .append(DUP1)
        .append(TSTORE)
        .push_number(1u8)
        .append(ADD)
        .push_number(jump_dest as u64)
        .append(JUMP);
    builder.stop().build()
}

/// Benchmark TSTORE opcode.
fn bench_tstore_opcode(c: &mut Criterion) {
    let bytecode = generate_tstore_bytecode();

    // Run once to collect gas consumption before benchmarking
    println!("\n=== Gas consumption for tstore (infinite loop) ===");
    for &(spec_name, spec) in SPEC_IDS {
        let start_time = std::time::Instant::now();
        let gas_used = execute_and_get_gas(&bytecode, spec);
        let end_time = std::time::Instant::now();
        let duration = end_time.duration_since(start_time);
        println!(
            "  {} spec: {} gas took {:?} seconds",
            spec_name,
            gas_used,
            duration.as_secs_f64()
        );
    }
    println!();

    let mut group = c.benchmark_group("tstore");
    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }
    group.finish();
}

//
// ============================================================================
// KECCAK256 Benchmarks
// ============================================================================
//

/// Generate bytecode that benchmarks KECCAK256 opcode with specified data size.
fn generate_keccak256_bytecode(data_size: usize, iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Fill memory with data
    let data = vec![0xFF; data_size];
    builder = builder.mstore(0, data);

    // Call KECCAK256 multiple times
    for _ in 0..iterations {
        builder = builder.push_number(data_size as u64); // size
        builder = builder.push_number(0_u64); // offset
        builder = builder.append(KECCAK256);
        builder = builder.append(POP); // remove hash result from stack
    }

    builder.build()
}

/// Benchmark KECCAK256 opcode with varying data sizes.
fn bench_keccak256_opcode(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let test_cases = [("32B", 32), ("1KB", 1024)];

    for (size_name, data_size) in test_cases {
        let mut group = c.benchmark_group(format!("keccak256_{}", size_name));
        let bytecode = generate_keccak256_bytecode(data_size, ITERATIONS);

        // Run once to collect gas consumption before benchmarking
        println!(
            "\n=== Gas consumption for keccak256_{} ({}x iterations) ===",
            size_name, ITERATIONS
        );
        for &(spec_name, spec) in SPEC_IDS {
            let gas_used = execute_and_get_gas(&bytecode, spec);
            println!("  {} spec: {} gas", spec_name, gas_used);
        }
        println!();

        for &(spec_name, spec) in SPEC_IDS {
            group.bench_function(spec_name, |b| {
                b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
            });
        }

        group.finish();
    }
}

//
// ============================================================================
// ECRECOVER Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls ECRECOVER precompile.
fn generate_ecrecover_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Sample ECRECOVER input (hash, v, r, s)
    let r = bytes!("0x9242685bf161793cc25603c231bc2f568eb630ea16aa137d2664ac8038825608");
    let s = bytes!("0x4f8ae3bd7535248d0bd448298cc2e2071e56992d0774dc340c368ae950852ada");
    let v = U256::from(28);
    let hash = bytes!("0x456e9aea5e197a1f1af7a3e85a3212fa4049a3ba34c2289b4c860fc0b0c64ef3");
    let _signer = address!("0x7156526fbd7a3c72969b54f64e42c10fbb768c8a");

    // Store input data in memory
    builder = builder.mstore(0, hash); // hash at offset 0
    builder = builder.mstore(32, v.to_be_bytes_vec()); // v at offset 32
    builder = builder.mstore(64, &r); // r at offset 64
    builder = builder.mstore(96, &s); // s at offset 96

    for _ in 0..iterations {
        builder = builder.push_number(32_u64); // retSize
        builder = builder.push_number(128_u64); // retOffset
        builder = builder.push_number(128_u64); // argsSize (hash + v + r + s = 128 bytes)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*ECRECOVER.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP); // remove success flag
    }

    builder.build()
}

/// Benchmark ECRECOVER precompile.
fn bench_ecrecover_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_ecrecover");
    let bytecode = generate_ecrecover_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!("\n=== Gas consumption for precompile_ecrecover ({}x iterations) ===", ITERATIONS);
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// SHA256 Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls SHA256 precompile.
fn generate_sha256_bytecode(data_size: usize, iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Fill memory with data
    let data = vec![0xFF; data_size];
    builder = builder.mstore(0, data);

    for _ in 0..iterations {
        builder = builder.push_number(32_u64); // retSize (SHA256 outputs 32 bytes)
        builder = builder.push_number(data_size as u64 + 32); // retOffset
        builder = builder.push_number(data_size as u64); // argsSize
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*SHA256.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark SHA256 precompile with varying input sizes.
fn bench_sha256_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let test_cases = [("32B", 32), ("1KB", 1024)];

    for (size_name, data_size) in test_cases {
        let mut group = c.benchmark_group(format!("precompile_sha256_{}", size_name));
        let bytecode = generate_sha256_bytecode(data_size, ITERATIONS);

        // Run once to collect gas consumption before benchmarking
        println!(
            "\n=== Gas consumption for precompile_sha256_{} ({}x iterations) ===",
            size_name, ITERATIONS
        );
        for &(spec_name, spec) in SPEC_IDS {
            let gas_used = execute_and_get_gas(&bytecode, spec);
            println!("  {} spec: {} gas", spec_name, gas_used);
        }
        println!();

        for &(spec_name, spec) in SPEC_IDS {
            group.bench_function(spec_name, |b| {
                b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
            });
        }

        group.finish();
    }
}

//
// ============================================================================
// RIPEMD160 Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls RIPEMD160 precompile.
fn generate_ripemd160_bytecode(data_size: usize, iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Fill memory with data
    let data = vec![0xFF; data_size];
    builder = builder.mstore(0, data);

    for _ in 0..iterations {
        builder = builder.push_number(32_u64); // retSize (RIPEMD160 outputs 32 bytes, padded)
        builder = builder.push_number(data_size as u64 + 32); // retOffset
        builder = builder.push_number(data_size as u64); // argsSize
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*RIPEMD160.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark RIPEMD160 precompile with varying input sizes.
fn bench_ripemd160_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let test_cases = [("32B", 32), ("1KB", 1024)];

    for (size_name, data_size) in test_cases {
        let mut group = c.benchmark_group(format!("precompile_ripemd160_{}", size_name));
        let bytecode = generate_ripemd160_bytecode(data_size, ITERATIONS);

        // Run once to collect gas consumption before benchmarking
        println!(
            "\n=== Gas consumption for precompile_ripemd160_{} ({}x iterations) ===",
            size_name, ITERATIONS
        );
        for &(spec_name, spec) in SPEC_IDS {
            let gas_used = execute_and_get_gas(&bytecode, spec);
            println!("  {} spec: {} gas", spec_name, gas_used);
        }
        println!();

        for &(spec_name, spec) in SPEC_IDS {
            group.bench_function(spec_name, |b| {
                b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
            });
        }

        group.finish();
    }
}

//
// ============================================================================
// MODEXP Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls MODEXP precompile with specified modulus size.
fn generate_modexp_bytecode(modulus_size: usize, iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // MODEXP input format:
    // <length_of_BASE><length_of_EXPONENT><length_of_MODULUS><BASE><EXPONENT><MODULUS>
    let base_len = modulus_size;
    let exp_len = modulus_size;
    let mod_len = modulus_size;

    // Create sample values (using simple patterns for testing)
    let base = vec![0x03u8; base_len];
    let exponent = vec![0xFFu8; exp_len];
    let modulus = vec![0x05u8; mod_len];

    // Store lengths (each 32 bytes)
    builder = builder.mstore(0, U256::from(base_len).to_be_bytes_vec());
    builder = builder.mstore(32, U256::from(exp_len).to_be_bytes_vec());
    builder = builder.mstore(64, U256::from(mod_len).to_be_bytes_vec());

    // Store base, exponent, and modulus
    builder = builder.mstore(96, base);
    builder = builder.mstore(96 + base_len, exponent);
    builder = builder.mstore(96 + base_len + exp_len, modulus);

    let input_size = 96 + base_len + exp_len + mod_len;

    for _ in 0..iterations {
        builder = builder.push_number(mod_len as u64); // retSize (output size = modulus size)
        builder = builder.push_number(input_size as u64 + 32); // retOffset
        builder = builder.push_number(input_size as u64); // argsSize
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*modexp::BYZANTIUM.address());
        builder = builder.append(GAS); // gas (modexp can be expensive)
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark MODEXP precompile with varying modulus sizes.
fn bench_modexp_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let test_cases = [("32B", 32)];

    for (size_name, modulus_size) in test_cases {
        let mut group = c.benchmark_group(format!("precompile_modexp_{}", size_name));
        let bytecode = generate_modexp_bytecode(modulus_size, ITERATIONS);

        // Run once to collect gas consumption before benchmarking
        println!(
            "\n=== Gas consumption for precompile_modexp_{} ({}x iterations) ===",
            size_name, ITERATIONS
        );
        for &(spec_name, spec) in SPEC_IDS {
            let gas_used = execute_and_get_gas(&bytecode, spec);
            println!("  {} spec: {} gas", spec_name, gas_used);
        }
        println!();

        for &(spec_name, spec) in SPEC_IDS {
            group.bench_function(spec_name, |b| {
                b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
            });
        }

        group.finish();
    }
}

//
// ============================================================================
// ECADD Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls ECADD precompile.
fn generate_ecadd_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Sample ECADD input: two points on alt_bn128 curve
    // Point 1: (1, 2)
    let x1 = U256::from(1);
    let y1 = U256::from(2);
    // Point 2: (1, 2) - doubling the point
    let x2 = U256::from(1);
    let y2 = U256::from(2);

    // Store input data in memory
    builder = builder.mstore(0, x1.to_be_bytes_vec());
    builder = builder.mstore(32, y1.to_be_bytes_vec());
    builder = builder.mstore(64, x2.to_be_bytes_vec());
    builder = builder.mstore(96, y2.to_be_bytes_vec());

    for _ in 0..iterations {
        builder = builder.push_number(64_u64); // retSize (x, y = 64 bytes)
        builder = builder.push_number(128_u64); // retOffset
        builder = builder.push_number(128_u64); // argsSize (two points = 128 bytes)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(bn128::add::ADDRESS);
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark ECADD precompile.
fn bench_ecadd_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_ecadd");
    let bytecode = generate_ecadd_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!("\n=== Gas consumption for precompile_ecadd ({}x iterations) ===", ITERATIONS);
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// ECMUL Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls ECMUL precompile.
fn generate_ecmul_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Sample ECMUL input: point and scalar
    // Point: (1, 2)
    let x = U256::from(1);
    let y = U256::from(2);
    // Scalar
    let scalar = U256::from(2);

    // Store input data in memory
    builder = builder.mstore(0, x.to_be_bytes_vec());
    builder = builder.mstore(32, y.to_be_bytes_vec());
    builder = builder.mstore(64, scalar.to_be_bytes_vec());

    for _ in 0..iterations {
        builder = builder.push_number(64_u64); // retSize (x, y = 64 bytes)
        builder = builder.push_number(96_u64); // retOffset
        builder = builder.push_number(96_u64); // argsSize (point + scalar = 96 bytes)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(bn128::mul::ADDRESS);
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark ECMUL precompile.
fn bench_ecmul_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_ecmul");
    let bytecode = generate_ecmul_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!("\n=== Gas consumption for precompile_ecmul ({}x iterations) ===", ITERATIONS);
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// ECPAIRING Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls ECPAIRING precompile with specified number of pairing points.
fn generate_ecpairing_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // Sample ECPAIRING input: pairs of (G1 point, G2 point)
    // For simplicity, we'll use the identity elements which are valid points
    // G1 point (x, y) - using generator point (1, 2)
    let g1_x1 = bytes!("0x2cf44499d5d27bb186308b7af7af02ac5bc9eeb6a3d147c186b21fb1b76e18da");
    let g1_y1 = bytes!("0x2c0f001f52110ccfe69108924926e45f0b0c868df0e7bde1fe16d3242dc715f6");
    let g1_x2 = bytes!("0x1fb19bb476f6b9e44e2a32234da8212f61cd63919354bc06aef31e3cfaff3ebc");
    let g1_y2 = bytes!("0x22606845ff186793914e03e21df544c34ffe2f2f3504de8a79d9159eca2d98d9");
    let g1_x3 = bytes!("0x2bd368e28381e8eccb5fa81fc26cf3f048eea9abfdd85d7ed3ab3698d63e4f90");
    let g1_y3 = bytes!("0x2fe02e47887507adf0ff1743cbac6ba291e66f59be6bd763950bb16041a0a85e");

    let g2_x1 = bytes!("0x0000000000000000000000000000000000000000000000000000000000000001");
    let g2_y1 = bytes!("0x30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd45");
    let g2_x2 = bytes!("0x1971ff0471b09fa93caaf13cbf443c1aede09cc4328f5a62aad45f40ec133eb4");
    let g2_y2 = bytes!("0x091058a3141822985733cbdddfed0fd8d6c104e9e9eff40bf5abfef9ab163bc7");
    let g2_x3 = bytes!("0x2a23af9a5ce2ba2796c1f4e453a370eb0af8c212d9dc9acd8fc02c2e907baea2");
    let g2_y3 = bytes!("0x23a8eb0b0996252cb548a4487da97b02422ebc0e834613f954de6c7e0afdc1fc");

    // Store pairing input data in memory
    builder = builder.mstore(0, &g1_x1);
    builder = builder.mstore(32, &g1_y1);
    builder = builder.mstore(64, &g1_x2);
    builder = builder.mstore(96, &g1_y2);
    builder = builder.mstore(128, &g1_x3);
    builder = builder.mstore(160, &g1_y3);

    // G2 point (128 bytes)
    builder = builder.mstore(192, &g2_x1);
    builder = builder.mstore(224, &g2_y1);
    builder = builder.mstore(256, &g2_x2);
    builder = builder.mstore(288, &g2_y2);
    builder = builder.mstore(320, &g2_x3);
    builder = builder.mstore(352, &g2_y3);

    let input_size = 384;

    for _ in 0..iterations {
        builder = builder.push_number(32_u64); // retSize (returns 32 bytes: 0 or 1)
        builder = builder.push_number(input_size as u64 + 32); // retOffset
        builder = builder.push_number(input_size as u64); // argsSize
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(bn128::pair::ADDRESS);
        builder = builder.append(GAS); // gas (pairing needs more gas)
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark ECPAIRING precompile with varying number of pairing points.
fn bench_ecpairing_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_ecpairing");
    let bytecode = generate_ecpairing_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!("\n=== Gas consumption for precompile_ecpairing ({}x iterations) ===", ITERATIONS);
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// BLAKE2F Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls BLAKE2F precompile with specified number of rounds.
fn generate_blake2f_bytecode(rounds: u32, iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // BLAKE2F input format (213 bytes total):
    // - rounds (4 bytes, big-endian)
    // - h (64 bytes, state vector)
    // - m (128 bytes, message block)
    // - t (16 bytes, offset counters)
    // - f (1 byte, final block indicator)

    let rounds_bytes = rounds.to_be_bytes();
    let h = [0x08u8; 64]; // Initial state vector
    let m = [0xFFu8; 128]; // Message block
    let t = [0x00u8; 16]; // Offset counters
    let f = [0x00u8]; // Final block indicator (0 = not final)

    // Store BLAKE2F input in memory
    builder = builder.mstore(0, rounds_bytes);
    builder = builder.mstore(4, h);
    builder = builder.mstore(68, m);
    builder = builder.mstore(196, t);
    builder = builder.mstore(212, f);

    for _ in 0..iterations {
        builder = builder.push_number(64_u64); // retSize (BLAKE2F returns 64 bytes)
        builder = builder.push_number(256_u64); // retOffset
        builder = builder.push_number(213_u64); // argsSize
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*blake2::FUN.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark BLAKE2F precompile with varying number of rounds.
fn bench_blake2f_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let test_cases = [("1round", 1)];

    for (rounds_name, rounds) in test_cases {
        let mut group = c.benchmark_group(format!("precompile_blake2f_{}", rounds_name));
        let bytecode = generate_blake2f_bytecode(rounds, ITERATIONS);

        // Run once to collect gas consumption before benchmarking
        println!(
            "\n=== Gas consumption for precompile_blake2f_{} ({}x iterations) ===",
            rounds_name, ITERATIONS
        );
        for &(spec_name, spec) in SPEC_IDS {
            let gas_used = execute_and_get_gas(&bytecode, spec);
            println!("  {} spec: {} gas", spec_name, gas_used);
        }
        println!();

        for &(spec_name, spec) in SPEC_IDS {
            group.bench_function(spec_name, |b| {
                b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
            });
        }

        group.finish();
    }
}

//
// ============================================================================
// KZG Point Evaluation Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls KZG Point Evaluation precompile.
fn generate_kzg_point_evaluation_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // KZG Point Evaluation input (192 bytes total):
    // - versioned_hash (32 bytes)
    // - z (32 bytes)
    // - y (32 bytes)
    // - commitment (48 bytes)
    // - proof (48 bytes)

    // Sample data from EIP-4844 test vectors
    let commitment = bytes!("8f59a8d2a1a625a17f3fea0fe5eb8c896db3764f3185481bc22f91b4aaffcca25f26936857bc3a7c2539ea8ec3a952b7");
    let mut versioned_hash = Sha256Hash::digest(&commitment).to_vec();
    versioned_hash[0] = 0x01; // VERSIONED_HASH_VERSION_KZG
    let z = bytes!("73eda753299d7d483339d80809a1d80553bda402fffe5bfeffffffff00000000");
    let y = bytes!("1522a4a7f34e1ea350ae07c29c96c7e79655aa926122e95fe69fcbd932ca49e9");
    let proof = bytes!("a62ad71d14c5719385c0686f1871430475bf3a00f0aa3f7b8dd99a9abc2160744faf0070725e00b60ad9a026a15b1a8c");

    // Store KZG input in memory
    builder = builder.mstore(0, versioned_hash);
    builder = builder.mstore(32, &z);
    builder = builder.mstore(64, &y);
    builder = builder.mstore(96, &commitment);
    builder = builder.mstore(144, &proof);

    for _ in 0..iterations {
        builder = builder.push_number(64_u64); // retSize (returns 64 bytes)
        builder = builder.push_number(256_u64); // retOffset
        builder = builder.push_number(192_u64); // argsSize (total input size)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*kzg_point_evaluation::POINT_EVALUATION.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark KZG Point Evaluation precompile.
fn bench_kzg_point_evaluation_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_kzg_point_evaluation");
    let bytecode = generate_kzg_point_evaluation_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!(
        "\n=== Gas consumption for precompile_kzg_point_evaluation ({}x iterations) ===",
        ITERATIONS
    );
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// BLS12-381 G1ADD Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls BLS12-381 G1ADD precompile.
fn generate_bls12_381_g1add_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // G1ADD input: two G1 points (128 bytes each = 256 bytes total)
    // Official EIP-2537 test vector
    let g1_add_input = bytes!("0000000000000000000000000000000012196c5a43d69224d8713389285f26b98f86ee910ab3dd668e413738282003cc5b7357af9a7af54bb713d62255e80f560000000000000000000000000000000006ba8102bfbeea4416b710c73e8cce3032c31c6269c44906f8ac4f7874ce99fb17559992486528963884ce429a992fee000000000000000000000000000000000001101098f5c39893765766af4512a0c74e1bb89bc7e6fdf14e3e7337d257cc0f94658179d83320b99f31ff94cd2bac0000000000000000000000000000000003e1a9f9f44ca2cdab4f43a1a3ee3470fdf90b2fc228eb3b709fcd72f014838ac82a6d797aeefed9a0804b22ed1ce8f7");

    // Store the input
    builder = builder.mstore(0, &g1_add_input);

    for _ in 0..iterations {
        builder = builder.push_number(128_u64); // retSize (one G1 point)
        builder = builder.push_number(256_u64); // retOffset
        builder = builder.push_number(256_u64); // argsSize (two G1 points)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*bls12_381::g1_add::PRECOMPILE.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark BLS12-381 G1ADD precompile.
fn bench_bls12_381_g1add_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_bls12_381_g1add");
    let bytecode = generate_bls12_381_g1add_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!(
        "\n=== Gas consumption for precompile_bls12_381_g1add ({}x iterations) ===",
        ITERATIONS
    );
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// BLS12-381 G1MSM Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls BLS12-381 G1MSM precompile.
fn generate_bls12_381_g1msm_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // G1MSM input: G1 point (128 bytes) + scalar (32 bytes) = 160 bytes per pair
    // Official EIP-2537 test vector (double-and-add worst case)
    let g1_msm_input = bytes!("0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e1ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");

    // Store the input
    builder = builder.mstore(0, &g1_msm_input);

    for _ in 0..iterations {
        builder = builder.push_number(128_u64); // retSize (one G1 point)
        builder = builder.push_number(160_u64); // retOffset
        builder = builder.push_number(160_u64); // argsSize (one G1 point + scalar)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*bls12_381::g1_msm::PRECOMPILE.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark BLS12-381 G1MSM precompile.
fn bench_bls12_381_g1msm_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_bls12_381_g1msm");
    let bytecode = generate_bls12_381_g1msm_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!(
        "\n=== Gas consumption for precompile_bls12_381_g1msm ({}x iterations) ===",
        ITERATIONS
    );
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// BLS12-381 G2ADD Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls BLS12-381 G2ADD precompile.
fn generate_bls12_381_g2add_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // G2ADD input: two G2 points (256 bytes each = 512 bytes total)
    // Official EIP-2537 test vector
    let g2_add_input = bytes!("0000000000000000000000000000000018c0ada6351b70661f053365deae56910798bd2ace6e2bf6ba4192d1a229967f6af6ca1c9a8a11ebc0a232344ee0f6d6000000000000000000000000000000000cc70a587f4652039d8117b6103858adcd9728f6aebe230578389a62da0042b7623b1c0436734f463cfdd187d20903240000000000000000000000000000000009f50bd7beedb23328818f9ffdafdb6da6a4dd80c5a9048ab8b154df3cad938ccede829f1156f769d9e149791e8e0cd900000000000000000000000000000000079ba50d2511631b20b6d6f3841e616e9d11b68ec3368cd60129d9d4787ab56c4e9145a38927e51c9cd6271d493d938800000000000000000000000000000000192fa5d8732ff9f38e0b1cf12eadfd2608f0c7a39aced7746837833ae253bb57ef9c0d98a4b69eeb2950901917e99d1e0000000000000000000000000000000009aeb10c372b5ef1010675c6a4762fda33636489c23b581c75220589afbc0cc46249f921eea02dd1b761e036ffdbae220000000000000000000000000000000002d225447600d49f932b9dd3ca1e6959697aa603e74d8666681a2dca8160c3857668ae074440366619eb8920256c4e4a00000000000000000000000000000000174882cdd3551e0ce6178861ff83e195fecbcffd53a67b6f10b4431e423e28a480327febe70276036f60bb9c99cf7633");

    // Store the input
    builder = builder.mstore(0, &g2_add_input);

    for _ in 0..iterations {
        builder = builder.push_number(256_u64); // retSize (one G2 point)
        builder = builder.push_number(512_u64); // retOffset
        builder = builder.push_number(512_u64); // argsSize (two G2 points)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*bls12_381::g2_add::PRECOMPILE.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark BLS12-381 G2ADD precompile.
fn bench_bls12_381_g2add_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_bls12_381_g2add");
    let bytecode = generate_bls12_381_g2add_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!(
        "\n=== Gas consumption for precompile_bls12_381_g2add ({}x iterations) ===",
        ITERATIONS
    );
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// BLS12-381 G2MSM Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls BLS12-381 G2MSM precompile.
fn generate_bls12_381_g2msm_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // G2MSM input: G2 point (256 bytes) + scalar (32 bytes) = 288 bytes per pair
    // Official EIP-2537 test vector (double-and-add worst case)
    let g2_msm_input = bytes!("00000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801000000000000000000000000000000000606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79beffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");

    // Store the input
    builder = builder.mstore(0, &g2_msm_input);

    for _ in 0..iterations {
        builder = builder.push_number(256_u64); // retSize (one G2 point)
        builder = builder.push_number(288_u64); // retOffset
        builder = builder.push_number(288_u64); // argsSize (one G2 point + scalar)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*bls12_381::g2_msm::PRECOMPILE.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark BLS12-381 G2MSM precompile.
fn bench_bls12_381_g2msm_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_bls12_381_g2msm");
    let bytecode = generate_bls12_381_g2msm_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!(
        "\n=== Gas consumption for precompile_bls12_381_g2msm ({}x iterations) ===",
        ITERATIONS
    );
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// BLS12-381 PAIRING Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls BLS12-381 PAIRING precompile.
fn generate_bls12_381_pairing_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // PAIRING input: 2 pairs (G1 point + G2 point) * 2 = 768 bytes
    // Official EIP-2537 test vector for 2 pairs
    let pairing_input = bytes!("0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e100000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801000000000000000000000000000000000606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79be0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e100000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb80000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e000000000000000000000000000000000ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801000000000000000000000000000000000606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79be");

    // Store the input
    builder = builder.mstore(0, &pairing_input);

    for _ in 0..iterations {
        builder = builder.push_number(32_u64); // retSize (returns 32 bytes: 0 or 1)
        builder = builder.push_number(768_u64); // retOffset
        builder = builder.push_number(768_u64); // argsSize (2 pairs = 768 bytes)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*bls12_381::pairing::PRECOMPILE.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark BLS12-381 PAIRING precompile.
fn bench_bls12_381_pairing_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_bls12_381_pairing");
    let bytecode = generate_bls12_381_pairing_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!(
        "\n=== Gas consumption for precompile_bls12_381_pairing ({}x iterations) ===",
        ITERATIONS
    );
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// BLS12-381 MAP_FP_TO_G1 Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls BLS12-381 `MAP_FP_TO_G1` precompile.
fn generate_bls12_381_map_fp_to_g1_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // MAP_FP_TO_G1 input: field element (64 bytes padded)
    // Using value 1 with proper padding (16 bytes padding + 48 bytes data)
    let mut fp_element = vec![0u8; 64];
    fp_element[63] = 1; // Set to 1

    // Store field element
    builder = builder.mstore(0, &fp_element);

    for _ in 0..iterations {
        builder = builder.push_number(128_u64); // retSize (one G1 point)
        builder = builder.push_number(64_u64); // retOffset
        builder = builder.push_number(64_u64); // argsSize (one field element)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*bls12_381::map_fp_to_g1::PRECOMPILE.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark BLS12-381 `MAP_FP_TO_G1` precompile.
fn bench_bls12_381_map_fp_to_g1_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_bls12_381_map_fp_to_g1");
    let bytecode = generate_bls12_381_map_fp_to_g1_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!(
        "\n=== Gas consumption for precompile_bls12_381_map_fp_to_g1 ({}x iterations) ===",
        ITERATIONS
    );
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// BLS12-381 MAP_FP2_TO_G2 Precompile Benchmark
// ============================================================================
//

/// Generate bytecode that calls BLS12-381 `MAP_FP2_TO_G2` precompile.
fn generate_bls12_381_map_fp2_to_g2_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();

    // MAP_FP2_TO_G2 input: Fp2 element (128 bytes padded = 2 * 64 bytes)
    // Using a simple valid Fp2 element: (1, 0) with proper padding
    // Each component is 64 bytes: 16 bytes padding + 48 bytes data
    let mut fp2_element = vec![0u8; 128];
    // Set first component to 1 (at byte 63, the last byte of first 64-byte chunk)
    fp2_element[63] = 1;

    // Store Fp2 element
    builder = builder.mstore(0, &fp2_element);

    for _ in 0..iterations {
        builder = builder.push_number(256_u64); // retSize (one G2 point)
        builder = builder.push_number(128_u64); // retOffset
        builder = builder.push_number(128_u64); // argsSize (one Fp2 element)
        builder = builder.push_number(0_u64); // argsOffset
        builder = builder.push_address(*bls12_381::map_fp2_to_g2::PRECOMPILE.address());
        builder = builder.append(GAS); // gas
        builder = builder.append(STATICCALL);
        // assert call success
        builder = builder.assert_stack_value(0, U256::from(1));
        builder = builder.append(POP);
    }

    builder.build()
}

/// Benchmark BLS12-381 `MAP_FP2_TO_G2` precompile.
fn bench_bls12_381_map_fp2_to_g2_precompile(c: &mut Criterion) {
    const ITERATIONS: usize = 100;

    let mut group = c.benchmark_group("precompile_bls12_381_map_fp2_to_g2");
    let bytecode = generate_bls12_381_map_fp2_to_g2_bytecode(ITERATIONS);

    // Run once to collect gas consumption before benchmarking
    println!(
        "\n=== Gas consumption for precompile_bls12_381_map_fp2_to_g2 ({}x iterations) ===",
        ITERATIONS
    );
    for &(spec_name, spec) in SPEC_IDS {
        let gas_used = execute_and_get_gas(&bytecode, spec);
        println!("  {} spec: {} gas", spec_name, gas_used);
    }
    println!();

    for &(spec_name, spec) in SPEC_IDS {
        group.bench_function(spec_name, |b| {
            b.iter(|| black_box(execute_bytecode(&bytecode, black_box(spec))))
        });
    }

    group.finish();
}

//
// ============================================================================
// Criterion Configuration
// ============================================================================
//

// criterion_group!(
//     benches,
//     bench_add_opcode,
//     bench_address_opcode,
//     bench_sdiv_opcode,
//     bench_tstore_opcode,
//     bench_keccak256_opcode,
//     bench_exp_opcode,
//     bench_ecrecover_precompile,
//     bench_sha256_precompile,
//     bench_ripemd160_precompile,
//     bench_modexp_precompile,
//     bench_ecadd_precompile,
//     bench_ecmul_precompile,
//     bench_ecpairing_precompile,
//     bench_blake2f_precompile,
//     bench_kzg_point_evaluation_precompile,
//     bench_bls12_381_g1add_precompile,
//     bench_bls12_381_g1msm_precompile,
//     bench_bls12_381_g2add_precompile,
//     bench_bls12_381_g2msm_precompile,
//     bench_bls12_381_pairing_precompile,
//     bench_bls12_381_map_fp_to_g1_precompile,
//     bench_bls12_381_map_fp2_to_g2_precompile,
// );
// criterion_group!(
//     benches,
//     // Arithmetic operations
//     bench_add_opcode,
//     bench_mul_opcode,
//     bench_sub_opcode,
//     bench_div_opcode,
//     bench_sdiv_opcode,
//     bench_mod_opcode,
//     bench_smod_opcode,
//     bench_addmod_opcode,
//     bench_mulmod_opcode,
//     bench_exp_opcode,
//     bench_signextend_opcode,
//     bench_stop_opcode,
//     // Comparison & bitwise operations
//     bench_lt_opcode,
//     bench_gt_opcode,
//     bench_slt_opcode,
//     bench_sgt_opcode,
//     bench_eq_opcode,
//     bench_iszero_opcode,
//     bench_and_opcode,
//     bench_or_opcode,
//     bench_xor_opcode,
//     bench_not_opcode,
//     bench_byte_opcode,
//     bench_shl_opcode,
//     bench_shr_opcode,
//     bench_sar_opcode,
//     bench_clz_opcode,
// );
criterion_group!(
    benches,
    // System opcodes
    bench_address_opcode,
    bench_balance_opcode,
    bench_origin_opcode,
    bench_caller_opcode,
    bench_callvalue_opcode,
    bench_calldataload_opcode,
    bench_calldatasize_opcode,
    bench_calldatacopy_opcode,
    bench_codesize_opcode,
    bench_codecopy_opcode,
    bench_gasprice_opcode,
    bench_blobhash_opcode,
    // Host opcodes
    bench_extcodesize_opcode,
    bench_extcodecopy_opcode,
    bench_extcodehash_opcode,
    bench_blockhash_opcode,
    bench_selfbalance_opcode,
    bench_sload_opcode,
    bench_sstore_opcode,
    bench_tload_opcode,
    // Block opcodes
    bench_coinbase_opcode,
    bench_timestamp_opcode,
    bench_number_opcode,
    bench_difficulty_opcode,
    bench_gaslimit_opcode,
    bench_chainid_opcode,
    bench_basefee_opcode,
    bench_blobbasefee_opcode,
    // Memory opcodes
    bench_pop_opcode,
    bench_mload_opcode,
    bench_mstore_opcode,
    bench_mstore8_opcode,
    bench_msize_opcode,
    bench_mcopy_opcode,
    bench_returndatasize_opcode,
    bench_returndatacopy_opcode,
    // Control flow opcodes
    bench_pc_opcode,
    bench_jumpdest_opcode,
    bench_gas_opcode,
);
criterion_main!(benches);
