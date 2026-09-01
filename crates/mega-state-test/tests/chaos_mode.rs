//! End-to-end tests for the chaos sweep.
//!
//! The corpus run is what the mode is for, and it cannot be a unit test: it needs the EEST corpus
//! and takes seconds. What can be pinned here is everything the corpus run's conclusions rest on —
//! that the same seed reproduces the same run, that different seeds are actually different, that
//! narrowing the shape filter narrows and nothing else, that the read-only control is read-only,
//! and that a sweep which mutated nothing fails its own gate rather than reporting a clean corpus.

use state_test::{
    chaos::{
        chaos_unit, run_chaos, vector_seed, ChaosClass, ChaosRunConfig, ChaosShape, ShapeFilter,
    },
    diff::{execute_unit_in_mode, RunMode},
    runner::FixtureScan,
    types::{SpecName, TestUnit, TxPartIndices},
};
use std::path::PathBuf;

const SENDER: &str = "0x1000000000000000000000000000000000000001";
const CALLEE: &str = "0x2000000000000000000000000000000000000002";
const INNER: &str = "0x3000000000000000000000000000000000000003";

/// The single transaction vector these hand-built fixtures declare.
const VECTOR_0: TxPartIndices = TxPartIndices { data: 0, gas: 0, value: 0 };

/// `SSTORE(1, 1); CALL(0x2710 gas, INNER, no value, no args, no return); POP; LOG0(0, 0); STOP`.
///
/// One of everything a callback can be handed: a storage write, a child frame, a log, and enough
/// plain opcodes between them for the stream to reach every callback family.
fn callee_code() -> String {
    format!("0x600160015560006000600060006000 73{} 612710 f1 50 60006000a000", &INNER[2..])
        .replace(' ', "")
}

/// `SSTORE(2, 2); CREATE(0, 0, 0); POP; STOP` — a child frame the create callbacks see.
const INNER_CODE: &str = "0x60026002556000600060006000f05000";

fn unit_json() -> serde_json::Value {
    serde_json::json!({
        "env": {
            "currentChainID": "0x18c6",
            "currentCoinbase": "0x3000000000000000000000000000000000000009",
            "currentDifficulty": "0x0",
            "currentGasLimit": "0x1c9c380",
            "currentNumber": "0x10",
            "currentTimestamp": "0x3e8",
            "currentBaseFee": "0x0",
            "currentRandom": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "currentExcessBlobGas": "0x0"
        },
        "pre": {
            SENDER: { "balance": "0xde0b6b3a7640000", "code": "0x", "nonce": "0x0", "storage": {} },
            CALLEE: { "balance": "0x0", "code": callee_code(), "nonce": "0x0", "storage": {} },
            INNER: { "balance": "0x0", "code": INNER_CODE, "nonce": "0x0", "storage": {} },
        },
        "transaction": {
            "type": 0,
            "data": ["0x"],
            "gasLimit": ["0x1e8480"],
            "gasPrice": "0x0",
            "nonce": "0x0",
            "secretKey": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "sender": SENDER,
            "to": CALLEE,
            "value": ["0x0"]
        },
        "post": {}
    })
}

fn unit() -> TestUnit {
    serde_json::from_value(unit_json()).expect("valid unit json")
}

/// The chaos run's tally for `unit` under `seed` and `filter`.
fn mutations(seed: u64, filter: ShapeFilter) -> Vec<(String, u32)> {
    let run =
        execute_unit_in_mode(&unit(), VECTOR_0, &SpecName::Rex7, RunMode::Chaos { seed, filter })
            .expect("the fixture executes");
    let tally = run.chaos.expect("a chaos run reports its tally");
    let mut applied: Vec<(String, u32)> =
        tally.applied.iter().map(|(k, v)| ((*k).to_string(), *v)).collect();
    applied.sort();
    applied
}

/// The same seed produces the same run, mutation for mutation.
///
/// Everything else this mode claims rests on this: a flagged vector's report line is only a
/// reproduction if re-running it reproduces.
#[test]
fn test_a_seed_reproduces_its_own_run() {
    let first = mutations(0xC0FFEE, ShapeFilter::default());
    let second = mutations(0xC0FFEE, ShapeFilter::default());
    assert!(!first.is_empty(), "the fixture must reach enough callbacks to mutate something");
    assert_eq!(first, second, "the same seed must produce the same mutations");
}

/// Different seeds produce different runs.
///
/// The mirror of the test above, and the one that fails if the seed stops reaching the decision
/// stream at all — a generator wired to ignore its seed would pass reproducibility perfectly.
#[test]
fn test_different_seeds_produce_different_runs() {
    let seeds = [1u64, 2, 3, 4, 5, 6, 7, 8];
    let runs: Vec<_> = seeds.iter().map(|s| mutations(*s, ShapeFilter::default())).collect();
    assert!(
        runs.iter().any(|run| *run != runs[0]),
        "eight seeds that all mutate identically means the seed reaches nothing: {runs:?}",
    );
}

/// A vector's seed depends on every part of its identity, and on the global seed.
#[test]
fn test_a_vector_seed_separates_every_part_of_the_identity() {
    let base = vector_seed(7, "a.json", "unit", VECTOR_0);
    let others = [
        vector_seed(8, "a.json", "unit", VECTOR_0),
        vector_seed(7, "b.json", "unit", VECTOR_0),
        vector_seed(7, "a.json", "other", VECTOR_0),
        vector_seed(7, "a.json", "unit", TxPartIndices { data: 1, gas: 0, value: 0 }),
        vector_seed(7, "a.json", "unit", TxPartIndices { data: 0, gas: 1, value: 0 }),
        vector_seed(7, "a.json", "unit", TxPartIndices { data: 0, gas: 0, value: 1 }),
    ];
    for (i, other) in others.iter().enumerate() {
        assert_ne!(base, *other, "identity component {i} does not reach the seed");
    }
    assert_eq!(base, vector_seed(7, "a.json", "unit", VECTOR_0), "and the seed is a function");
}

/// Narrowing the filter keeps every surviving mutation where the full run put it.
///
/// This is what makes narrowing a triage tool rather than a different experiment: the shapes that
/// remain are applied at the same callbacks, so a flagged mutation is still there to be found.
#[test]
fn test_narrowing_the_filter_keeps_the_surviving_mutations() {
    let full = mutations(0xC0FFEE, ShapeFilter::default());
    let only = [ChaosShape::InjectGas, ChaosShape::DrainGas];
    let narrowed = mutations(0xC0FFEE, ShapeFilter::only(&only));
    let kept: Vec<_> = only.iter().map(|s| s.label()).collect();

    assert!(!narrowed.is_empty(), "the narrowed run must still mutate something");
    for (shape, _) in &narrowed {
        assert!(kept.contains(&shape.as_str()), "{shape} is not in the filter");
    }
    for (shape, count) in &full {
        if !kept.contains(&shape.as_str()) {
            continue;
        }
        let narrowed_count = narrowed.iter().find(|(s, _)| s == shape).map_or(0, |(_, c)| *c);
        assert!(
            narrowed_count >= *count,
            "{shape}: narrowing dropped a mutation the full run made ({count} -> {narrowed_count})",
        );
    }
}

/// Every shape label round-trips, and an unknown one is refused with a message that lists them.
#[test]
fn test_every_shape_label_parses_and_an_unknown_one_does_not() {
    for shape in ChaosShape::ALL {
        assert_eq!(ChaosShape::parse(shape.label()), Ok(shape));
    }
    let error = ChaosShape::parse("not_a_shape").expect_err("an unknown label must be refused");
    assert!(error.contains("inject_gas"), "the message must list the known shapes: {error}");
}

/// The read-only control leaves the execution exactly as it found it.
///
/// Checked here on one fixture and over the whole corpus by the sweep itself; this is the version
/// that fails in a unit test run rather than only in a five-second corpus sweep.
#[test]
fn test_the_control_inspector_changes_nothing() {
    let unit = unit();
    let plain = execute_unit_in_mode(&unit, VECTOR_0, &SpecName::Rex7, RunMode::Plain)
        .expect("the fixture executes");
    let observed = execute_unit_in_mode(&unit, VECTOR_0, &SpecName::Rex7, RunMode::Observe)
        .expect("the fixture executes");

    assert!(observed.observed > 0, "the control must actually be handed callbacks");
    assert!(observed.ledger.is_zero(), "and must book nothing: {:?}", observed.ledger);
    assert!(
        state_test::diff::compare(&observed.outcome, &plain.outcome).is_empty(),
        "an observation-only inspector moved something",
    );
}

/// A vector the rewriting run leaves executable comes back `Pass`, with mutations to show for it.
#[test]
fn test_a_mutated_vector_passes_with_mutations_recorded() {
    let verdict = chaos_unit(&unit(), VECTOR_0, &SpecName::Rex7, 0xC0FFEE, ShapeFilter::default());
    assert_eq!(verdict.class, ChaosClass::Pass, "{:?}", verdict.detail);
    assert!(verdict.applied.total() > 0, "the fixture must be mutated: {:?}", verdict.applied);
    assert!(verdict.applied.callbacks > 0, "and the callbacks must be counted");
}

/// A sweep whose inspector mutated nothing fails its own gate.
///
/// Every count such a run prints is truthful and every one of them is zero, which is exactly what
/// a clean sweep looks like. Reading it as a pass is how a chaos mode that stopped being chaotic
/// becomes a green nightly.
#[test]
fn test_a_sweep_that_mutated_nothing_is_a_failure() {
    let path = write_suite("chaos_mode_no_mutations.json");
    let scan = FixtureScan { files: vec![path], errors: vec![] };
    // An empty allow-list: every draw is rejected, so the run executes the whole corpus and
    // changes nothing.
    let tally = run_chaos(
        scan,
        ChaosRunConfig {
            spec: SpecName::Rex7,
            seed: 1,
            filter: ShapeFilter::only(&[]),
            single_thread: true,
            progress: false,
        },
    );

    assert_eq!(tally.count(ChaosClass::Pass), 1, "the vector must still be judged");
    assert_eq!(tally.count(ChaosClass::Panic), 0);
    assert_eq!(tally.shapes.total(), 0, "and must have been mutated in no way at all");
    assert!(tally.is_failure(), "a sweep that tested nothing must not report success");
}

/// A sweep that did mutate, over the same fixture, passes.
#[test]
fn test_a_sweep_that_mutated_passes() {
    let path = write_suite("chaos_mode_mutations.json");
    let scan = FixtureScan { files: vec![path], errors: vec![] };
    let tally = run_chaos(
        scan,
        ChaosRunConfig {
            spec: SpecName::Rex7,
            seed: 1,
            filter: ShapeFilter::default(),
            single_thread: true,
            progress: false,
        },
    );

    assert_eq!(tally.count(ChaosClass::Pass), 1);
    assert!(tally.shapes.total() > 0, "the fixture must be mutated");
    assert!(tally.flagged.is_empty(), "{:?}", tally.flagged);
    assert!(!tally.is_failure(), "the sweep must pass");
}

/// Writes the fixture to a unique temp file and returns its path.
fn write_suite(file_name: &str) -> PathBuf {
    let suite: serde_json::Map<String, serde_json::Value> =
        std::iter::once(("chaos_unit".to_string(), unit_json())).collect();
    let dir = std::env::temp_dir().join("mega_state_test_chaos_mode");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(file_name);
    std::fs::write(&path, serde_json::to_string_pretty(&suite).expect("serialize"))
        .expect("write fixture");
    path
}
