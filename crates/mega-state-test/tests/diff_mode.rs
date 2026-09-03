//! End-to-end tests for the differential runner and the keep-going fill.
//!
//! The classifier's decision table is unit-tested in `src/diff.rs`; these tests drive real
//! executions, so they cover the parts the table cannot: that the two specs are actually executed
//! and committed the way validation does, that the staged frame evidence reaches a halt no
//! transaction result exposes, that evidence a fixture authored itself buys it nothing, and that
//! a keep-going fill isolates one unit's failure from the rest of its file.

use mega_evm::{alloy_sol_types::SolError, revm::primitives::B256, MegaLimitExceeded};
use state_test::{
    diff::{
        collect_fixture_files, compare, diff_test_suite, diff_unit, execute_unit_outcome, judge,
        run_diff, DiffClass, DiffRunConfig, DiffSpecs, Mechanism, SpecOutcome,
    },
    runner::{
        execute_test_suite, fill_test_suite, fill_test_suite_keep_going, FixtureScan, UnitStatus,
    },
    types::{SpecName, TestUnit, TxPartIndices},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const SENDER: &str = "0x1000000000000000000000000000000000000001";
const CALLEE: &str = "0x2000000000000000000000000000000000000002";
const INNER: &str = "0x3000000000000000000000000000000000000003";

/// The single transaction vector these hand-built fixtures declare.
const VECTOR_0: TxPartIndices = TxPartIndices { data: 0, gas: 0, value: 0 };

/// `CALL(0 gas, 0x40..04, no value, no args, no return); POP` — runs out of gas partway.
///
/// Given a small enough allowance this frame halts, and it halts somewhere the two specs account
/// for differently: the opcode that crosses the frame's gas records nothing under Rex6, while
/// Rex7 settles the whole open segment at frame exit.
const INNER_RUNS_OUT: &str =
    "0x600060006000600060007340000000000000000000000000000000000000046000f150";

/// `CALL(<gas> gas, INNER, no value, no args, no return); POP` — no trailing `STOP`.
///
/// `CALL` pushes 0 on failure and the caller carries on, so nothing about the child's halt
/// reaches the transaction's own result.
fn call_inner_frag(gas: u16) -> String {
    format!("6000600060006000600073{}61{gas:04x}f150", &INNER[2..])
}

/// `CALL(<gas> gas, 0x08, no value, 1 byte of args, no return); POP` — no trailing `STOP`.
///
/// `0x08` is the bn128 pairing precompile, which rejects any input whose length is not a multiple
/// of 192. It never becomes an EVM frame, so the whole forwarded envelope is lost without being
/// executed: Rex7 books it as a destroyed remainder, and the caller absorbs the failure.
fn call_precompile_frag(gas: u16) -> String {
    format!("60006000600160006000600861{gas:04x}f150")
}

/// Wraps code fragments into a contract that runs them and stops.
fn contract(frags: &[String]) -> String {
    format!("0x{}00", frags.concat())
}

/// `MSTORE(0, <selector padded left>); REVERT(28, 4)` — a plain revert carrying four chosen bytes.
///
/// Nothing about this contract is a `MegaETH` mechanism. It writes the same four bytes `MegaETH`
/// writes when a frame-local resource limit is exceeded, which is all a classifier that reads
/// revert payloads as evidence would need to see.
fn revert_with_selector(selector: [u8; 4]) -> String {
    let word = u32::from_be_bytes(selector);
    format!("0x63{word:08x}6000526004601cfd")
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
    let (target, base) = DiffSpecs::SUPPORTED;
    DiffSpecs::new(target, base).expect("the supported pair")
}

fn rex7(unit: &TestUnit, collect_evidence: bool) -> SpecOutcome {
    execute_unit_outcome(unit, VECTOR_0, &SpecName::Rex7, collect_evidence).expect("rex7 executes")
}

fn rex6(unit: &TestUnit, collect_evidence: bool) -> SpecOutcome {
    execute_unit_outcome(unit, VECTOR_0, &SpecName::Rex6, collect_evidence).expect("rex6 executes")
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
        let json = unit_json(&contract(&[call_inner_frag(gas)]), Some(INNER_RUNS_OUT));
        let unit = parse_unit(&json);
        let (target, base) = (rex7(&unit, false), rex6(&unit, false));
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

/// A unit whose two specs disagree on the reported compute total, that books a Rex7 destroyed
/// remainder, and whose own transaction result is a plain success.
///
/// Two independent inner calls: a failing precompile, which loses its whole envelope without
/// executing it and so books the remainder, and a gas-starved inner frame, which is what actually
/// moves the reported total. Neither is visible from the transaction's own result, so before the
/// frame pass the remainder is the only thing on the table — exactly the situation in which a
/// derived number must not be allowed to certify itself.
///
/// Searched over the inner allowance for the same reason as [`inner_halt_json`]: the property is
/// that the shape exists, not that a particular gas number produces it.
fn destroyed_without_visible_halt_unit() -> TestUnit {
    for gas in 1..=64u16 {
        let code = contract(&[call_precompile_frag(2_000), call_inner_frag(gas)]);
        let unit = parse_unit(&unit_json(&code, Some(INNER_RUNS_OUT)));
        let (target, base) = (rex7(&unit, false), rex6(&unit, false));
        if target.status == "success" &&
            target.compute_gas_destroyed > 0 &&
            target.compute_gas_used != base.compute_gas_used
        {
            return unit;
        }
    }
    panic!("no forwarded-gas amount produced a destroyed remainder under a successful transaction")
}

// A transaction that stays inside every limit, ends no frame in an exceptional halt and trips no
// guard is bit-identical under Rex7 and Rex6 — the precision invariant's own statement, executed.
#[test]
fn test_within_limit_transaction_is_identical_under_both_specs() {
    let unit = parse_unit(&unit_json("0x", None));
    let outcome = diff_unit(&unit, VECTOR_0, rex7_over_rex6(), true);
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

    let without = diff_unit(&unit, VECTOR_0, rex7_over_rex6(), false);
    assert_eq!(
        without.class,
        DiffClass::Unexplained,
        "the transaction's own result hides the inner halt: {without:?}"
    );

    let with = diff_unit(&unit, VECTOR_0, rex7_over_rex6(), true);
    assert_eq!(with.class, DiffClass::Explained, "{with:?}");
    assert!(
        with.mechanisms.contains(&Mechanism::ExceptionalHalt),
        "frame evidence should name the halt: {:?}",
        with.mechanisms
    );
}

// A destroyed remainder is derived from a conservation law over the transaction's envelope, not
// observed. A missing term in that law produces a non-zero remainder with no halt behind it, so
// letting the remainder license the compute-total difference it causes would make that defect its
// own alibi. Here a real transaction books one and ends in a plain success: the remainder alone
// leaves the difference unexplained, and only the halted frame the inspector finds licenses it.
#[test]
fn test_destroyed_remainder_is_licensed_by_the_frame_not_by_itself() {
    let unit = destroyed_without_visible_halt_unit();

    let without = diff_unit(&unit, VECTOR_0, rex7_over_rex6(), false);
    assert_eq!(
        without.class,
        DiffClass::Unexplained,
        "a destroyed remainder must not certify the halt it claims: {without:?}"
    );
    assert!(
        without.mechanisms.contains(&Mechanism::DestroyedComputeGas),
        "the remainder is still reported: {:?}",
        without.mechanisms
    );

    let with = diff_unit(&unit, VECTOR_0, rex7_over_rex6(), true);
    assert_eq!(with.class, DiffClass::Explained, "{with:?}");
    assert!(
        with.mechanisms.contains(&Mechanism::ExceptionalHalt),
        "the independent witness is the frame the EVM finished: {:?}",
        with.mechanisms
    );
}

// Anti-vacuity control, and the reason evidence has to be bound to the execution. A contract that
// writes MegaETH's `MegaLimitExceeded` selector into its revert buffer is observed doing so — the
// claim is real and reported — but it claims the hypothesis that licenses *every* compared
// quantity, and it is four bytes any fixture can write. An unrelated difference laid over that
// execution stays UNEXPLAINED, so the sweep cannot be talked out of a finding by its own input.
#[test]
fn test_a_forged_limit_selector_buys_no_exemption() {
    let selector: [u8; 4] = MegaLimitExceeded::SELECTOR;
    let unit = parse_unit(&unit_json(&revert_with_selector(selector), None));

    // The forgery is a plain `REVERT`, and the inspector does see the four bytes.
    let observed = rex7(&unit, true);
    let frames = observed.frames.expect("the inspected pass collects frame evidence");
    assert!(
        frames.limit_revert_payloads > 0,
        "the forged selector should be observed and reported: {frames:?}"
    );
    assert_eq!(frames.halted, 0, "a plain revert is not a halt");

    // On its own the fixture is identical under both specs — the forgery changes nothing.
    let outcome = diff_unit(&unit, VECTOR_0, rex7_over_rex6(), true);
    assert_eq!(outcome.class, DiffClass::Pass, "{outcome:?}");

    // Lay an unrelated difference over the same real execution — one on a quantity only a changed
    // execution path can move. The forged claim is the only thing on the table, and it licenses
    // nothing, so the difference is still a finding.
    let mut target = observed.clone();
    target.state_root = B256::repeat_byte(9);
    target.gas_used += 1;
    let verdict = judge(&compare(&target, &observed), &target, &observed);
    assert_eq!(
        verdict.class,
        DiffClass::Unexplained,
        "bytes the fixture chose must not license a difference: {verdict:?}"
    );
    assert!(
        verdict.mechanisms.contains(&Mechanism::LimitRevertPayload),
        "the claim is still reported for a human triaging the finding: {:?}",
        verdict.mechanisms
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
    let outcome = diff_unit(&unit, VECTOR_0, rex7_over_rex6(), true);
    assert_eq!(outcome.class, DiffClass::Skipped, "{outcome:?}");
}

/// A unit with two transaction vectors: `data[1]` is a non-empty calldata, and the two `post`
/// entries name index 0 and index 1.
fn two_vector_json() -> serde_json::Value {
    let mut json = unit_json("0x", None);
    json["transaction"]["data"] = serde_json::json!(["0x", "0xdeadbeef"]);
    let entry = |data: usize| {
        serde_json::json!({
            "indexes": { "data": data, "gas": 0, "value": 0 },
            "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "logs": "0x0000000000000000000000000000000000000000000000000000000000000000"
        })
    };
    json["post"] = serde_json::json!({ "Rex6": [entry(0), entry(1)] });
    json
}

// A unit is a family of transactions, one per vector its `post` names. Judging only index
// `{0,0,0}` would report a green unit while never running the rest of it, and every count the
// sweep prints would be short by the vectors it skipped.
#[test]
fn test_every_declared_vector_is_judged() {
    let unit = parse_unit(&two_vector_json());
    assert_eq!(unit.vectors().len(), 2, "the fixture declares two vectors");

    let path = write_suite("two_vectors.json", &[("family", two_vector_json())]);
    let diffs = diff_test_suite(&path, rex7_over_rex6(), true).expect("diff suite");
    assert_eq!(diffs.len(), 2, "one verdict per vector: {diffs:?}");
    let mut names: Vec<&str> = diffs.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["family[d=0,g=0,v=0]", "family[d=1,g=0,v=0]"]);

    // The two vectors send different calldata, so they are genuinely different transactions and
    // not the same one counted twice.
    let gas: Vec<u64> = unit
        .vectors()
        .into_iter()
        .map(|v| execute_unit_outcome(&unit, v, &SpecName::Rex7, false).expect("executes").gas_used)
        .collect();
    assert_ne!(gas[0], gas[1], "calldata cost should differ between the vectors");
}

// `--fill --force` exists to overwrite a stale expectation. Collapsing the `post` map to a single
// `{0,0,0}` entry would make it delete the other vectors' expectations too, and silently: the
// file still parses, still validates, and covers less than it did.
#[test]
fn test_fill_records_one_expectation_per_vector() {
    let path = write_suite("fill_two_vectors.json", &[("family", two_vector_json())]);
    let report =
        fill_test_suite_keep_going(&path, Some(SpecName::Rex7), true).expect("fill the file");
    assert_eq!(report.filled(), 2, "the tally counts vectors, and this unit declares two");

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    let post = &written["family"]["post"]["Rex7"];
    assert_eq!(post.as_array().map(Vec::len), Some(2), "both vectors kept: {post}");
    assert_eq!(post[0]["indexes"], serde_json::json!({ "data": 0, "gas": 0, "value": 0 }));
    assert_eq!(post[1]["indexes"], serde_json::json!({ "data": 1, "gas": 0, "value": 0 }));
    assert_ne!(
        post[0]["megaGasUsed"], post[1]["megaGasUsed"],
        "each entry records its own vector's execution"
    );
}

/// `CALLDATACOPY(0, 0, CALLDATASIZE); RETURN(0, CALLDATASIZE)` — returns whatever it was called
/// with, so two vectors that send different calldata produce different output.
const ECHO_CALLDATA: &str = "0x366000600037366000f3";

/// `MSTORE(0, 42); RETURN(0, 32)` — returns the same word whatever it was called with.
const RETURN_CONSTANT: &str = "0x602a60005260206000f3";

/// Re-runs validation over a fixture and returns how many expectations it checked.
fn validate(path: &Path) -> usize {
    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    execute_test_suite(path, &elapsed, false, false).expect("the filled fixture self-validates")
}

/// [`two_vector_json`] with a chosen callee, so the two vectors' outputs can be made to agree or
/// to differ.
fn two_vector_json_with_callee(code: &str) -> serde_json::Value {
    let mut json = two_vector_json();
    json["pre"][CALLEE]["code"] = serde_json::json!(code);
    json
}

// `out` is one field for the whole unit, and a multi-vector unit only has an output to record
// when its vectors agree on one. Clearing it unconditionally drops an expectation the fixture was
// entitled to — quietly, since the result still parses and still validates.
#[test]
fn test_fill_keeps_a_multi_vector_out_when_the_vectors_agree() {
    let path = write_suite(
        "fill_out_agree.json",
        &[("family", two_vector_json_with_callee(RETURN_CONSTANT))],
    );
    let report =
        fill_test_suite_keep_going(&path, Some(SpecName::Rex7), true).expect("fill the file");
    assert_eq!(report.filled(), 2);

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    assert_eq!(
        written["family"]["out"],
        serde_json::json!("0x000000000000000000000000000000000000000000000000000000000000002a"),
        "both vectors return that word, so the unit has one output: {written}"
    );
    // And the recorded output is checked, rather than merely stored.
    assert_eq!(validate(&path), 2);
}

// The other half: vectors that return different outputs have no `out` this schema can express.
// Recording one vector's output would assert it for every vector, and per-vector output is a
// schema change, so the unit is refused with a reason instead of filled with a claim.
#[test]
fn test_fill_refuses_a_multi_vector_unit_whose_outputs_disagree() {
    let unit = two_vector_json_with_callee(ECHO_CALLDATA);
    let path = write_suite("fill_out_disagree.json", &[("family", unit)]);
    let before = std::fs::read_to_string(&path).expect("read");

    let report =
        fill_test_suite_keep_going(&path, Some(SpecName::Rex7), true).expect("fill the file");
    assert_eq!(report.filled(), 0, "{:?}", report.vectors);
    assert_eq!(report.failed(), 2, "the refusal covers every vector of the unit");
    for vector in &report.vectors {
        let message = vector.status.message().expect("a refused vector carries its reason");
        assert!(
            message.contains("different output"),
            "the reason should name what could not be recorded: {message}"
        );
    }
    assert_eq!(std::fs::read_to_string(&path).expect("read"), before, "file must be untouched");
}

// Fill and diff sweep the same corpus and print a total each. Counting units in one and vectors in
// the other makes those totals disagree over any multi-vector fixture — and a tally that cannot be
// compared against the other mode's, or against a baseline taken under the other mode, is a number
// nobody can act on.
#[test]
fn test_fill_and_diff_count_the_same_vectors() {
    let units = [
        ("family", two_vector_json()),
        ("single", unit_json("0x", None)),
        ("another_family", two_vector_json()),
    ];
    let path = write_suite("count_parity.json", &units);

    let diffs = diff_test_suite(&path, rex7_over_rex6(), true).expect("diff suite");
    let report =
        fill_test_suite_keep_going(&path, Some(SpecName::Rex7), true).expect("fill the file");

    assert_eq!(report.vectors.len(), 5, "two units of two vectors and one of one");
    assert_eq!(report.vectors.len(), diffs.len(), "the two modes count the same things");

    let mut filled: Vec<&str> = report.vectors.iter().map(|v| v.name.as_str()).collect();
    let mut judged: Vec<&str> = diffs.iter().map(|d| d.name.as_str()).collect();
    filled.sort_unstable();
    judged.sort_unstable();
    assert_eq!(filled, judged, "and name them the same way");
}

// Every pair but one is refused at construction, and the constructor is the only way to build the
// value: the fields it validates are private, which this file — compiled as a consumer of the
// crate — can only observe through the accessors. The compile-time half of that is pinned by the
// `compile_fail` example on `DiffSpecs`.
#[test]
fn test_diff_specs_is_only_reachable_through_its_constructor() {
    let (target, base) = DiffSpecs::SUPPORTED;
    let specs = DiffSpecs::new(target, base).expect("the supported pair");
    assert_eq!((specs.target(), specs.base()), (SpecName::Rex7, SpecName::Rex6));

    for (t, b) in [
        (SpecName::Rex7, SpecName::Equivalence),
        (SpecName::Rex6, SpecName::Rex5),
        (SpecName::Rex6, SpecName::Rex7),
        (SpecName::Rex7, SpecName::Rex7),
    ] {
        let err = DiffSpecs::new(t, b).expect_err("only one pair has an invariant");
        assert!(err.contains("Rex7") && err.contains("Rex6"), "name the supported pair: {err}");
    }
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
    let failed = report.vectors.iter().find(|v| !v.status.is_ok()).expect("one vector failed");
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

// A part of the corpus the discovery walk could not read is a hole in coverage, and it reaches
// the gate as a smaller file list — every count still truthful, every count short. `run_diff`
// carries those errors into the tally so the run fails instead of grading the part it reached.
#[test]
fn test_an_unreadable_part_of_the_corpus_fails_the_run() {
    let path = write_suite("scan_errors.json", &[("quiet", unit_json("0x", None))]);
    let clean = FixtureScan { files: vec![path.clone()], errors: vec![] };
    let config = DiffRunConfig {
        specs: rex7_over_rex6(),
        single_thread: true,
        collect_evidence: true,
        progress: false,
    };

    let tally = run_diff(clean, config);
    assert_eq!(tally.count(DiffClass::Pass), 1);
    assert!(!tally.is_failure(), "a corpus the sweep read in full and passed");

    let partial =
        FixtureScan { files: vec![path], errors: vec!["walk /corpus/sub: denied".to_string()] };
    let tally = run_diff(partial, config);
    assert_eq!(tally.count(DiffClass::Pass), 1, "the readable part still runs");
    assert_eq!(tally.file_errors.len(), 1, "and the unreadable part is reported");
    assert!(tally.is_failure(), "a partly-read corpus is not a pass");
}

// A directory whose contents cannot be listed yields no fixtures, which is indistinguishable from
// a directory that holds none. The walk has to report it.
#[test]
#[cfg(unix)]
fn test_discovery_reports_a_directory_it_cannot_read() {
    use std::os::unix::fs::PermissionsExt;

    let root = std::env::temp_dir().join("mega_state_test_unreadable_scan");
    let _ = std::fs::remove_dir_all(&root);
    let locked = root.join("locked");
    std::fs::create_dir_all(&locked).expect("mkdir");
    std::fs::write(locked.join("hidden.json"), "{}").expect("write");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

    let unreadable = std::fs::read_dir(&locked).is_err();
    let scan = state_test::runner::find_all_json_tests(&root);

    // Restore before asserting, so a failure does not leave an unreadable directory behind.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod back");
    let _ = std::fs::remove_dir_all(&root);

    // Running as root defeats the permission bits; then there is nothing to detect and the walk
    // legitimately finds the file.
    if unreadable {
        assert!(scan.files.is_empty(), "the fixture is behind the locked directory");
        assert!(!scan.errors.is_empty(), "the unreadable directory must be reported");
    } else {
        assert_eq!(scan.files.len(), 1, "readable after all (running as root?)");
    }
}

// The path-level guards the differential run relies on before it judges anything.
#[test]
fn test_collect_fixture_files_rejects_a_corpus_with_nothing_in_it() {
    let missing = std::env::temp_dir().join("mega_state_test_no_such_corpus_4928");
    let _ = std::fs::remove_dir_all(&missing);
    assert!(
        collect_fixture_files(std::slice::from_ref(&missing)).is_err(),
        "a path that does not exist"
    );

    std::fs::create_dir_all(&missing).expect("mkdir");
    assert!(
        collect_fixture_files(std::slice::from_ref(&missing)).is_err(),
        "a directory with no fixtures"
    );
    let _ = std::fs::remove_dir_all(&missing);
}
