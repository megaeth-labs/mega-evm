//! A set of JSON scenarios through `MegaEvm`.
//!
//! The inputs are the scenarios under `benches/scenarios` (the `test_utils::Scenario` format),
//! picked to spread over the mechanisms the engine runs. They are bench inputs. Each transaction
//! is pinned, in `benches/corpus-expectations.json`, to an outcome class (success, revert, or
//! halt, with the halt reason) and to the gas used at the spec's byte prices. The bench checks
//! the pins in this setup, before anything is measured, so a pricing change that takes a shorter
//! path fails here instead of being timed.
//!
//! A measurement build whose byte prices are not the constants still checks the outcome class and
//! skips the gas pin: the gas moves with the prices (`satin-price-override`, `MEGA_SATIN_CPSB`,
//! `MEGA_SATIN_CPHB`).
//!
//! Set `UPDATE_CORPUS_EXPECTATIONS=1` and run `cargo bench -p mega-evm --bench corpus -- --test`
//! to rewrite the pins from a run at the constants.
//!
//! A scenario is a block: one `MegaEvm`, built with the pre-state in the setup, runs all its
//! transactions and commits each before the next (`Scenario::run_on`). What is measured is the
//! transactions and their commits; building the EVM, like building the pre-state, is setup and is
//! not measured, as in `transact`, so the two families measure per-transaction execution alike.
//! The one difference is the commit to the in-memory database, which `transact` does not do.
#![allow(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use mega_evm::{
    revm::context::result::ExecutionResult,
    test_utils::{Scenario, ScenarioTxOutcome},
};
use serde::{Deserialize, Serialize};

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

/// The pins, next to the scenarios. Not inside `benches/scenarios`: that directory is read as
/// scenario JSON, which refuses an unknown field.
const EXPECTATIONS: &str = "benches/corpus-expectations.json";

/// The scenario directory. Every JSON file here is one bench scenario.
const SCENARIOS_DIR: &str = "benches/scenarios";

/// Set to rewrite the pins from a run at the spec's byte prices.
const UPDATE_ENV: &str = "UPDATE_CORPUS_EXPECTATIONS";

fn bench_corpus(c: &mut Criterion) {
    let expectations = prepare();
    // Other byte prices move the receipt gas, so the gas pin holds only at the constants.
    // The outcome class does not move with the prices and is always checked.
    let check_gas = at_constants();
    let mut group = c.benchmark_group("corpus");
    for (id, name) in SLICE {
        let scenario = load(name);
        let file = scenario_path(name);
        let (outcomes, _) = scenario.run(scenario.database());
        let expected = expectations.get(*name).unwrap_or_else(|| {
            panic!("{file}: no pinned outcome; rerun with {UPDATE_ENV}=1");
        });
        assert!(
            outcomes.len() == expected.len(),
            "{file}: expected {} transactions, actual {}",
            expected.len(),
            outcomes.len()
        );
        for (i, (outcome, pin)) in outcomes.iter().zip(expected.iter()).enumerate() {
            let actual = pinned_tx(&file, i, outcome);
            let expected_outcome = pin.label();
            let actual_outcome = actual.label();
            assert!(
                actual_outcome == expected_outcome,
                "{file}: tx[{i}] outcome: expected {expected_outcome}, actual {actual_outcome}"
            );
            if check_gas {
                let expected_gas = pin.gas_used();
                let actual_gas = actual.gas_used();
                assert!(
                    actual_gas == expected_gas,
                    "{file}: tx[{i}] gas_used: expected {expected_gas}, actual {actual_gas}"
                );
            }
        }
        group.bench_function(*id, |b| {
            b.iter_batched(
                || scenario.evm(scenario.database()),
                |mut evm| scenario.run_on(&mut evm),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn load(name: &str) -> Scenario {
    let file = scenarios_dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&file).unwrap_or_else(|err| panic!("{file:?}: {err}"));
    let scenario: Scenario = serde_json::from_str(&text).unwrap_or_else(|err| {
        panic!("{file:?}: {err}");
    });
    scenario.validate().unwrap_or_else(|err| panic!("{file:?}: {err}"));
    scenario
}

fn at_constants() -> bool {
    mega_evm::active_satin_prices().is_constants()
}

/// The pins the bench checks, rewritten first when `UPDATE_CORPUS_EXPECTATIONS` is set.
fn prepare() -> BTreeMap<String, Vec<PinnedTx>> {
    let slice = slice_names();
    assert_scenarios(SCENARIOS_DIR, &slice, &scenario_names_on_disk());

    if std::env::var_os(UPDATE_ENV).is_some() {
        assert!(
            at_constants(),
            "{UPDATE_ENV} rewrites the gas pins, and the byte prices in effect are not the \
             constants"
        );
        let pins = collect_pins();
        write_pins(&pins);
        let read_back = read_pins();
        assert!(
            read_back == pins,
            "{EXPECTATIONS}: the written pins did not read back\n\
             expected {pins:?}\n\
             actual {read_back:?}"
        );
        return index(pins);
    }

    let pins = read_pins();
    let named = names_of(&pins);
    assert_scenarios(EXPECTATIONS, &slice, &named);
    assert!(named.len() == pins.len(), "{EXPECTATIONS}: a scenario is pinned twice");
    index(pins)
}

fn assert_scenarios(what: &str, expected: &BTreeSet<String>, actual: &BTreeSet<String>) {
    assert!(expected == actual, "{what}: expected scenarios {expected:?}, actual {actual:?}");
}

fn slice_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (_, name) in SLICE {
        assert!(names.insert((*name).to_owned()), "SLICE lists {name} twice");
    }
    names
}

fn scenario_names_on_disk() -> BTreeSet<String> {
    let dir = scenarios_dir();
    let mut names = BTreeSet::new();
    let entries = std::fs::read_dir(&dir).unwrap_or_else(|err| panic!("{dir:?}: {err}"));
    for entry in entries {
        let path = entry.unwrap_or_else(|err| panic!("{dir:?}: {err}")).path();
        assert!(
            path.extension().and_then(|ext| ext.to_str()) == Some("json"),
            "{path:?}: the scenarios directory holds only scenario json"
        );
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_else(|| panic!("{path:?}: scenario file name is not utf-8"))
            .to_owned();
        assert!(names.insert(name.clone()), "{path:?}: scenario {name} is listed twice");
    }
    names
}

fn collect_pins() -> Vec<ScenarioPins> {
    SLICE
        .iter()
        .map(|(_, name)| {
            let scenario = load(name);
            let file = scenario_path(name);
            let (outcomes, _) = scenario.run(scenario.database());
            let txs = outcomes
                .iter()
                .enumerate()
                .map(|(i, outcome)| pinned_tx(&file, i, outcome))
                .collect();
            ScenarioPins { scenario: (*name).to_owned(), txs }
        })
        .collect()
}

fn pinned_tx(file: &str, index: usize, outcome: &ScenarioTxOutcome) -> PinnedTx {
    let Ok(executed) = outcome else {
        panic!("{file}: tx[{index}] is rejected: {outcome:?}");
    };
    // The receipt's gas used: a shorter path moves it, which is what the pin is for.
    let gas_used = executed.result.tx_gas_used();
    match &executed.result {
        ExecutionResult::Success { .. } => PinnedTx::Success { gas_used },
        ExecutionResult::Revert { .. } => PinnedTx::Revert { gas_used },
        ExecutionResult::Halt { reason, .. } => {
            PinnedTx::Halt { reason: format!("{reason:?}"), gas_used }
        }
    }
}

fn write_pins(pins: &[ScenarioPins]) {
    let path = expectations_path();
    let text = serde_json::to_string_pretty(pins).unwrap_or_else(|err| panic!("{path:?}: {err}"));
    std::fs::write(&path, format!("{text}\n")).unwrap_or_else(|err| panic!("{path:?}: {err}"));
}

fn read_pins() -> Vec<ScenarioPins> {
    let path = expectations_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("{EXPECTATIONS}: {err}; rerun with {UPDATE_ENV}=1");
    });
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{EXPECTATIONS}: {err}"))
}

fn names_of(pins: &[ScenarioPins]) -> BTreeSet<String> {
    pins.iter().map(|pin| pin.scenario.clone()).collect()
}

fn index(pins: Vec<ScenarioPins>) -> BTreeMap<String, Vec<PinnedTx>> {
    pins.into_iter().map(|pin| (pin.scenario, pin.txs)).collect()
}

fn scenarios_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(SCENARIOS_DIR)
}

fn expectations_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(EXPECTATIONS)
}

fn scenario_path(name: &str) -> String {
    format!("{SCENARIOS_DIR}/{name}.json")
}

/// One scenario's pinned transactions, in the order the scenario runs them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioPins {
    scenario: String,
    txs: Vec<PinnedTx>,
}

/// One transaction's pinned outcome.
///
/// `gas_used` is the receipt's figure at the spec's byte prices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum PinnedTx {
    Success { gas_used: u64 },
    Revert { gas_used: u64 },
    Halt { reason: String, gas_used: u64 },
}

impl PinnedTx {
    fn gas_used(&self) -> u64 {
        match self {
            Self::Success { gas_used } |
            Self::Revert { gas_used } |
            Self::Halt { gas_used, .. } => *gas_used,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Success { .. } => "success".to_owned(),
            Self::Revert { .. } => "revert".to_owned(),
            Self::Halt { reason, .. } => format!("halt({reason})"),
        }
    }
}

criterion_group!(benches, bench_corpus);
criterion_main!(benches);
