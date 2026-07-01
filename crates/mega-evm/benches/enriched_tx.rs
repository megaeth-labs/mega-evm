//! Benchmarks the cost of `MegaTransactionExt::{tx_size, estimated_da_size}` recompute vs the
//! `EnrichedMegaTx` cached fields.
//!
//! `EnrichedMegaTx` exists to precompute `tx_size`/`da_size` once (e.g. via `new_slow` from a
//! mempool-cached value) so block-execution callers can reuse them instead of recomputing on
//! every access. Three rows:
//! - `recompute_via_tx_unwrap` mirrors `MegaBlockExecutor::run_transaction`'s call pattern
//!   (`tx.tx().estimated_da_size()` / `tx.tx().tx_size()`), which unwraps `EnrichedMegaTx` down to
//!   the raw inner transaction and always hits the recomputing default impl.
//! - `via_trait_dispatch` mirrors `MegaBlockExecutor::run_transaction_enriched`'s call pattern
//!   (`tx.estimated_da_size()` / `tx.tx_size()` on the outer wrapper), which now dispatches to the
//!   stored fields.
//! - `cached_fields` reads the wrapper's precomputed fields directly, the floor
//!   `via_trait_dispatch` should match.

#![allow(missing_docs)]

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::RecoveredTx;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, U256};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use mega_evm::{EnrichedMegaTx, MegaTransactionExt, MegaTxEnvelope};

const CALLER: Address = address!("2000000000000000000000000000000000000001");
const CONTRACT: Address = address!("3000000000000000000000000000000000000001");

/// Calldata sizes spanning a plain transfer up to a large multicall-style payload.
const CALLDATA_SIZES: &[usize] = &[0, 68, 180, 1000];

fn enriched_tx(calldata_len: usize) -> EnrichedMegaTx<Recovered<MegaTxEnvelope>> {
    let tx = TxLegacy {
        chain_id: Some(1),
        nonce: 7,
        gas_price: 9,
        gas_limit: 21_000,
        to: TxKind::Call(CONTRACT),
        value: U256::from(11),
        input: Bytes::from(vec![0xabu8; calldata_len]),
    };
    let envelope = MegaTxEnvelope::Legacy(Signed::new_unchecked(
        tx,
        Signature::test_signature(),
        Default::default(),
    ));
    let recovered = Recovered::new_unchecked(envelope, CALLER);
    EnrichedMegaTx::new_slow(recovered)
}

/// `MegaBlockExecutor::run_transaction`'s hot-path pattern: `tx.tx().<method>()` unwraps
/// `EnrichedMegaTx` down to the raw inner transaction, so `tx_size`/`estimated_da_size` always
/// hit the recomputing default impl even when the wrapper carries precomputed fields.
fn bench_recompute_via_tx_unwrap(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_size_da_size/recompute_via_tx_unwrap");
    for &len in CALLDATA_SIZES {
        let tx = enriched_tx(len);
        group.bench_with_input(BenchmarkId::new("estimated_da_size", len), &tx, |b, tx| {
            b.iter(|| black_box(tx.tx()).estimated_da_size())
        });
        group.bench_with_input(BenchmarkId::new("tx_size", len), &tx, |b, tx| {
            b.iter(|| black_box(tx.tx()).tx_size())
        });
    }
    group.finish();
}

/// `MegaBlockExecutor::run_transaction_enriched`'s call pattern: `tx.<method>()` called directly
/// on the outer `EnrichedMegaTx`, dispatching to the stored fields via `MegaTransactionExt`.
fn bench_via_trait_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_size_da_size/via_trait_dispatch");
    for &len in CALLDATA_SIZES {
        let tx = enriched_tx(len);
        group.bench_with_input(BenchmarkId::new("estimated_da_size", len), &tx, |b, tx| {
            b.iter(|| black_box(tx).estimated_da_size())
        });
        group.bench_with_input(BenchmarkId::new("tx_size", len), &tx, |b, tx| {
            b.iter(|| black_box(tx).tx_size())
        });
    }
    group.finish();
}

/// Reading `EnrichedMegaTx`'s precomputed fields directly.
fn bench_cached_fields(c: &mut Criterion) {
    let mut group = c.benchmark_group("tx_size_da_size/cached_fields");
    for &len in CALLDATA_SIZES {
        let tx = enriched_tx(len);
        group.bench_with_input(BenchmarkId::new("estimated_da_size", len), &tx, |b, tx| {
            b.iter(|| black_box(tx).da_size)
        });
        group.bench_with_input(BenchmarkId::new("tx_size", len), &tx, |b, tx| {
            b.iter(|| black_box(tx).tx_size)
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_recompute_via_tx_unwrap,
    bench_via_trait_dispatch,
    bench_cached_fields
);
criterion_main!(benches);
