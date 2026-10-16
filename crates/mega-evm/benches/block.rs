//! A block through `MegaBlockExecutor`: what block execution costs per transaction on top of
//! what the transaction itself costs, in a chain's steady state.
//!
//! Every block runs as a node runs one:
//!
//! - on a state that already holds the seven predeploys, which a chain deploys once, in its first
//!   block after activation: the state the previous block's pre-block changes left;
//! - under the chain's default limits, `ProtocolLimits::DEFAULT`: the production data-size limits
//!   and gas detention's caps, with no building policy on top (`BlockLimits::default()`);
//! - from a fresh `State` over that database, as a node starts each block.
//!
//! Two workloads of `N` transactions each, whose per-transaction work beyond the transaction
//! itself is the executor's own: the admission checks, the size estimates, the limits, the
//! counters, the receipt and the commit.
//!
//! - `transfers`: value transfers to accounts with no code — the cheapest transaction there is, so
//!   the executor's own cost is most of what is measured.
//! - `storage_writes`: calls to a contract that writes the slot the transaction names in its
//!   calldata, so every transaction changes storage and carries a write record through the
//!   counters, with a log-free receipt.
//!
//! The measurement runs `apply_pre_execution_changes`, the transactions and `finish` — the whole
//! block a node executes — and hands the state back, so dropping it is not measured. Two more
//! arms measure `apply_pre_execution_changes` alone:
//!
//! - `steady_pre_block`: on the steady state, the per-block cost of the idempotent check of the
//!   predeploys and of the pre-block system calls;
//! - `activation_pre_block`: on a state without the predeploys, the first block after activation,
//!   which deploys all seven. A chain pays it once; it is the one arm that runs the deploys.
#![allow(missing_docs)]

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::{
    system::{SequencerRegistryConfig, SYSTEM_CONTRACT_DEPLOY_COUNT},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope, PreBlockStateSource, ProtocolLimits,
};
use revm::{
    bytecode::opcode::{CALLDATALOAD, SSTORE},
    context::{BlockEnv, CfgEnv},
    database::State,
    state::EvmState,
    DatabaseCommit,
};
use std::{
    hint::black_box,
    sync::{Arc, Mutex},
};

/// Transactions in the block.
const N: u64 = 64;

/// The chain the block belongs to.
const CHAIN_ID: u64 = 4_326;

/// The block the bench executes.
const BLOCK_NUMBER: u64 = 1_000;

/// The timestamp of the block the bench executes.
const BLOCK_TIMESTAMP: u64 = 1_800_000_000;

const SENDER: Address = address!("0x2000000000000000000000000000000000000002");
const RECIPIENT: Address = address!("0x3000000000000000000000000000000000000003");
const WRITER: Address = address!("0x1000000000000000000000000000000000000001");

/// What the pre-block changes of a block handed their observer, in the order the executor
/// commits them.
type PreBlockStates = Arc<Mutex<Vec<(PreBlockStateSource, EvmState)>>>;

/// Code that writes 1 to the slot its first calldata word names, so every call that names a slot
/// of its own changes storage and leaves a write record behind.
fn writer_code() -> Bytes {
    BytecodeBuilder::default()
        .push_number(1_u64)
        .push_number(0_u64)
        .append(CALLDATALOAD)
        .append(SSTORE)
        .stop()
        .build()
}

/// The slot the transaction with this nonce writes: one per transaction, so none of them writes
/// a value the slot already holds.
fn slot_calldata(nonce: u64) -> Bytes {
    Bytes::from(U256::from(nonce + 1).to_be_bytes::<32>())
}

/// The accounts the workloads use, and nothing the chain deploys.
fn database() -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_code(WRITER, writer_code());
    db.set_account_code(RECIPIENT, Bytes::new());
    db.set_account_balance(SENDER, U256::from(1_000_000_000_000_000_u64));
    db
}

/// The environment of block `number`, one second after the block before it.
fn evm_env_at(number: u64) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = CfgEnv::new_with_spec(MegaSpecId::SATIN);
    cfg_env.chain_id = CHAIN_ID;
    EvmEnv {
        cfg_env,
        block_env: BlockEnv {
            number: U256::from(number),
            timestamp: U256::from(BLOCK_TIMESTAMP + number - BLOCK_NUMBER),
            gas_limit: 1_000_000_000,
            ..Default::default()
        },
    }
}

/// The environment of the block the bench executes.
fn evm_env() -> EvmEnv<MegaSpecId> {
    evm_env_at(BLOCK_NUMBER)
}

/// A block packed under no building policy, as a validator executes one: the chain's limits
/// alone hold it.
fn block_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), BlockLimits::default())
}

/// `N` transactions to `to`, one per nonce, each carrying the input `input` builds for it.
fn transactions(
    to: Address,
    value: u64,
    gas_limit: u64,
    input: impl Fn(u64) -> Bytes,
) -> Vec<Recovered<MegaTxEnvelope>> {
    (0..N)
        .map(|nonce| {
            let tx = TxLegacy {
                chain_id: Some(CHAIN_ID),
                nonce,
                gas_price: 0,
                gas_limit,
                to: TxKind::Call(to),
                value: U256::from(value),
                input: input(nonce),
            };
            let signed =
                Signed::new_unchecked(tx, Signature::test_signature(), B256::repeat_byte(1));
            Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), SENDER)
        })
        .collect()
}

fn chain_spec() -> MegaHardforkConfig {
    MegaHardforkConfig::default()
        .with_all_activated()
        .with_params(SequencerRegistryConfig::placeholder())
        .with_params(ProtocolLimits::DEFAULT)
}

/// Runs the pre-block changes of block `number` on `db` and returns what they handed their
/// observer, without committing anything to `db`.
fn pre_block_states(db: MemoryDatabase, number: u64) -> Vec<(PreBlockStateSource, EvmState)> {
    let spec = chain_spec();
    let mut state = State::builder().with_database(db).build();
    let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env_at(number));
    let mut executor =
        MegaBlockExecutor::new(evm, block_ctx(), &spec, OpAlloyReceiptBuilder::default());
    let log = PreBlockStates::default();
    let captured = Arc::clone(&log);
    executor.set_pre_block_observer(Some(Box::new(
        move |source: PreBlockStateSource, state: &EvmState| {
            captured.lock().expect("pre-block observer").push((source, state.clone()));
        },
    )));
    executor.apply_pre_execution_changes().expect("the pre-block changes apply");
    drop(executor);
    let states = std::mem::take(&mut *log.lock().expect("pre-block observer"));
    states
}

/// The database the chain's steady state starts a block from: [`database`] with what the
/// previous block's pre-block changes left in it, the seven predeploys among them, committed in
/// the order the executor commits them.
fn steady_database() -> MemoryDatabase {
    let mut db = database();
    let states = pre_block_states(db.clone(), BLOCK_NUMBER - 1);
    let deployed = states
        .iter()
        .filter(|(source, _)| matches!(source, PreBlockStateSource::SystemContract(_)))
        .filter(|(_, state)| state.values().any(|account| account.is_created()))
        .count();
    assert_eq!(deployed, SYSTEM_CONTRACT_DEPLOY_COUNT, "the previous block deploys all seven");
    for (_, state) in states {
        db.commit(state);
    }
    db
}

/// Asserts that the pre-block changes of the bench's block on `db` find every predeploy in place:
/// each is a read-only witness entry, neither created nor touched.
fn assert_predeploys_in_place(db: &MemoryDatabase) {
    let states = pre_block_states(db.clone(), BLOCK_NUMBER);
    let deploys: Vec<_> = states
        .iter()
        .filter(|(source, _)| matches!(source, PreBlockStateSource::SystemContract(_)))
        .collect();
    assert_eq!(deploys.len(), SYSTEM_CONTRACT_DEPLOY_COUNT);
    for (source, state) in &deploys {
        assert_eq!(state.len(), 1, "{source:?} is one witness account");
        let account = state.values().next().expect("the account is present");
        assert!(!account.is_touched(), "{source:?} is not touched");
        assert!(!account.is_created(), "{source:?} is not created");
    }
    assert_eq!(states[0].0, PreBlockStateSource::Eip2935);
    assert_eq!(states[1].0, PreBlockStateSource::Eip4788);
}

fn bench_block(c: &mut Criterion) {
    let spec = chain_spec();
    let steady = steady_database();
    // Checked once before measurement, so a block that would deploy is not measured as steady.
    assert_predeploys_in_place(&steady);

    let workloads = [
        // Both budgets clear the state gas the Satin gas table charges: a transfer to an account
        // that does not exist yet pays for the new account, a storage write pays for the slot.
        ("transfers", transactions(RECIPIENT, 1, 300_000, |_| Bytes::new())),
        ("storage_writes", transactions(WRITER, 0, 300_000, slot_calldata)),
    ];

    let mut group = c.benchmark_group("block");
    for (name, txs) in &workloads {
        // The workload runs once outside the measurement, so a block that does not execute — or
        // one whose transactions keep nothing — is caught here rather than being measured.
        {
            let mut state = State::builder().with_database(steady.clone()).build();
            let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env());
            let mut executor =
                MegaBlockExecutor::new(evm, block_ctx(), &spec, OpAlloyReceiptBuilder::default());
            executor.apply_pre_execution_changes().expect("the block starts");
            for tx in txs {
                let outcome =
                    executor.execute_transaction_without_commit(tx).expect("the transaction runs");
                assert!(outcome.result.is_success(), "{:?}", outcome.result);
                assert_eq!(outcome.limit_exceeded, None, "no limit stops the workload");
                assert!(
                    outcome.usage.write_records > 0,
                    "every transaction of the workload keeps a write record"
                );
                executor.commit_transaction(outcome);
            }
            let (_, result) = executor.finish_with_counters().expect("the block finishes");
            assert_eq!(result.receipts().len(), N as usize);
            assert!(result.gas.execution > 0);
            assert_eq!(result.usage.write_records, N, "one record per transaction, at least");
        }

        group.bench_function(*name, |b| {
            b.iter_batched(
                || State::builder().with_database(steady.clone()).build(),
                |mut state| {
                    let result = {
                        let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env());
                        let mut executor = MegaBlockExecutor::new(
                            evm,
                            block_ctx(),
                            &spec,
                            OpAlloyReceiptBuilder::default(),
                        );
                        executor.apply_pre_execution_changes().unwrap();
                        for tx in txs {
                            black_box(executor.execute_transaction(tx).unwrap());
                        }
                        executor.finish_with_counters().unwrap().1
                    };
                    (state, black_box(result))
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.bench_function("steady_pre_block", |b| {
        b.iter_batched(
            || State::builder().with_database(steady.clone()).build(),
            |mut state| {
                let used = {
                    let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env());
                    let mut executor = MegaBlockExecutor::new(
                        evm,
                        block_ctx(),
                        &spec,
                        OpAlloyReceiptBuilder::default(),
                    );
                    executor.apply_pre_execution_changes().unwrap();
                    executor.limiter().block_gas_used
                };
                (state, black_box(used))
            },
            BatchSize::SmallInput,
        );
    });

    // The first block after activation deploys all seven, checked once before measurement.
    let deployed = pre_block_states(database(), BLOCK_NUMBER)
        .iter()
        .filter(|(source, _)| matches!(source, PreBlockStateSource::SystemContract(_)))
        .filter(|(_, state)| state.values().any(|account| account.is_created()))
        .count();
    assert_eq!(deployed, SYSTEM_CONTRACT_DEPLOY_COUNT, "the first block deploys all seven");

    group.bench_function("activation_pre_block", |b| {
        b.iter_batched(
            || State::builder().with_database(database()).build(),
            |mut state| {
                let used = {
                    let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env());
                    let mut executor = MegaBlockExecutor::new(
                        evm,
                        block_ctx(),
                        &spec,
                        OpAlloyReceiptBuilder::default(),
                    );
                    executor.apply_pre_execution_changes().unwrap();
                    executor.limiter().block_gas_used
                };
                (state, black_box(used))
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_block);
criterion_main!(benches);
