//! End-to-end tests for the chaos sweep.
//!
//! The corpus run is what the mode is for, and it cannot be a unit test: it needs the EEST corpus
//! and takes seconds. What can be pinned here is everything the corpus run's conclusions rest on —
//! that the same seed reproduces the same run, that different seeds are actually different, that
//! narrowing the shape filter narrows and nothing else, that the read-only control is read-only,
//! and that a sweep which mutated nothing fails its own gate rather than reporting a clean corpus.

use mega_evm::FORBIDDEN_FRAME_INIT_REWRITE;
use state_test::{
    chaos::{
        chaos_unit, run_chaos, vector_seed, ChaosClass, ChaosRunConfig, ChaosShape, ChaosTally,
        ShapeFilter,
    },
    diff::{execute_unit_in_mode, execute_unit_reporting_chaos, RunMode},
    runner::FixtureScan,
    types::{SpecName, TestUnit, TxPartIndices},
};
use std::path::PathBuf;

const SENDER: &str = "0x1000000000000000000000000000000000000001";
const CALLEE: &str = "0x2000000000000000000000000000000000000002";
const INNER: &str = "0x3000000000000000000000000000000000000003";
/// An address with no code and no `pre` entry, so a `CALL` to it comes back out of frame init
/// without a frame ever being built.
const EMPTY: &str = "0x4000000000000000000000000000000000000004";

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

/// [`callee_code`] with two things the shape pool needs and the plain fixture does not offer: a
/// slot set and cleared again, so every frame ends holding a refund the EVM produced, and a `CALL`
/// that asks for a word of return data, so a finished call outcome has a range to rewrite.
fn refunding_callee_code() -> String {
    format!(
        "0x6001600155 6000600155 6020 6000 6000 6000 6000 73{} 612710 f1 50 60006000a000",
        &INNER[2..]
    )
    .replace(' ', "")
}

fn unit_json() -> serde_json::Value {
    unit_json_with(callee_code())
}

fn unit_json_with(callee: String) -> serde_json::Value {
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
            CALLEE: { "balance": "0x0", "code": callee, "nonce": "0x0", "storage": {} },
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

/// The fixture the ledger-gate test uses — see [`refunding_callee_code`].
fn refunding_unit() -> TestUnit {
    serde_json::from_value(unit_json_with(refunding_callee_code())).expect("valid unit json")
}

/// [`callee_code`] pointed at an account with no code, so its `CALL` returns out of frame init
/// with no child frame ever built.
fn empty_target_callee_code() -> String {
    format!("0x600160015560006000600060006000 73{} 612710 f1 50 60006000a000", &EMPTY[2..])
        .replace(' ', "")
}

/// The fixture the refused-shape test uses: the cheapest way to reach a result frame init
/// produced, which is the only kind [`ChaosShape::MoveInitResultClass`] fires on.
fn init_result_unit() -> TestUnit {
    serde_json::from_value(unit_json_with(empty_target_callee_code())).expect("valid unit json")
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
    // A seed whose full run draws both of the shapes the narrowed one keeps; which seeds those are
    // is a function of the pool's size, so a shape added to the pool can move it.
    const SEED: u64 = 2;
    let full = mutations(SEED, ShapeFilter::default());
    let only = [ChaosShape::InjectGas, ChaosShape::DrainGas];
    let narrowed = mutations(SEED, ShapeFilter::only(&only));
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

/// ★ Every shape the ledger gate is stated over really does move the ledger, run on its own.
///
/// [`ChaosClass::LedgerBlind`] fires when a run applied one of these and the ledger is still
/// all-zero. That verdict is only meaningful if the premise holds — so this checks the premise
/// directly, shape by shape, rather than trusting the partition. Five of the shapes here
/// (`grow_memory_free`, `move_outcome_metadata`, `cancel_refund_edit`, `skip_opcode`,
/// `rewrite_return_data`) were added because the shim did *not* book them, and this is the test
/// that would have said so.
#[test]
fn test_every_always_booked_shape_moves_the_ledger() {
    let always_booked: Vec<ChaosShape> = ChaosShape::ALL
        .into_iter()
        .filter(|s| s.is_always_booked() && *s != ChaosShape::MoveInitResultClass)
        .collect();
    assert!(!always_booked.is_empty(), "the gate must be stated over something");

    let unit = refunding_unit();
    for shape in always_booked {
        let filter = ShapeFilter::only(&[shape]);
        let mut reached = 0u32;
        // A narrow filter only lands where the stream happens to draw that shape, so sweep seeds
        // until enough of them do. A shape no seed reaches is a hole in the pool, not a pass.
        for seed in 0u64..1_024 {
            let run = execute_unit_in_mode(
                &unit,
                VECTOR_0,
                &SpecName::Rex7,
                RunMode::Chaos { seed, filter },
            )
            .expect("the fixture executes");
            if run.chaos.as_ref().expect("a chaos run reports its tally").total() == 0 {
                continue;
            }
            reached += 1;
            assert!(
                !run.ledger.is_zero(),
                "{} applied under seed {seed} and the shim booked nothing: {:?}",
                shape.label(),
                run.ledger,
            );
            if reached == 3 {
                break;
            }
        }
        assert_eq!(reached, 3, "{}: too few seeds in the sweep applied it", shape.label());
    }
}

/// The one always-booked shape a successful run cannot be measured on, and the stronger premise
/// that stands in for it.
///
/// The shim answers [`ChaosShape::MoveInitResultClass`] by declining the transaction, so there is
/// no receipt and no ledger to read — and nothing for the ledger gate to be stated over. What
/// replaces it is stronger than an all-zero-ledger check: a run that never executed cannot reach a
/// block at all. What this pins is that the decline really is the shim's refusal, that the sweep
/// classifies it as the designed outcome rather than as a defect, and that the tally still reports
/// what such a run mutated even though it produced nothing.
#[test]
fn test_the_refused_shape_is_declined_and_counted() {
    let unit = init_result_unit();
    let filter = ShapeFilter::only(&[ChaosShape::MoveInitResultClass]);
    let mut reached = 0u32;
    for seed in 0u64..1_024 {
        let mut applied = ChaosTally::default();
        let run = execute_unit_reporting_chaos(
            &unit,
            VECTOR_0,
            &SpecName::Rex7,
            RunMode::Chaos { seed, filter },
            &mut applied,
        );
        if applied.total() == 0 {
            assert!(run.is_ok(), "a run that mutated nothing must execute: {:?}", run.err());
            continue;
        }
        reached += 1;
        let error = run.expect_err("a refused rewrite declines the transaction").to_string();
        assert!(
            error.contains(FORBIDDEN_FRAME_INIT_REWRITE),
            "seed {seed} was declined for something other than the refusal: {error}",
        );
        let verdict = chaos_unit(&unit, VECTOR_0, &SpecName::Rex7, seed, filter);
        assert_eq!(
            verdict.class,
            ChaosClass::Refused,
            "a refusal is the designed outcome, not a disagreement about whether the \
             transaction executes",
        );
        assert!(!verdict.class.is_failure(), "a refusal must not fail the sweep");
        assert_eq!(
            verdict.applied.total(),
            applied.total(),
            "a declined run still has to report what it mutated",
        );
        if reached == 3 {
            break;
        }
    }
    assert_eq!(reached, 3, "too few seeds in the sweep reached a result frame init produced");
}

/// The partition the gate rests on is not vacuous in either direction.
///
/// A gate stated over every shape would fail on a working shim — several shapes are booked only
/// when what they moved still reaches something — and one stated over none would never fire.
#[test]
fn test_the_always_booked_partition_is_not_vacuous() {
    let (booked, conditional): (Vec<_>, Vec<_>) =
        ChaosShape::ALL.into_iter().partition(|s| s.is_always_booked());
    assert!(!booked.is_empty(), "the gate must have shapes to fire on");
    assert!(
        !conditional.is_empty(),
        "a shape whose booking is conditional must stay out of the gate; if none is left, the \
         gate should be stated over the whole pool instead",
    );
    for shape in [ChaosShape::InjectGas, ChaosShape::RaiseResultGas, ChaosShape::WriteReservoir] {
        assert!(
            !shape.is_always_booked(),
            "{} is booked only when what it moved still reaches something",
            shape.label(),
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

/// The declared control produces the run the other two produce.
///
/// Three runs of one vector: no inspector, the control measured, and the control declared
/// `TrustedObserver` so the shim delegates without measuring. The declaration's whole claim is
/// that the third is the first, and the field-by-field comparison is what says so — the receipt,
/// the four resource dimensions, the roots, and everything else `SpecOutcome` carries.
///
/// The callback count is asserted equal too, because every other assertion here would also pass
/// for a fast path that skipped the inspector rather than the measurement.
#[test]
fn test_the_declared_control_changes_nothing_either() {
    let unit = unit();
    let plain = execute_unit_in_mode(&unit, VECTOR_0, &SpecName::Rex7, RunMode::Plain)
        .expect("the fixture executes");
    let observed = execute_unit_in_mode(&unit, VECTOR_0, &SpecName::Rex7, RunMode::Observe)
        .expect("the fixture executes");
    let trusted = execute_unit_in_mode(&unit, VECTOR_0, &SpecName::Rex7, RunMode::ObserveTrusted)
        .expect("the fixture executes");

    assert!(trusted.observed > 0, "the declared control must still be handed callbacks");
    assert_eq!(
        trusted.observed, observed.observed,
        "and the same ones the measured control was handed",
    );
    assert!(trusted.ledger.is_zero(), "the fast path books nothing: {:?}", trusted.ledger);
    assert!(
        state_test::diff::compare(&trusted.outcome, &plain.outcome).is_empty(),
        "a declared observation-only inspector moved something",
    );
    assert!(
        state_test::diff::compare(&trusted.outcome, &observed.outcome).is_empty(),
        "declaring the control changed what its run produced",
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
