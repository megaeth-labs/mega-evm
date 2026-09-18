//! Construction of a `MegaEvm` through `MegaEvmFactory`, as a node's block executor makes one
//! per block.
//!
//! `create_evm` builds the context and applies the configuration the spec fixes; the EVM is
//! dropped inside the measurement, so its teardown counts too.
#![allow(missing_docs)]

use alloy_evm::{EvmEnv, EvmFactory};
use criterion::{criterion_group, criterion_main, Criterion};
use mega_evm::{MegaEvmFactory, MegaSpecId};
use revm::{
    context::{BlockEnv, CfgEnv},
    database::EmptyDB,
    inspector::NoOpInspector,
};
use std::hint::black_box;

fn evm_env() -> EvmEnv<MegaSpecId> {
    EvmEnv { cfg_env: CfgEnv::new_with_spec(MegaSpecId::SATIN), block_env: BlockEnv::default() }
}

fn bench_factory(c: &mut Criterion) {
    let factory = MegaEvmFactory::new();
    let mut group = c.benchmark_group("factory");
    group.bench_function("create_evm", |b| {
        b.iter(|| drop(black_box(factory.create_evm(EmptyDB::default(), black_box(evm_env())))));
    });
    group.bench_function("create_evm_with_inspector", |b| {
        b.iter(|| {
            drop(black_box(factory.create_evm_with_inspector(
                EmptyDB::default(),
                black_box(evm_env()),
                NoOpInspector,
            )))
        });
    });
    group.finish();
}

criterion_group!(benches, bench_factory);
criterion_main!(benches);
