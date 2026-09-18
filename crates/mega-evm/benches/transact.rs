//! `ExecuteEvm::transact` on the Satin engine next to op-revm's `OpEvm` on the same `CfgEnv`.
//!
//! A skeleton that keeps one benchmark target alive; T2.2 rebuilds the benchmark suite. Each
//! workload runs through `MegaEvm` (`satin`) and through `OpEvm` (`op_revm`), so the gap is the
//! wrapper's own cost. EVM construction is setup and is not measured.
#![allow(missing_docs)]

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, TxKind, U256};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::{
    test_utils::{op_transaction, zero_fee_l1_block_info, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId,
};
use op_revm::{L1BlockInfo, OpEvm, OpSpecId, OpTransaction};
use revm::{
    context::{BlockEnv, CfgEnv, Context, ContextTr, TxEnv},
    inspector::NoOpInspector,
    ExecuteEvm, Journal,
};

const CALLER: Address = address!("0x0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0x0000000000000000000000000000000000100001");

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
        .account_balance(CALLEE, U256::from(1));
    let cfg = mega_context(db.clone()).cfg().clone();

    let mut group = c.benchmark_group("transact");
    for (workload, value) in [("empty_transaction", U256::ZERO), ("ether_transfer", U256::from(1))]
    {
        group.bench_function(format!("{workload}/satin"), |b| {
            b.iter_batched(
                || MegaEvm::new(mega_context(db.clone())),
                |mut evm| evm.transact(OpTx(tx(value))).unwrap(),
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
                |mut evm| evm.transact(tx(value)).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_transact);
criterion_main!(benches);
