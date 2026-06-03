//! Benchmarks for mega-evm specific features.
//!
//! These benchmarks exercise code paths unique to mega-evm:
//! - **`volatile_data`**: Gas detention from block env opcodes (COINBASE, TIMESTAMP, etc.)
//! - **`gas_detention_computation`**: Impact of gas detention on subsequent heavy computation
//! - **`log_opcodes`**: LOG0-LOG4 with dual gas model (compute + storage gas)
//! - **`sstore_heavy`**: SSTORE-intensive workloads triggering resource limit tracking
//! - **`system_contract_single` / `system_contract_100x`**: System contract call interception
//!   overhead
//! - **`delegatecall_system_contract`**: DELEGATECALL vs CALL to system contracts
//! - **`oracle_sload`**: Oracle forced-cold SLOAD vs regular SLOAD
//! - **`create_deploy`**: CREATE/CREATE2 contract deployment with resource tracking
//! - **`selfdestruct`**: SELFDESTRUCT behavior across specs
//! - **`call_value_empty_account`**: CALL with value to empty accounts (dynamic gas)
//! - **`mixed_workload`**: Realistic combined workload

#![allow(missing_docs)]

use alloy_primitives::{address, Address, Bytes, U256};
use criterion::{criterion_group, criterion_main, Criterion};
use mega_evm::{test_utils::BytecodeBuilder, MegaSpecId};
use revm::bytecode::opcode::{
    ADD, CALL, COINBASE, CREATE, CREATE2, DELEGATECALL, GAS, LOG0, LOG1, LOG2, LOG4, NUMBER, POP,
    PUSH0, SELFDESTRUCT, SLOAD, SSTORE, STATICCALL, STOP, TIMESTAMP,
};

mod common;
use common::{
    register_all, register_all_suffixed, register_mega_specs, register_mega_specs_suffixed,
    register_mega_suffixed, Account, TxSpec, Workload,
};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");
const SECONDARY: Address = address!("0000000000000000000000000000000000100003");

/// Gas limit shared by every mega-feature workload — high enough that the
/// bench never halts on the EVM gas limit, leaving mega's own resource limits
/// as the only relevant ceiling.
const FEATURE_GAS_LIMIT: u64 = 10_000_000_000;

/// A funded `CALLER` calling a contract at `CONTRACT` holding `code`, under the
/// shared feature gas limit with empty calldata. Covers most mega benches; the
/// few with extra accounts or a different target build their `Workload` inline.
fn mega_contract_workload(code: Bytes) -> Workload {
    Workload::single(
        vec![
            Account::new(CONTRACT).code(code),
            Account::new(CALLER).balance(U256::from(10).pow(U256::from(18))),
        ],
        TxSpec::call(CALLER, CONTRACT).gas_limit(FEATURE_GAS_LIMIT),
    )
}

//
// ============================================================================
// Volatile Data Access Benchmarks
// ============================================================================
//

const VOLATILE_ITERATIONS: usize = 100;

fn generate_baseline_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..iterations {
        builder = builder.push_number(1u64).push_number(2u64).append(ADD).append(POP);
    }
    builder.build()
}

fn generate_coinbase_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..iterations {
        builder = builder.append(COINBASE).append(POP);
    }
    builder.build()
}

fn generate_timestamp_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..iterations {
        builder = builder.append(TIMESTAMP).append(POP);
    }
    builder.build()
}

fn generate_number_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..iterations {
        builder = builder.append(NUMBER).append(POP);
    }
    builder.build()
}

fn bench_volatile_data(c: &mut Criterion) {
    let variants: &[(&str, Bytes)] = &[
        ("baseline_add", generate_baseline_bytecode(VOLATILE_ITERATIONS)),
        ("coinbase", generate_coinbase_bytecode(VOLATILE_ITERATIONS)),
        ("timestamp", generate_timestamp_bytecode(VOLATILE_ITERATIONS)),
        ("number", generate_number_bytecode(VOLATILE_ITERATIONS)),
    ];

    let mut group = c.benchmark_group("volatile_data");
    for (variant, bytecode) in variants {
        register_mega_suffixed(&mut group, variant, &mega_contract_workload(bytecode.clone()));
    }
    group.finish();
}

//
// ============================================================================
// Gas Detention + Computation Benchmark
// ============================================================================
//
// Measures the impact of gas detention on subsequent heavy computation.
// After accessing volatile data, every subsequent opcode must check the
// detained compute gas limit. This benchmark isolates that overhead.
//

fn bench_gas_detention_computation(c: &mut Criterion) {
    // Heavy computation only (500 ADD ops) — no volatile access
    let computation_only = {
        let mut builder = BytecodeBuilder::default();
        for _ in 0..500 {
            builder = builder.push_number(1u64).push_number(2u64).append(ADD).append(POP);
        }
        builder.build()
    };

    // Volatile access THEN heavy computation (COINBASE + 500 ADD ops)
    // Every ADD after COINBASE must check the detained gas cap.
    let volatile_then_compute = {
        let mut builder = BytecodeBuilder::default();
        builder = builder.append(COINBASE).append(POP);
        for _ in 0..500 {
            builder = builder.push_number(1u64).push_number(2u64).append(ADD).append(POP);
        }
        builder.build()
    };

    let mut group = c.benchmark_group("gas_detention_computation");
    register_mega_suffixed(
        &mut group,
        "compute_only_500",
        &mega_contract_workload(computation_only),
    );
    register_mega_suffixed(
        &mut group,
        "volatile_then_compute_500",
        &mega_contract_workload(volatile_then_compute),
    );
    group.finish();
}

//
// ============================================================================
// LOG Opcode Benchmarks
// ============================================================================
//

const LOG_ITERATIONS: usize = 50;

fn generate_log0_bytecode(iterations: usize, data_size: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..iterations {
        builder = builder.push_number(data_size as u64).push_number(0u64).append(LOG0);
    }
    builder.build()
}

fn generate_log2_bytecode(iterations: usize, data_size: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..iterations {
        builder = builder
            .push_number(0xdead_beef_u64)
            .push_number(0xcafe_babe_u64)
            .push_number(data_size as u64)
            .push_number(0u64)
            .append(LOG2);
    }
    builder.build()
}

fn generate_log4_bytecode(iterations: usize, data_size: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for _ in 0..iterations {
        builder = builder
            .push_number(0x1111_u64)
            .push_number(0x2222_u64)
            .push_number(0x3333_u64)
            .push_number(0x4444_u64)
            .push_number(data_size as u64)
            .push_number(0u64)
            .append(LOG4);
    }
    builder.build()
}

fn bench_log_opcodes(c: &mut Criterion) {
    let variants: &[(&str, Bytes)] = &[
        ("log0_32b", generate_log0_bytecode(LOG_ITERATIONS, 32)),
        ("log0_256b", generate_log0_bytecode(LOG_ITERATIONS, 256)),
        ("log2_32b", generate_log2_bytecode(LOG_ITERATIONS, 32)),
        ("log4_32b", generate_log4_bytecode(LOG_ITERATIONS, 32)),
        ("log4_256b", generate_log4_bytecode(LOG_ITERATIONS, 256)),
    ];

    let mut group = c.benchmark_group("log_opcodes");
    for (variant, bytecode) in variants {
        register_all_suffixed(&mut group, variant, &mega_contract_workload(bytecode.clone()));
    }
    group.finish();
}

//
// ============================================================================
// SSTORE Heavy Benchmarks
// ============================================================================
//

const SSTORE_ITERATIONS: usize = 100;

fn generate_sstore_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for i in 0..iterations {
        builder = builder.push_number(i as u64 + 1).push_number(i as u64).append(SSTORE);
    }
    builder.build()
}

fn generate_sload_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for i in 0..iterations {
        builder = builder.push_number(i as u64).append(SLOAD).append(POP);
    }
    builder.build()
}

fn generate_sstore_sload_bytecode(iterations: usize) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for i in 0..iterations {
        builder = builder
            .push_number(i as u64 + 1)
            .push_number(i as u64)
            .append(SSTORE)
            .push_number(i as u64)
            .append(SLOAD)
            .append(POP);
    }
    builder.build()
}

fn bench_sstore(c: &mut Criterion) {
    let variants: &[(&str, Bytes)] = &[
        ("sstore_100", generate_sstore_bytecode(SSTORE_ITERATIONS)),
        ("sload_100", generate_sload_bytecode(SSTORE_ITERATIONS)),
        ("sstore_sload_100", generate_sstore_sload_bytecode(SSTORE_ITERATIONS)),
    ];

    let mut group = c.benchmark_group("sstore_heavy");
    for (variant, bytecode) in variants {
        register_all_suffixed(&mut group, variant, &mega_contract_workload(bytecode.clone()));
    }
    group.finish();
}

//
// ============================================================================
// CREATE / CREATE2 Contract Deployment Benchmarks
// ============================================================================
//
// Exercises:
// - forward_gas_ext (98/100 gas forwarding rule)
// - storage_gas_ext (SALT dynamic gas for new account/contract)
// - State growth tracking
// - Data size tracking (deployed code size)
//

/// Build bytecode that deploys a minimal contract via CREATE.
/// The init code stores a small runtime bytecode and returns it.
fn make_create_bytecode(n_deploys: usize) -> Bytes {
    // Init code that deploys an empty contract: PUSH1 0x00 PUSH1 0x00 RETURN.
    let init_code: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xf3];

    let mut builder = BytecodeBuilder::default();
    builder = builder.mstore(0, init_code);

    for _ in 0..n_deploys {
        builder = builder
            .push_number(5u64) // size of init code
            .push_number(0u64) // memory offset
            .push_number(0u64) // value
            .append(CREATE)
            .append(POP); // discard deployed address
    }
    builder.build()
}

/// Build bytecode that deploys a minimal contract via CREATE2 with varying salts.
fn make_create2_bytecode(n_deploys: usize) -> Bytes {
    let init_code: [u8; 5] = [0x60, 0x00, 0x60, 0x00, 0xf3];

    let mut builder = BytecodeBuilder::default();
    builder = builder.mstore(0, init_code);

    for i in 0..n_deploys {
        builder = builder
            .push_number(i as u64) // salt (different each time)
            .push_number(5u64) // size
            .push_number(0u64) // offset
            .push_number(0u64) // value
            .append(CREATE2)
            .append(POP);
    }
    builder.build()
}

fn bench_create_deploy(c: &mut Criterion) {
    let variants: &[(&str, Bytes)] =
        &[("create_10", make_create_bytecode(10)), ("create2_10", make_create2_bytecode(10))];

    let mut group = c.benchmark_group("create_deploy");
    group.sample_size(10);

    for (variant, bytecode) in variants {
        register_all_suffixed(&mut group, variant, &mega_contract_workload(bytecode.clone()));
    }
    group.finish();
}

//
// ============================================================================
// SELFDESTRUCT Benchmark
// ============================================================================
//
// SELFDESTRUCT behavior varies by spec:
// - EQUIVALENCE: Standard EIP-6780 behavior
// - MINI_REX ~ REX1: Disabled (halts execution)
// - REX2+: Re-enabled with EIP-6780 semantics
// - REX4+: Also marks beneficiary balance access for gas detention
//

fn bench_selfdestruct(c: &mut Criterion) {
    // SELFDESTRUCT(target) — send remaining balance to target
    let selfdestruct_code: Bytes = vec![PUSH0, SELFDESTRUCT].into();

    // Specs where SELFDESTRUCT is allowed
    let selfdestruct_specs: &[(&str, MegaSpecId)] = &[
        ("equivalence", MegaSpecId::EQUIVALENCE),
        ("rex2", MegaSpecId::REX2),
        ("rex4", MegaSpecId::REX4),
    ];

    let mut group = c.benchmark_group("selfdestruct");
    // A subject rebuilds the DB from the workload each iteration (SELFDESTRUCT
    // modifies state), and `allow_halt` covers the specs where it is disabled.
    let workload = mega_contract_workload(selfdestruct_code).allow_halt();
    register_mega_specs(&mut group, selfdestruct_specs, &workload);
    group.finish();
}

//
// ============================================================================
// CALL with Value to Empty Account Benchmark
// ============================================================================
//
// When CALL transfers value to an empty (non-existent) account, mega-evm
// charges dynamic account creation gas via SALT bucket pricing.
// This benchmark compares:
// - CALL with value to existing account (no creation cost)
// - CALL with value to empty account (triggers account creation gas)
//

fn bench_call_value_empty_account(c: &mut Criterion) {
    let existing_target: Address = address!("cccccccccccccccccccccccccccccccccccccccc");
    let empty_target: Address = address!("dddddddddddddddddddddddddddddddddddddddd");

    // Build bytecode: CALL(gas, target, value=1wei, argsOff=0, argsLen=0, retOff=0, retLen=0)
    let make_call_with_value = |target: Address, n: usize| -> Bytes {
        let mut builder = BytecodeBuilder::default();
        for _ in 0..n {
            builder = builder
                .push_number(0u64) // retSize
                .push_number(0u64) // retOffset
                .push_number(0u64) // argsSize
                .push_number(0u64) // argsOffset
                .push_number(1u64) // value = 1 wei
                .push_address(target)
                .append(GAS)
                .append(CALL)
                .append(POP);
        }
        builder.build()
    };

    let call_existing = make_call_with_value(existing_target, 50);
    let call_empty = make_call_with_value(empty_target, 50);

    let mut group = c.benchmark_group("call_value_empty_account");

    // Variant: existing target has a balance; CALL hits an existing account.
    let existing = Workload::single(
        vec![
            Account::new(CONTRACT).code(call_existing),
            Account::new(CALLER).balance(U256::from(10).pow(U256::from(18))),
            Account::new(existing_target).balance(U256::from(1)),
        ],
        TxSpec::call(CALLER, CONTRACT).gas_limit(FEATURE_GAS_LIMIT),
    );
    register_all_suffixed(&mut group, "existing_account_50", &existing);

    // Variant: empty target — CALL with value triggers account creation gas.
    register_all_suffixed(&mut group, "empty_account_50", &mega_contract_workload(call_empty));

    group.finish();
}

//
// ============================================================================
// System Contract Interception Benchmarks
// ============================================================================
//

/// Oracle contract address.
const ORACLE_ADDRESS: Address = address!("6342000000000000000000000000000000000001");

fn make_staticcall_bytecode(target: Address, selector: [u8; 4]) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    builder = builder.mstore(0, selector);
    builder = builder
        .push_number(32u64)
        .push_number(32u64)
        .push_number(4u64)
        .push_number(0u64)
        .push_address(target)
        .append(GAS)
        .append(STATICCALL)
        .append(POP);
    builder.build()
}

fn make_repeated_staticcall_bytecode(
    target: Address,
    selector: [u8; 4],
    iterations: usize,
) -> Bytes {
    let mut builder = BytecodeBuilder::default();
    builder = builder.mstore(0, selector);
    for _ in 0..iterations {
        builder = builder
            .push_number(32u64)
            .push_number(32u64)
            .push_number(4u64)
            .push_number(0u64)
            .push_address(target)
            .append(GAS)
            .append(STATICCALL)
            .append(POP);
    }
    builder.build()
}

fn bench_system_contract(c: &mut Criterion) {
    let access_control_addr: Address = address!("6342000000000000000000000000000000000004");
    let is_disabled_selector: [u8; 4] = [0x9e, 0x8e, 0x7b, 0xc0];
    let limit_control_addr: Address = address!("6342000000000000000000000000000000000005");
    let remaining_gas_selector: [u8; 4] = [0xde, 0x85, 0xee, 0xf5];
    let empty_contract_code = Bytes::from_static(&[STOP]);
    let rex4: &[(&str, MegaSpecId)] = &[("rex4", MegaSpecId::REX4)];

    // A `CONTRACT` that STATICCALLs `SECONDARY` (a regular contract), used as
    // the non-system-contract baseline for the interception comparison.
    let regular_workload = |code: Bytes| {
        Workload::single(
            vec![
                Account::new(CONTRACT).code(code),
                Account::new(CALLER).balance(U256::from(10).pow(U256::from(18))),
                Account::new(SECONDARY).code(empty_contract_code.clone()),
            ],
            TxSpec::call(CALLER, CONTRACT).gas_limit(FEATURE_GAS_LIMIT),
        )
    };

    // Single call benchmarks
    {
        let mut group = c.benchmark_group("system_contract_single");
        register_mega_specs_suffixed(
            &mut group,
            rex4,
            "access_control",
            &mega_contract_workload(make_staticcall_bytecode(
                access_control_addr,
                is_disabled_selector,
            )),
        );
        register_mega_specs_suffixed(
            &mut group,
            rex4,
            "limit_control",
            &mega_contract_workload(make_staticcall_bytecode(
                limit_control_addr,
                remaining_gas_selector,
            )),
        );
        register_mega_specs_suffixed(
            &mut group,
            rex4,
            "regular_contract",
            &regular_workload(make_staticcall_bytecode(SECONDARY, [0x00, 0x00, 0x00, 0x00])),
        );
        group.finish();
    }

    // Repeated calls (100 iterations)
    {
        let n = 100;
        let mut group = c.benchmark_group("system_contract_100x");
        register_mega_specs_suffixed(
            &mut group,
            rex4,
            "access_control",
            &mega_contract_workload(make_repeated_staticcall_bytecode(
                access_control_addr,
                is_disabled_selector,
                n,
            )),
        );
        register_mega_specs_suffixed(
            &mut group,
            rex4,
            "limit_control",
            &mega_contract_workload(make_repeated_staticcall_bytecode(
                limit_control_addr,
                remaining_gas_selector,
                n,
            )),
        );
        register_mega_specs_suffixed(
            &mut group,
            rex4,
            "regular_contract",
            &regular_workload(make_repeated_staticcall_bytecode(
                SECONDARY,
                [0x00, 0x00, 0x00, 0x00],
                n,
            )),
        );
        group.finish();
    }
}

//
// ============================================================================
// DELEGATECALL to System Contract Benchmark
// ============================================================================
//
// System contracts are intercepted on CALL but NOT on DELEGATECALL.
// DELEGATECALL falls through to on-chain bytecode (or reverts if no code).
// This is a security-relevant boundary.
//

fn bench_delegatecall_system_contract(c: &mut Criterion) {
    let access_control_addr: Address = address!("6342000000000000000000000000000000000004");
    let is_disabled_selector: [u8; 4] = [0x9e, 0x8e, 0x7b, 0xc0];
    let rex4: &[(&str, MegaSpecId)] = &[("rex4", MegaSpecId::REX4)];

    // STATICCALL to system contract — intercepted, returns result
    let staticcall_code = make_staticcall_bytecode(access_control_addr, is_disabled_selector);

    // DELEGATECALL to system contract — NOT intercepted, falls through
    let delegatecall_code = {
        let mut builder = BytecodeBuilder::default();
        builder = builder.mstore(0, is_disabled_selector);
        // DELEGATECALL(gas, target, argsOffset, argsSize, retOffset, retSize)
        builder = builder
            .push_number(32u64) // retSize
            .push_number(32u64) // retOffset
            .push_number(4u64) // argsSize
            .push_number(0u64) // argsOffset
            .push_address(access_control_addr)
            .append(GAS)
            .append(DELEGATECALL)
            .append(POP);
        builder.build()
    };

    let mut group = c.benchmark_group("delegatecall_system_contract");
    register_mega_specs_suffixed(
        &mut group,
        rex4,
        "staticcall_intercepted",
        &mega_contract_workload(staticcall_code),
    );
    // DELEGATECALL falls through to absent bytecode and may halt, so allow it.
    register_mega_specs_suffixed(
        &mut group,
        rex4,
        "delegatecall_not_intercepted",
        &mega_contract_workload(delegatecall_code).allow_halt(),
    );
    group.finish();
}

//
// ============================================================================
// Oracle SLOAD Benchmark
// ============================================================================
//

fn bench_oracle_sload(c: &mut Criterion) {
    let sload_bytecode = generate_sload_bytecode(50);

    let mut group = c.benchmark_group("oracle_sload");
    // Regular SLOAD against a normal contract.
    register_mega_suffixed(
        &mut group,
        "regular_sload_50",
        &mega_contract_workload(sload_bytecode.clone()),
    );
    // Oracle SLOAD: the same bytecode runs at the oracle address, where reads
    // are forced cold.
    let oracle = Workload::single(
        vec![
            Account::new(ORACLE_ADDRESS).code(sload_bytecode),
            Account::new(CALLER).balance(U256::from(10).pow(U256::from(18))),
        ],
        TxSpec::call(CALLER, ORACLE_ADDRESS).gas_limit(FEATURE_GAS_LIMIT),
    );
    register_mega_suffixed(&mut group, "oracle_sload_50", &oracle);
    group.finish();
}

//
// ============================================================================
// Mixed Workload Benchmark
// ============================================================================
//

fn generate_mixed_workload_bytecode() -> Bytes {
    let mut builder = BytecodeBuilder::default();
    // Volatile data access
    builder = builder.append(TIMESTAMP).append(POP).append(COINBASE).append(POP);
    // Storage writes
    for i in 0..20u64 {
        builder = builder.push_number(i + 1).push_number(i).append(SSTORE);
    }
    // Log emissions
    for _ in 0..10 {
        builder =
            builder.push_number(0xdead_beef_u64).push_number(32u64).push_number(0u64).append(LOG1);
    }
    // Computation
    for _ in 0..50 {
        builder = builder.push_number(1u64).push_number(2u64).append(ADD).append(POP);
    }
    builder.build()
}

fn bench_mixed_workload(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_workload");
    register_all(&mut group, &mega_contract_workload(generate_mixed_workload_bytecode()));
    group.finish();
}

criterion_group!(
    benches,
    bench_volatile_data,
    bench_gas_detention_computation,
    bench_log_opcodes,
    bench_sstore,
    bench_create_deploy,
    bench_selfdestruct,
    bench_call_value_empty_account,
    bench_system_contract,
    bench_delegatecall_system_contract,
    bench_oracle_sload,
    bench_mixed_workload,
);
criterion_main!(benches);
