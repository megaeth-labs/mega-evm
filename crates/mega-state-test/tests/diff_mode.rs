//! End-to-end tests for the differential runner and the keep-going fill.
//!
//! The classifier's decision table is unit-tested in `src/diff.rs`; these tests drive real
//! executions, so they cover the parts the table cannot: that the two specs are actually executed
//! and committed the way validation does, that the staged frame evidence reaches a halt no
//! transaction result exposes, and that a keep-going fill isolates one unit's failure from the
//! rest of its file.

use state_test::{
    diff::{diff_test_suite, diff_unit, execute_unit_outcome, DiffClass, DiffSpecs, Mechanism},
    runner::{fill_test_suite, fill_test_suite_keep_going, UnitStatus},
    types::{SpecName, TestUnit},
};
use std::path::PathBuf;

const SENDER: &str = "0x1000000000000000000000000000000000000001";
const CALLEE: &str = "0x2000000000000000000000000000000000000002";
const INNER: &str = "0x3000000000000000000000000000000000000003";

/// `CALL(0 gas, 0x40..04, no value, no args, no return); POP` — runs out of gas partway.
///
/// Given a small enough allowance this frame halts, and it halts somewhere the two specs account
/// for differently: the opcode that crosses the frame's gas records nothing under Rex6, while
/// Rex7 settles the whole open segment at frame exit.
const INNER_RUNS_OUT: &str =
    "0x600060006000600060007340000000000000000000000000000000000000046000f150";

/// `CALL(<gas> gas, INNER, no value, no args, no return); POP; STOP`.
///
/// `CALL` pushes 0 on failure and this frame carries on to a normal `STOP`, so nothing about the
/// child's halt reaches the transaction's own result.
fn call_into_inner(gas: u16) -> String {
    format!("0x6000600060006000600073{}61{gas:04x}f1500000", &INNER[2..])
}

/// A unit whose transaction calls `CALLEE`, with an optional third account at `INNER`.
fn unit_json(callee_code: &str, inner_code: Option<&str>) -> serde_json::Value {
    let mut pre = serde_json::json!({
        SENDER: { "balance": "0xde0b6b3a7640000", "code": "0x", "nonce": "0x0", "storage": {} },
        CALLEE: { "balance": "0x0", "code": callee_code, "nonce": "0x0", "storage": {} },
    });
    if let Some(code) = inner_code {
        pre[INNER] =
            serde_json::json!({ "balance": "0x0", "code": code, "nonce": "0x0", "storage": {} });
    }
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
        "pre": pre,
        "transaction": {
            "type": 0,
            "data": ["0x"],
            "gasLimit": ["0x30d40"],
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

fn parse_unit(json: &serde_json::Value) -> TestUnit {
    serde_json::from_value(json.clone()).expect("valid unit json")
}

fn rex7_over_rex6() -> DiffSpecs {
    DiffSpecs { target: SpecName::Rex7, base: SpecName::Rex6 }
}

/// Writes a suite of named units to a unique temp file and returns its path.
fn write_suite(file_name: &str, units: &[(&str, serde_json::Value)]) -> PathBuf {
    let suite: serde_json::Map<String, serde_json::Value> =
        units.iter().map(|(n, u)| ((*n).to_string(), u.clone())).collect();
    let dir = std::env::temp_dir().join("mega_state_test_diff_mode");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(file_name);
    std::fs::write(&path, serde_json::to_string_pretty(&suite).expect("serialize"))
        .expect("write fixture");
    path
}

/// A unit whose inner frame runs out of gas somewhere the two specs account for differently.
///
/// The exact allowance that lands on such an opcode is a function of the gas schedule, so it is
/// searched for rather than hard-coded: the property under test is that *some* inner halt moves
/// the reported compute total while leaving the transaction's own result untouched, not that a
/// particular gas number does.
fn inner_halt_json() -> serde_json::Value {
    for gas in 1..=64u16 {
        let json = unit_json(&call_into_inner(gas), Some(INNER_RUNS_OUT));
        let unit = parse_unit(&json);
        let target = execute_unit_outcome(&unit, &SpecName::Rex7, false).expect("rex7 executes");
        let base = execute_unit_outcome(&unit, &SpecName::Rex6, false).expect("rex6 executes");
        let hidden = target.status == "success" && target.compute_gas_destroyed == 0;
        if hidden && target.compute_gas_used != base.compute_gas_used {
            return json;
        }
    }
    panic!("no forwarded-gas amount produced an inner halt that moves the reported compute total")
}

fn inner_halt_unit() -> TestUnit {
    parse_unit(&inner_halt_json())
}

// A transaction that stays inside every limit, ends no frame in an exceptional halt and trips no
// guard is bit-identical under Rex7 and Rex6 — the precision invariant's own statement, executed.
#[test]
fn test_within_limit_transaction_is_identical_under_both_specs() {
    let unit = parse_unit(&unit_json("0x", None));
    let outcome = diff_unit(&unit, rex7_over_rex6(), true);
    assert_eq!(outcome.class, DiffClass::Pass, "{outcome:?}");
    assert!(outcome.fields.is_empty());
}

// The staged evidence pass exists for exactly this shape: an inner frame runs out of gas, its
// caller absorbs the failure and returns normally, and the interpreter left nothing to destroy.
// The transaction's own result is a plain success, so only the frame the EVM finished shows the
// halt — and without that, a real and correct Rex7 deviation reads as a defect.
#[test]
fn test_inner_frame_halt_is_explained_only_with_frame_evidence() {
    let unit = inner_halt_unit();

    let without = diff_unit(&unit, rex7_over_rex6(), false);
    assert_eq!(
        without.class,
        DiffClass::Unexplained,
        "the transaction's own result hides the inner halt: {without:?}"
    );

    let with = diff_unit(&unit, rex7_over_rex6(), true);
    assert_eq!(with.class, DiffClass::Explained, "{with:?}");
    assert!(
        with.mechanisms.contains(&Mechanism::ExceptionalHalt),
        "frame evidence should name the halt: {:?}",
        with.mechanisms
    );
}

// The whole file is judged, one verdict per unit, and a unit's verdict is attributed to its own
// name — a sweep that mislabels which fixture differed is unusable for triage.
#[test]
fn test_diff_test_suite_reports_one_verdict_per_unit() {
    let path = write_suite(
        "two_units.json",
        &[("quiet", unit_json("0x", None)), ("inner_halt", inner_halt_json())],
    );
    let diffs = diff_test_suite(&path, rex7_over_rex6(), true).expect("diff suite");
    assert_eq!(diffs.len(), 2);
    let quiet = diffs.iter().find(|d| d.name == "quiet").expect("quiet unit");
    let halting = diffs.iter().find(|d| d.name == "inner_halt").expect("halting unit");
    assert_eq!(quiet.class, DiffClass::Pass);
    assert_eq!(halting.class, DiffClass::Explained);
}

// A fixture the runner declines on both sides says nothing about either spec: the gas limit here
// is below the intrinsic cost both specs charge, so neither executes anything.
#[test]
fn test_transaction_rejected_by_both_specs_is_skipped() {
    let mut json = unit_json("0x", None);
    json["transaction"]["gasLimit"] = serde_json::json!(["0x1"]);
    let unit = parse_unit(&json);
    let outcome = diff_unit(&unit, rex7_over_rex6(), true);
    assert_eq!(outcome.class, DiffClass::Skipped, "{outcome:?}");
}

// Keep-going fill: one unit's failure must cost that unit, not the units after it in the same
// file. Without this, a corpus sweep has to split every multi-unit fixture first.
#[test]
fn test_keep_going_fill_isolates_one_unit_failure() {
    // The middle unit's gas limit is below the intrinsic cost, so filling it fails.
    let mut broken = unit_json("0x", None);
    broken["transaction"]["gasLimit"] = serde_json::json!(["0x1"]);
    let path = write_suite(
        "keep_going.json",
        &[("a_ok", unit_json("0x", None)), ("b_broken", broken), ("c_ok", unit_json("0x", None))],
    );

    // Without keep-going the whole file aborts at the broken unit and nothing is written.
    let before = std::fs::read_to_string(&path).expect("read");
    let err = fill_test_suite(&path, Some(SpecName::Rex7), true).expect_err("must abort");
    assert!(err.to_string().contains("b_broken"), "error should name the unit: {err}");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), before, "file must be untouched");

    let report = fill_test_suite_keep_going(&path, Some(SpecName::Rex7), true).expect("fill");
    assert_eq!(report.filled(), 2);
    assert_eq!(report.failed(), 1);
    let failed = report.units.iter().find(|u| !u.status.is_ok()).expect("one unit failed");
    assert_eq!(failed.name, "b_broken");
    assert!(matches!(failed.status, UnitStatus::Error(_)), "{:?}", failed.status);

    // The two good units carry a freshly recorded Rex7 expectation; the failed one keeps its
    // original (empty) post rather than a half-written one.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    assert!(written["a_ok"]["post"]["Rex7"].is_array());
    assert!(written["c_ok"]["post"]["Rex7"].is_array());
    assert_eq!(written["b_broken"]["post"], serde_json::json!({}));
}
