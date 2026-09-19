//! A set of JSON scenarios through `MegaEvm`.
//!
//! The inputs are the scenarios under `benches/scenarios` (the `test_utils::Scenario` format),
//! picked to spread over the mechanisms the engine runs. They are bench inputs, not a conformance
//! suite: nothing here compares their outcome with another engine, so an instruction-count change
//! means only that the execution or the pricing of those scenarios moved. Each scenario runs all
//! its transactions, one fresh `MegaEvm` per transaction; building the pre-state is setup and is
//! not measured.
#![allow(missing_docs)]

use std::path::Path;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::test_utils::Scenario;

/// `(bench id, scenario name)`: the scenario is `benches/scenarios/<name>.json`.
const SLICE: &[(&str, &str)] = &[
    ("sstore_set", "sstore_set_spills"),
    ("create", "create_success"),
    ("nested_revert", "nested_call_revert_with_spill"),
    ("two_txs", "two_txs_sequential"),
    ("system_call", "system_call"),
    ("calldata_floor", "test_calldata_floor_with_sstore_fork_Amsterdam_state_test"),
    (
        "nested_refund_depth_10",
        "test_nested_state_gas_refund_consumed_at_depth_fork_Amsterdam_state_test_consume_deepest_depth_10_sstore_restoration",
    ),
    ("eip7702_delegation", "eip7702_delegate_new_authority"),
    ("access_list", "access_list_warm_slots_and_account"),
    ("selfdestruct", "selfdestruct_created_in_same_tx_to_existing_account"),
    ("logs", "logs_all_topic_counts"),
    ("ecrecover", "precompile_ecrecover"),
    ("modexp", "precompile_modexp"),
];

fn load(path: &str) -> Scenario {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches/scenarios")
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
