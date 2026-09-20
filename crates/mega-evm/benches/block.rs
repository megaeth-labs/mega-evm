//! A block through `MegaBlockExecutor`: what block execution costs per transaction on top of
//! what the transaction itself costs.
//!
//! Two workloads of `N` transactions each, both with the block's limits unset so the
//! per-transaction work is the executor's own: the admission checks, the size estimates, the
//! counters, the receipt and the commit.
//!
//! - `transfers`: value transfers to accounts with no code — the cheapest transaction there is, so
//!   the executor's own cost is most of what is measured.
//! - `storage_writes`: calls to a contract that writes the slot the transaction names in its
//!   calldata, so every transaction changes storage and carries a write record through the
//!   counters, with a log-free receipt.
//!
//! The pre-block calls and the state the block is built on are setup, and the measurement runs
//! `apply_pre_execution_changes`, the transactions and `finish` — the whole block a node
//! executes.
#![allow(missing_docs)]

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvmFactory, MegaHardforkConfig,
    MegaSpecId, MegaTxEnvelope,
};
use revm::{
    bytecode::opcode::{CALLDATALOAD, SSTORE},
    context::{BlockEnv, CfgEnv},
    database::State,
};
use std::hint::black_box;

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

/// The database every iteration starts from.
fn database() -> MemoryDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_code(WRITER, writer_code());
    db.set_account_code(RECIPIENT, Bytes::new());
    db.set_account_balance(SENDER, U256::from(1_000_000_000_000_000_u64));
    db
}

fn evm_env() -> EvmEnv<MegaSpecId> {
    let mut cfg_env = CfgEnv::new_with_spec(MegaSpecId::SATIN);
    cfg_env.chain_id = CHAIN_ID;
    EvmEnv {
        cfg_env,
        block_env: BlockEnv {
            number: U256::from(BLOCK_NUMBER),
            timestamp: U256::from(BLOCK_TIMESTAMP),
            gas_limit: 1_000_000_000,
            ..Default::default()
        },
    }
}

/// A block whose limits are all unset, so what is measured is the executor's own work.
fn block_ctx() -> MegaBlockExecutionCtx {
    MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), BlockLimits::no_limits())
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

fn bench_block(c: &mut Criterion) {
    let spec = MegaHardforkConfig::default().with_all_activated();
    let workloads = [
        ("transfers", transactions(RECIPIENT, 1, 100_000, |_| Bytes::new())),
        ("storage_writes", transactions(WRITER, 0, 200_000, slot_calldata)),
    ];

    let mut group = c.benchmark_group("block");
    for (name, txs) in &workloads {
        // The workload runs once outside the measurement, so a block that does not execute — or
        // one whose transactions keep nothing — is caught here rather than being measured.
        {
            let mut state = State::builder().with_database(database()).build();
            let evm = MegaEvmFactory::new().create_evm(&mut state, evm_env());
            let mut executor =
                MegaBlockExecutor::new(evm, block_ctx(), &spec, OpAlloyReceiptBuilder::default());
            executor.apply_pre_execution_changes().expect("the block starts");
            for tx in txs {
                let outcome =
                    executor.execute_transaction_without_commit(tx).expect("the transaction runs");
                assert!(outcome.result.is_success(), "{:?}", outcome.result);
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
                || State::builder().with_database(database()).build(),
                |mut state| {
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
                    black_box(executor.finish_with_counters().unwrap().1)
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_block);
criterion_main!(benches);
