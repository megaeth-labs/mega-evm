//! `ExecuteEvm::transact` on the Satin engine next to op-revm's `OpEvm` on the same `CfgEnv`.
//!
//! Each workload runs through `MegaEvm` (`satin`) and through `OpEvm` (`op_revm`), so the gap is
//! the wrapper's own cost. EVM construction is setup and is not measured.
//!
//! - `empty_transaction`, `ether_transfer`: the per-transaction cost.
//! - `deep_calls`: a contract calling itself 64 deep, each frame writing a slot and logging, so
//!   every frame pays the frame lifecycle's lanes and every `SSTORE` and `LOG` its commit wrapper.
//! - `storage_writes`: 200 first writes to fresh slots, then 200 writes back, in one frame: the
//!   `SSTORE` wrapper's commit and refund.
//! - `logs`: 200 two-topic logs in one frame: the `LOG` wrapper's commit.
#![allow(missing_docs)]

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::{
    test_utils::{op_transaction, zero_fee_l1_block_info, BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId,
};
use op_revm::{L1BlockInfo, OpEvm, OpSpecId, OpTransaction};
use revm::{
    bytecode::opcode::{
        ADDRESS, CALL, CALLDATALOAD, DUP1, GAS, ISZERO, JUMPDEST, JUMPI, LOG0, LOG2, MSTORE, PUSH0,
        PUSH1, SSTORE, STOP, SUB, SWAP1,
    },
    context::{BlockEnv, CfgEnv, Context, ContextTr, TxEnv},
    inspector::NoOpInspector,
    ExecuteEvm, Journal,
};

const CALLER: Address = address!("0x0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0x0000000000000000000000000000000000100001");
const RECURSIVE: Address = address!("0x0000000000000000000000000000000000100002");
const WRITER: Address = address!("0x0000000000000000000000000000000000100003");
const LOGGER: Address = address!("0x0000000000000000000000000000000000100004");

/// Depth the recursive contract reaches.
const DEPTH: u8 = 64;
/// Slots written, then written back, by the storage workload; logs emitted by the log workload.
const REPEAT: u64 = 200;

/// Offset of the `JUMPDEST` the recursion ends at.
const DONE: u8 = 0x1f;

/// Reads `n` from calldata; while `n > 0` it writes slot `n`, logs, and calls itself with `n - 1`.
fn recursive_code() -> Bytes {
    #[rustfmt::skip]
    let body = [
        PUSH0, CALLDATALOAD,                             // n
        DUP1, ISZERO, PUSH1, DONE, JUMPI,                // n == 0: done
        DUP1, PUSH1, 1, SWAP1, SSTORE,                   // storage[n] = 1
        PUSH0, PUSH0, LOG0,                              // an empty log
        PUSH1, 1, SWAP1, SUB, PUSH0, MSTORE,             // memory[0..32] = n - 1
        PUSH0, PUSH0, PUSH1, 0x20, PUSH0, PUSH0, ADDRESS, GAS, CALL, // call self with n - 1
        STOP,
        JUMPDEST, STOP,                                  // done
    ];
    assert_eq!(body[DONE as usize], JUMPDEST);
    Bytes::from(body.to_vec())
}

/// Writes `REPEAT` fresh slots, then writes each back to zero.
fn writer_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for slot in 0..REPEAT {
        code = code.sstore(U256::from(slot), U256::from(1));
    }
    for slot in 0..REPEAT {
        code = code.sstore(U256::from(slot), U256::ZERO);
    }
    code.stop().build()
}

/// Emits `REPEAT` two-topic logs of 32 bytes.
fn logger_code() -> Bytes {
    let mut code = BytecodeBuilder::default();
    for topic in 0..REPEAT {
        code = code
            .push_number(topic)
            .push_number(topic)
            .push_number(32_u8)
            .append(PUSH0)
            .append(LOG2);
    }
    code.stop().build()
}

fn call_tx(to: Address, data: Bytes, gas_limit: u64) -> OpTransaction<TxEnv> {
    op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(to),
        data,
        gas_limit,
        ..Default::default()
    })
}

type OpContext = Context<
    BlockEnv,
    OpTransaction<TxEnv>,
    CfgEnv<OpSpecId>,
    MemoryDatabase,
    Journal<MemoryDatabase>,
    L1BlockInfo,
>;

fn tx(value: U256) -> OpTransaction<TxEnv> {
    op_transaction(TxEnv {
        caller: CALLER,
        kind: TxKind::Call(CALLEE),
        value,
        gas_limit: 1_000_000,
        ..Default::default()
    })
}

fn mega_context(db: MemoryDatabase) -> MegaContext<MemoryDatabase> {
    MegaContext::new(db, MegaSpecId::SATIN).with_chain(zero_fee_l1_block_info())
}

fn bench_transact(c: &mut Criterion) {
    let db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10u64.pow(18)))
        .account_balance(CALLEE, U256::from(1))
        .account_code(RECURSIVE, recursive_code())
        .account_code(WRITER, writer_code())
        .account_code(LOGGER, logger_code());
    let cfg = mega_context(db.clone()).cfg().clone();

    let mut depth = [0u8; 32];
    depth[31] = DEPTH;
    let workloads = [
        ("empty_transaction", tx(U256::ZERO)),
        ("ether_transfer", tx(U256::from(1))),
        ("deep_calls", call_tx(RECURSIVE, Bytes::from(depth.to_vec()), 30_000_000)),
        ("storage_writes", call_tx(WRITER, Bytes::new(), 30_000_000)),
        ("logs", call_tx(LOGGER, Bytes::new(), 30_000_000)),
    ];

    let mut group = c.benchmark_group("transact");
    for (workload, tx) in workloads {
        let satin = MegaEvm::new(mega_context(db.clone())).transact(OpTx(tx.clone())).unwrap();
        assert!(satin.result.is_success(), "{workload}: {:?}", satin.result);
        if workload == "deep_calls" {
            let written =
                satin.state[&RECURSIVE].storage.values().filter(|s| s.is_changed()).count();
            assert_eq!(written, DEPTH as usize, "every frame of the recursion ran");
        }
        group.bench_function(format!("{workload}/satin"), |b| {
            b.iter_batched(
                || MegaEvm::new(mega_context(db.clone())),
                |mut evm| evm.transact(OpTx(tx.clone())).unwrap(),
                BatchSize::SmallInput,
            );
        });
        group.bench_function(format!("{workload}/op_revm"), |b| {
            b.iter_batched(
                || {
                    let ctx = OpContext::new(db.clone(), OpSpecId::KARST)
                        .with_cfg(cfg.clone())
                        .with_chain(zero_fee_l1_block_info());
                    OpEvm::new(ctx, NoOpInspector)
                },
                |mut evm| evm.transact(tx.clone()).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_transact);
criterion_main!(benches);
