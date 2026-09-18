//! A slice of the differential corpus through `MegaEvm`.
//!
//! The inputs are scenarios of `crates/mega-differential/scenarios`, the corpus the differential
//! harness compares against stock revm 43, so an instruction-count change here means the
//! execution or the pricing of those scenarios moved. Each scenario runs all its transactions,
//! one fresh `MegaEvm` per transaction as the harness does; building the pre-state is setup and
//! is not measured.
#![allow(missing_docs)]

use std::path::Path;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::test_utils::Scenario;

/// `(bench id, scenario path under the corpus directory)`: a few scenarios of each origin,
/// spread over the mechanisms the corpus covers.
const SLICE: &[(&str, &str)] = &[
    ("sstore_set", "handwritten/sstore_set_spills"),
    ("create", "handwritten/create_success"),
    ("nested_revert", "handwritten/nested_call_revert_with_spill"),
    ("two_txs", "handwritten/two_txs_sequential"),
    ("system_call", "handwritten/system_call"),
    ("calldata_floor", "eest/test_calldata_floor_with_sstore_fork_Amsterdam_state_test"),
    (
        "nested_refund_depth_10",
        "eest/test_nested_state_gas_refund_consumed_at_depth_fork_Amsterdam_state_test_consume_deepest_depth_10_sstore_restoration",
    ),
    ("eip7702_delegation", "harness/eip7702_delegate_new_authority"),
    ("access_list", "harness/access_list_warm_slots_and_account"),
    ("selfdestruct", "harness/selfdestruct_created_in_same_tx_to_existing_account"),
    ("logs", "harness/logs_all_topic_counts"),
    ("ecrecover", "harness/precompile_ecrecover"),
    ("modexp", "harness/precompile_modexp"),
];

fn load(path: &str) -> Scenario {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../mega-differential/scenarios")
        .join(format!("{path}.json"));
    let text = std::fs::read_to_string(&file).unwrap_or_else(|err| panic!("{file:?}: {err}"));
    let scenario: Scenario = serde_json::from_str(&text).unwrap();
    scenario.validate().unwrap();
    scenario
}

fn bench_corpus(c: &mut Criterion) {
    let mut group = c.benchmark_group("corpus");
    for (id, path) in SLICE {
        let scenario = load(path);
        // The measured path must be the scenario's execution, not a rejected transaction.
        let (outcomes, _) = scenario.run(scenario.database());
        for (i, outcome) in outcomes.iter().enumerate() {
            assert!(outcome.is_ok(), "{path}: tx[{i}] is rejected: {outcome:?}");
        }
        group.bench_function(*id, |b| {
            b.iter_batched(|| scenario.database(), |db| scenario.run(db), BatchSize::SmallInput);
        });
    }
    group.finish();
}

criterion_group!(benches, bench_corpus);
criterion_main!(benches);
