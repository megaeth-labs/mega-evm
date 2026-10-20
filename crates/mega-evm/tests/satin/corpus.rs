//! The corpus bench's scenarios keep their outcome and gas.
//!
//! `benches/corpus.rs` times the JSON scenarios under `benches/scenarios`. A change that sends a
//! transaction of one down a shorter path would be timed without notice, so every transaction of
//! every scenario file is pinned here: its outcome class and its gas used.
//! The scenarios fix their gas limits, so at dearer byte prices a transaction can run out of gas;
//! the pins hold at the spec's byte prices only, and the snapshot is skipped at other ones.

use std::{collections::BTreeMap, path::Path};

use mega_evm::{revm::context::result::ExecutionResult, test_utils::Scenario};
use serde::Serialize;

/// One transaction's outcome class: `success`, `revert` or `halt(<reason>)`.
fn class(result: &ExecutionResult<mega_evm::MegaHaltReason>) -> String {
    match result {
        ExecutionResult::Success { .. } => "success".to_owned(),
        ExecutionResult::Revert { .. } => "revert".to_owned(),
        ExecutionResult::Halt { reason, .. } => format!("halt({reason:?})"),
    }
}

#[derive(Serialize)]
struct Tx {
    outcome: String,
    gas_used: u64,
}

#[test]
fn test_corpus_scenarios_keep_their_outcome_and_gas() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/scenarios");
    let mut gas: BTreeMap<String, Vec<Tx>> = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        assert_eq!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("json"),
            "{path:?}: the scenarios directory holds only scenario json"
        );
        let name = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        let scenario: Scenario =
            serde_json::from_str(&text).unwrap_or_else(|err| panic!("{path:?}: {err}"));
        scenario.validate().unwrap_or_else(|err| panic!("{path:?}: {err}"));
        let (outcomes, _) = scenario.run(scenario.database());
        let mut scenario_gas = Vec::new();
        for (i, outcome) in outcomes.iter().enumerate() {
            let executed = outcome.as_ref().unwrap_or_else(|err| {
                panic!("{name}: tx[{i}] is rejected: {err:?}");
            });
            let outcome = class(&executed.result);
            scenario_gas.push(Tx { outcome, gas_used: executed.result.tx_gas_used() });
        }
        gas.insert(name, scenario_gas);
    }
    assert_eq!(gas.len(), 13, "the corpus has 13 scenarios");

    crate::assert_sorted_json_snapshot!("corpus_outcomes_and_gas", gas);
}
