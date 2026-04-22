//! Benchmarks for the block executor pipeline.
//!
//! Measures the full production path: `pre_execution_changes` (system contract
//! deployment), transaction execution, and `post_execution_changes`.

#![allow(missing_docs)]

use std::convert::Infallible;

use alloy_consensus::{Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_hardforks::ForkCondition;
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaHardfork,
    MegaHardforkConfig, MegaSpecId, MegaTxEnvelope, TestExternalEnvs, ACCESS_CONTROL_ADDRESS,
    ACCESS_CONTROL_CODE, HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
    HIGH_PRECISION_TIMESTAMP_ORACLE_CODE, KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE,
    LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE, MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
    ORACLE_CONTRACT_CODE_REX5, SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE,
};
use mega_system_contracts::sequencer_registry::storage_slots::{
    ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, INITIAL_FROM_BLOCK, INITIAL_SEQUENCER,
    INITIAL_SYSTEM_ADDRESS,
};
use revm::{
    bytecode::opcode::{ADD, LOG1, POP, SSTORE},
    context::BlockEnv,
    database::State,
};

const CALLER: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT: Address = address!("1000000000000000000000000000000000000001");

/// Create a recovered legacy transaction for block execution.
fn create_call_tx(
    nonce: u64,
    gas_limit: u64,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Call(CONTRACT),
        value: U256::ZERO,
        input: Bytes::new(),
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Create a contract deployment transaction.
fn create_deploy_tx(
    nonce: u64,
    gas_limit: u64,
    init_code: Bytes,
) -> alloy_consensus::transaction::Recovered<MegaTxEnvelope> {
    let tx_legacy = TxLegacy {
        chain_id: Some(8453),
        nonce,
        gas_price: 1_000_000,
        gas_limit,
        to: TxKind::Create,
        value: U256::ZERO,
        input: init_code,
    };
    let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), Default::default());
    let tx = MegaTxEnvelope::Legacy(signed);
    alloy_consensus::transaction::Recovered::new_unchecked(tx, CALLER)
}

/// Hardfork config activating all hardforks from genesis.
fn all_hardforks_config() -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(0))
        .with(MegaHardfork::Rex, ForkCondition::Timestamp(0))
        .with(MegaHardfork::Rex1, ForkCondition::Timestamp(0))
        .with(MegaHardfork::Rex2, ForkCondition::Timestamp(0))
        .with(MegaHardfork::Rex3, ForkCondition::Timestamp(0))
        .with(MegaHardfork::Rex4, ForkCondition::Timestamp(0))
        .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
}

/// Create block EVM environment.
fn block_evm_env(spec: MegaSpecId) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = spec;
    let block_env = BlockEnv {
        number: U256::from(1000),
        timestamp: U256::from(1_800_000_000),
        gas_limit: 30_000_000,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

fn rex5_steady_state_db(contract_code: &Bytes) -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_code(CONTRACT, contract_code.clone());
    db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

    // Predeploy all REX5 system contracts so this benchmark measures the steady-state
    // pre-block path instead of repeatedly paying first-deploy cost.
    db.set_account_code(ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE_REX5);
    db.set_account_code(
        HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
        HIGH_PRECISION_TIMESTAMP_ORACLE_CODE,
    );
    db.set_account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE);
    db.set_account_code(ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE);
    db.set_account_code(LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE);
    db.set_account_code(SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE);

    // Seed the bootstrap SequencerRegistry slots exactly once so deploy stays idempotent
    // and resolve_system_address reads committed steady-state data.
    let initial_address = U256::from_be_bytes(MEGA_SYSTEM_ADDRESS.into_word().0);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS, initial_address);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, CURRENT_SEQUENCER, initial_address);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, ADMIN, initial_address);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, INITIAL_SYSTEM_ADDRESS, initial_address);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, INITIAL_SEQUENCER, initial_address);
    db.set_account_storage(SEQUENCER_REGISTRY_ADDRESS, INITIAL_FROM_BLOCK, U256::from(1000));

    db
}

/// Simple contract: just STOPs.
fn empty_contract() -> Bytes {
    BytecodeBuilder::default().stop().build()
}

/// Contract that does 10 SSTOREs + 5 LOG1s + 50 ADDs.
fn mixed_contract() -> Bytes {
    let mut builder = BytecodeBuilder::default();
    for i in 0..10u64 {
        builder = builder.push_number(i + 1).push_number(i).append(SSTORE);
    }
    for _ in 0..5 {
        builder = builder.push_number(0xbeef_u64).push_number(32u64).push_number(0u64).append(LOG1);
    }
    for _ in 0..50 {
        builder = builder.push_number(1u64).push_number(2u64).append(ADD).append(POP);
    }
    builder.stop().build()
}

//
// ============================================================================
// Block Executor Pipeline Benchmarks
// ============================================================================
//

/// Benchmark a complete block with N empty transactions.
fn bench_block_empty_txs(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_executor_empty_txs");
    group.sample_size(10);

    let contract_code = empty_contract();
    let spec = MegaSpecId::REX4;

    for n_txs in [1, 10, 50] {
        group.bench_function(format!("rex4/{n_txs}_txs"), |b| {
            b.iter(|| {
                let mut db = MemoryDatabase::default();
                db.set_account_code(CONTRACT, contract_code.clone());
                db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

                let mut state = State::builder().with_database(&mut db).build();
                let external_envs = TestExternalEnvs::<Infallible>::new();
                let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
                let evm = evm_factory.create_evm(&mut state, block_evm_env(spec));

                let block_ctx = MegaBlockExecutionCtx::new(
                    B256::ZERO,
                    Some(B256::ZERO),
                    Bytes::new(),
                    BlockLimits::no_limits(),
                );
                let mut executor = MegaBlockExecutor::new(
                    evm,
                    block_ctx,
                    all_hardforks_config(),
                    OpAlloyReceiptBuilder::default(),
                );
                executor
                    .apply_pre_execution_changes()
                    .expect("pre-execution changes should succeed");

                for i in 0..n_txs {
                    let tx = create_call_tx(i, 1_000_000);
                    let gas = executor.execute_transaction(&tx).expect("should succeed");
                    black_box(gas);
                }

                let (_evm, block_result) = executor.finish().expect("finish should succeed");
                black_box(block_result);
            })
        });
    }
    group.finish();
}

/// Benchmark a complete block with N mixed workload transactions.
fn bench_block_mixed_txs(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_executor_mixed_txs");
    group.sample_size(10);

    let contract_code = mixed_contract();
    let spec = MegaSpecId::REX4;

    for n_txs in [1, 10] {
        group.bench_function(format!("rex4/{n_txs}_txs"), |b| {
            b.iter(|| {
                let mut db = MemoryDatabase::default();
                db.set_account_code(CONTRACT, contract_code.clone());
                db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

                let mut state = State::builder().with_database(&mut db).build();
                let external_envs = TestExternalEnvs::<Infallible>::new();
                let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
                let evm = evm_factory.create_evm(&mut state, block_evm_env(spec));

                let block_ctx = MegaBlockExecutionCtx::new(
                    B256::ZERO,
                    Some(B256::ZERO),
                    Bytes::new(),
                    BlockLimits::no_limits(),
                );
                let mut executor = MegaBlockExecutor::new(
                    evm,
                    block_ctx,
                    all_hardforks_config(),
                    OpAlloyReceiptBuilder::default(),
                );
                executor
                    .apply_pre_execution_changes()
                    .expect("pre-execution changes should succeed");

                for i in 0..n_txs {
                    let tx = create_call_tx(i, 10_000_000);
                    let gas = executor.execute_transaction(&tx).expect("should succeed");
                    black_box(gas);
                }

                let (_evm, block_result) = executor.finish().expect("finish should succeed");
                black_box(block_result);
            })
        });
    }
    group.finish();
}

/// Benchmark contract deployment through the block executor.
fn bench_block_deploy(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_executor_deploy");
    group.sample_size(10);

    // Init code that deploys a small runtime: PUSH1 0x00 PUSH1 0x00 RETURN
    let init_code: Bytes = vec![0x60, 0x00, 0x60, 0x00, 0xf3].into();
    let spec = MegaSpecId::REX4;

    group.bench_function("rex4/deploy_1", |b| {
        b.iter(|| {
            let mut db = MemoryDatabase::default();
            db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

            let mut state = State::builder().with_database(&mut db).build();
            let external_envs = TestExternalEnvs::<Infallible>::new();
            let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
            let evm = evm_factory.create_evm(&mut state, block_evm_env(spec));

            let block_ctx = MegaBlockExecutionCtx::new(
                B256::ZERO,
                Some(B256::ZERO),
                Bytes::new(),
                BlockLimits::no_limits(),
            );
            let mut executor = MegaBlockExecutor::new(
                evm,
                block_ctx,
                all_hardforks_config(),
                OpAlloyReceiptBuilder::default(),
            );
            executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");

            let nonce = 0u64;
            let tx = create_deploy_tx(nonce, 10_000_000, init_code.clone());
            let gas = executor.execute_transaction(&tx).expect("deploy should succeed");
            black_box(gas);

            let (_evm, block_result) = executor.finish().expect("finish should succeed");
            black_box(block_result);
        })
    });
    group.finish();
}

/// Benchmark spec comparison for block execution.
///
/// NOTE: All specs run with `all_hardforks_config()`, so even `EQUIVALENCE` and `MINI_REX`
/// deploy Rex4 system contracts during `pre_execution_changes`. This is intentional —
/// the benchmark isolates EVM execution behavior differences across specs, not system
/// contract deployment overhead.
fn bench_block_spec_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_executor_spec_comparison");
    group.sample_size(10);

    let contract_code = mixed_contract();
    let specs: &[(&str, MegaSpecId)] = &[
        ("equivalence", MegaSpecId::EQUIVALENCE),
        ("mini_rex", MegaSpecId::MINI_REX),
        ("rex4", MegaSpecId::REX4),
        ("rex5", MegaSpecId::REX5),
    ];

    for &(spec_name, spec) in specs {
        group.bench_function(format!("{spec_name}/5_mixed_txs"), |b| {
            b.iter(|| {
                let mut db = MemoryDatabase::default();
                db.set_account_code(CONTRACT, contract_code.clone());
                db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

                let mut state = State::builder().with_database(&mut db).build();
                let external_envs = TestExternalEnvs::<Infallible>::new();
                let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
                let evm = evm_factory.create_evm(&mut state, block_evm_env(spec));

                let block_ctx = MegaBlockExecutionCtx::new(
                    B256::ZERO,
                    Some(B256::ZERO),
                    Bytes::new(),
                    BlockLimits::no_limits(),
                );
                let mut executor = MegaBlockExecutor::new(
                    evm,
                    block_ctx,
                    all_hardforks_config(),
                    OpAlloyReceiptBuilder::default(),
                );
                executor
                    .apply_pre_execution_changes()
                    .expect("pre-execution changes should succeed");

                for i in 0..5u64 {
                    let tx = create_call_tx(i, 10_000_000);
                    let gas = executor.execute_transaction(&tx).expect("should succeed");
                    black_box(gas);
                }

                let (_evm, block_result) = executor.finish().expect("finish should succeed");
                black_box(block_result);
            })
        });
    }
    group.finish();
}

/// Benchmark the full REX5 pre-block path.
///
/// `bootstrap` starts from an empty DB and pays first-deploy cost for the REX5 system contracts.
/// `no_rotation` starts from a steady-state DB where all deploy-only contracts already exist and
/// the `SequencerRegistry` has no pending changes.
fn bench_rex5_pre_block(c: &mut Criterion) {
    let mut group = c.benchmark_group("rex5_pre_block");
    group.sample_size(10);

    let contract_code = empty_contract();
    let spec = MegaSpecId::REX5;

    // Bootstrap block: first REX5 pre-block on an empty state.
    group.bench_function("bootstrap", |b| {
        b.iter(|| {
            let mut db = MemoryDatabase::default();
            db.set_account_code(CONTRACT, contract_code.clone());
            db.set_account_balance(CALLER, U256::from(1_000_000_000_000_000u64));

            let mut state = State::builder().with_database(&mut db).build();
            let external_envs = TestExternalEnvs::<Infallible>::new();
            let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
            let evm = evm_factory.create_evm(&mut state, block_evm_env(spec));

            let block_ctx = MegaBlockExecutionCtx::new(
                B256::ZERO,
                Some(B256::ZERO),
                Bytes::new(),
                BlockLimits::no_limits(),
            );
            let mut executor = MegaBlockExecutor::new(
                evm,
                block_ctx,
                all_hardforks_config(),
                OpAlloyReceiptBuilder::default(),
            );
            executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");
            black_box(());
        })
    });

    // Steady-state block: all deploy-only system contracts already exist and the registry has no
    // pending changes.
    group.bench_function("no_rotation", |b| {
        let baseline_db = rex5_steady_state_db(&contract_code);

        b.iter(|| {
            let mut db = baseline_db.clone();
            let mut state = State::builder().with_database(&mut db).build();
            let external_envs = TestExternalEnvs::<Infallible>::new();
            let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
            let evm = evm_factory.create_evm(&mut state, block_evm_env(spec));
            let block_ctx = MegaBlockExecutionCtx::new(
                B256::ZERO,
                Some(B256::ZERO),
                Bytes::new(),
                BlockLimits::no_limits(),
            );
            let mut executor = MegaBlockExecutor::new(
                evm,
                block_ctx,
                all_hardforks_config(),
                OpAlloyReceiptBuilder::default(),
            );
            executor.apply_pre_execution_changes().expect("pre-execution changes should succeed");
            black_box(());
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_block_empty_txs,
    bench_block_mixed_txs,
    bench_block_deploy,
    bench_block_spec_comparison,
    bench_rex5_pre_block,
);
criterion_main!(benches);
