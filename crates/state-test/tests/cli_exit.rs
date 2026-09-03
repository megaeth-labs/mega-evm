//! Exit-code contract tests for the `state-test` CLI.
//!
//! The runner library no longer calls `std::process::exit`; the CLI owns the
//! contract CI relies on: failing tests (or any error) → stderr message +
//! exit code 1, passing run → exit code 0.

use std::{path::PathBuf, process::Command};

/// A unit whose execution succeeds but whose recorded roots are wrong, so
/// validation must fail.
const FAILING_SUITE: &str = r#"{
    "exit_code_test": {
        "env": {
            "currentChainID": "0x18c6",
            "currentCoinbase": "0x3000000000000000000000000000000000000003",
            "currentDifficulty": "0x0",
            "currentGasLimit": "0x1c9c380",
            "currentNumber": "0x10",
            "currentTimestamp": "0x3e8",
            "currentBaseFee": "0x0",
            "currentRandom": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "currentExcessBlobGas": "0x0"
        },
        "pre": {
            "0x1000000000000000000000000000000000000001": {
                "balance": "0xde0b6b3a7640000",
                "code": "0x",
                "nonce": "0x0",
                "storage": {}
            }
        },
        "transaction": {
            "type": 0,
            "data": ["0x"],
            "gasLimit": ["0x30d40"],
            "gasPrice": "0x0",
            "nonce": "0x0",
            "secretKey": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "sender": "0x1000000000000000000000000000000000000001",
            "to": "0x1000000000000000000000000000000000000001",
            "value": ["0x0"]
        },
        "post": {
            "Rex5": [{
                "indexes": { "data": 0, "gas": 0, "value": 0 },
                "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "logs": "0x0000000000000000000000000000000000000000000000000000000000000000"
            }]
        }
    }
}"#;

fn write_fixture(file_name: &str, contents: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("state_test_cli_exit");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join(file_name);
    std::fs::write(&path, contents).expect("write fixture");
    path
}

fn run_cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_state-test")).args(args).output().expect("spawn state-test")
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn combined(out: &std::process::Output) -> String {
    format!("{}\n{}", stdout(out), stderr(out))
}

#[test]
fn test_failing_tests_exit_with_code_1() {
    let path = write_fixture("failing.json", FAILING_SUITE);
    let path = path.to_str().expect("utf8 path");

    // `--keep-going` exercises the `TestsFailed` summary path (no thread error).
    let out = run_cli(&[path, "--keep-going", "--json-outcome"]);
    assert_eq!(out.status.code(), Some(1), "failing tests must exit 1 (keep-going)");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("failed"),
        "stderr should report the failure"
    );

    // Without `--keep-going` the first thread error propagates; still exit 1.
    let out = run_cli(&[path, "--json-outcome"]);
    assert_eq!(out.status.code(), Some(1), "failing tests must exit 1 (fail-fast)");
}

#[test]
fn test_invalid_path_exits_with_code_1() {
    let out = run_cli(&["/nonexistent/state_test_cli_exit_4928"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!out.stderr.is_empty(), "stderr should carry the error message");
}

#[test]
fn test_passing_run_exits_with_code_0() {
    // A fixture whose recorded roots are the ones its execution produces. `--fill` computes them,
    // which is also what makes this a run with something in it to pass: the expectation exists and
    // is checked.
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    suite["exit_code_test"]["post"] = serde_json::json!({});
    let path = write_fixture("passing.json", &serde_json::to_string(&suite).expect("serialize"));
    let path = path.to_str().expect("utf8 path");

    let out = run_cli(&[path, "--fill", "--bench-spec", "Rex5"]);
    assert_eq!(out.status.code(), Some(0), "fill must succeed: {}", stderr(&out));

    let out = run_cli(&[path]);
    assert_eq!(out.status.code(), Some(0), "passing run must exit 0: {}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("All tests passed!"),
        "and say so: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn test_validate_run_over_a_unit_with_no_expectation_exits_1() {
    // A unit whose `post` is empty is walked, executed against nothing, and counted by nothing.
    // Reading that as a pass makes "the runner checked this file" true of a file that pins no
    // behavior at all — and `--fill --force` writing an empty `post` is one bug away.
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    suite["exit_code_test"]["post"] = serde_json::json!({});
    let path = write_fixture("no_expectation.json", &serde_json::to_string(&suite).expect("ser"));

    let out = run_cli(&[path.to_str().expect("utf8 path")]);
    assert_eq!(out.status.code(), Some(1), "a unit that pins nothing is not a passing run");
    assert!(
        stderr(&out).contains("no fixture expectation was validated"),
        "stderr should say what was missing: {}",
        stderr(&out)
    );

    // The same file with a `post` that holds an empty vector list: a unit, a spec, and still no
    // expectation to check.
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    suite["exit_code_test"]["post"] = serde_json::json!({ "Rex5": [] });
    let path = write_fixture("empty_vector_list.json", &serde_json::to_string(&suite).expect("s"));
    let out = run_cli(&[path.to_str().expect("utf8 path")]);
    assert_eq!(out.status.code(), Some(1), "an empty vector list judges nothing either");
}

#[test]
fn test_fill_that_filled_nothing_exits_1() {
    // `--keep-going` decides when a run stops, not whether an empty one counts. Without it the
    // fill loop simply has nothing to fail at, so a corpus that never arrived walks no file,
    // writes no fixture, and used to exit 0 — the one report that must never read as a pass.
    let dir = std::env::temp_dir().join("state_test_cli_exit_empty_fill");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let dir_arg = dir.to_str().expect("utf8 path");

    for args in [
        vec![dir_arg, "--fill", "--bench-spec", "Rex7"],
        vec![dir_arg, "--fill", "--keep-going", "--bench-spec", "Rex7"],
    ] {
        let out = run_cli(&args);
        assert_eq!(out.status.code(), Some(1), "a fill that filled nothing must fail: {args:?}");
        assert!(
            stderr(&out).contains("no transaction vector was filled"),
            "stderr should say what was missing: {}",
            stderr(&out)
        );
    }

    // A corpus of nothing but files the runner skips by name reaches the fill loop and still
    // fills nothing.
    std::fs::write(dir.join("ValueOverflow.json"), FAILING_SUITE).expect("write");
    let out = run_cli(&[dir_arg, "--fill", "--keep-going", "--force", "--bench-spec", "Rex7"]);
    assert_eq!(out.status.code(), Some(1), "a skipped-only corpus fills nothing");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_fill_tally_counts_transaction_vectors() {
    // The tally a sweep gates on has to count what the differential sweep counts, or the two
    // numbers cannot be compared with each other or against a baseline recorded under the other
    // mode. A unit is a family of transactions; the vector is the unit both modes agree on.
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    suite["exit_code_test"]["transaction"]["data"] = serde_json::json!(["0x", "0xdeadbeef"]);
    let entry = |data: usize| {
        serde_json::json!({
            "indexes": { "data": data, "gas": 0, "value": 0 },
            "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "logs": "0x0000000000000000000000000000000000000000000000000000000000000000"
        })
    };
    suite["exit_code_test"]["post"] = serde_json::json!({ "Rex5": [entry(0), entry(1)] });
    let path = write_fixture("tally_vectors.json", &serde_json::to_string(&suite).expect("ser"));
    let path = path.to_str().expect("utf8 path");

    let out = run_cli(&[path, "--fill", "--force", "--keep-going", "--bench-spec", "Rex7"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Fill tally: OK=2 ERR=0 PANIC=0 FILE_ERR=0 SKIP_FILE=0 TOTAL=2"),
        "one unit, two vectors, two filled: {stdout}"
    );

    // The differential sweep over the same fixture reports the same total.
    let out = run_cli(&[path, "--bench-spec", "Rex7", "--diff-spec", "Rex6"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("over 2 unit(s)"), "the same two vectors: {stdout}");
}

#[test]
fn test_diff_run_with_no_unexplained_difference_exits_with_code_0() {
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    // The differential run computes both sides itself; the recorded `post` is irrelevant, and an
    // empty one keeps the fixture honest about that.
    suite["exit_code_test"]["post"] = serde_json::json!({});
    let path = write_fixture("diff_pass.json", &serde_json::to_string(&suite).expect("serialize"));
    let report = write_fixture("diff_pass_report.json", "");

    let out = run_cli(&[
        path.to_str().expect("utf8 path"),
        "--bench-spec",
        "Rex7",
        "--diff-spec",
        "Rex6",
        "--diff-report",
        report.to_str().expect("utf8 path"),
    ]);
    assert_eq!(out.status.code(), Some(0), "a run with no unexplained difference must exit 0");

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("read report"))
            .expect("report is json");
    assert_eq!(written["targetSpec"], "Rex7");
    assert_eq!(written["baseSpec"], "Rex6");
    assert_eq!(written["classes"]["PASS"], 1);
    assert_eq!(written["classes"]["UNEXPLAINED"], 0);
}

#[test]
fn test_diff_run_over_an_unauthorized_spec_pair_is_refused() {
    // Every rule in the classifier is a reading of one sentence, the Rex7 precision invariant,
    // which relates Rex7 to Rex6 and states nothing about any other pair. Pointed at another pair
    // it would grant a licence that pair never had — deciding, from mechanisms that are evidence
    // for nothing there, that a difference is fine. It refuses instead of judging.
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    suite["exit_code_test"]["post"] = serde_json::json!({});
    let path = write_fixture("diff_pair.json", &serde_json::to_string(&suite).expect("serialize"));
    let path = path.to_str().expect("utf8 path");

    for (target, base) in [("Rex7", "Equivalence"), ("Rex6", "Rex5"), ("Rex6", "Rex7")] {
        let out = run_cli(&[path, "--bench-spec", target, "--diff-spec", base]);
        assert_eq!(out.status.code(), Some(1), "{target} vs {base} must be refused");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Rex7") && stderr.contains("Rex6"),
            "the error should name the one supported pair: {stderr}"
        );
    }
}

#[test]
fn test_validate_run_that_judged_nothing_exits_1() {
    // Same hole as in the differential mode, one mode over: a corpus whose every file is on the
    // validation skip list walks files, reaches no unit, and reports zero errors.
    let dir = std::env::temp_dir().join("state_test_cli_exit_all_skipped");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("ValueOverflow.json"), FAILING_SUITE).expect("write");

    let out = run_cli(&[dir.to_str().expect("utf8 path")]);
    assert_eq!(out.status.code(), Some(1), "a run that validated nothing must fail");
    assert!(
        stderr(&out).contains("no fixture expectation was validated"),
        "stderr should say what was missing: {}",
        stderr(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_diff_run_that_judged_nothing_exits_1() {
    // A sweep whose corpus never arrived reaches the gate with an empty tally: zero panics, zero
    // unexplained differences, every count truthful and meaningless. It must not read as a pass.
    let dir = std::env::temp_dir().join("state_test_cli_exit_empty_corpus");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    let out =
        run_cli(&[dir.to_str().expect("utf8 path"), "--bench-spec", "Rex7", "--diff-spec", "Rex6"]);
    assert_eq!(out.status.code(), Some(1), "a corpus with no fixture in it must fail");
    let empty = combined(&out);
    assert!(
        empty.contains("no JSON test files found") || empty.contains("no fixture was judged"),
        "an empty corpus must say nothing was judged: {empty}"
    );
    assert!(
        !empty.contains("Error: 0 tests failed"),
        "must not claim zero failures while exiting 1: {empty}"
    );

    // A directory holding only fixtures on the validation skip list reaches the runner but judges
    // no unit, which is the same hole one step further in.
    std::fs::write(dir.join("ValueOverflow.json"), FAILING_SUITE).expect("write");
    let out =
        run_cli(&[dir.to_str().expect("utf8 path"), "--bench-spec", "Rex7", "--diff-spec", "Rex6"]);
    assert_eq!(out.status.code(), Some(1), "a sweep that judged no unit must fail");
    let skipped = combined(&out);
    assert!(
        skipped.contains("no fixture was judged"),
        "a sweep that judged no unit must say so: {skipped}"
    );
    assert!(
        !skipped.contains("Error: 0 tests failed"),
        "must not claim zero failures while exiting 1: {skipped}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_diff_run_with_an_unparseable_fixture_exits_1() {
    // A file the sweep cannot parse is a fixture it did not judge. Skipping it quietly is how a
    // corpus shrinks without anyone noticing.
    let dir = std::env::temp_dir().join("state_test_cli_exit_bad_fixture");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut suite: serde_json::Value = serde_json::from_str(FAILING_SUITE).expect("parse");
    suite["exit_code_test"]["post"] = serde_json::json!({});
    std::fs::write(dir.join("good.json"), serde_json::to_string(&suite).expect("serialize"))
        .expect("write");
    std::fs::write(dir.join("broken.json"), "{ not json").expect("write");

    let out =
        run_cli(&[dir.to_str().expect("utf8 path"), "--bench-spec", "Rex7", "--diff-spec", "Rex6"]);
    let stdout = stdout(&out);
    let report = combined(&out);
    assert!(
        stdout.lines().any(|l| l.split_whitespace().eq(["PASS", "1"])),
        "the readable fixture still runs: {stdout}"
    );
    assert!(stdout.contains("FILE_ERROR"), "the unreadable one is reported: {stdout}");
    assert!(
        report.contains("1 fixtures unreadable"),
        "the summary must count unreadable fixtures: {report}"
    );
    assert!(
        !report.contains("Error: 0 tests failed"),
        "must not claim zero failures while exiting 1: {report}"
    );
    assert_eq!(out.status.code(), Some(1), "a corpus the sweep only partly read is not a pass");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_diff_spec_requires_an_explicit_target_spec() {
    let path = write_fixture("diff_needs_target.json", FAILING_SUITE);
    let out = run_cli(&[path.to_str().expect("utf8 path"), "--diff-spec", "Rex6"]);
    assert_eq!(out.status.code(), Some(2), "clap rejects the incomplete flag combination");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("bench-spec"),
        "the error should name the missing flag"
    );
}

#[test]
fn test_diff_spec_rejects_an_unknown_spec_name() {
    let path = write_fixture("diff_bad_spec.json", FAILING_SUITE);
    let out = run_cli(&[
        path.to_str().expect("utf8 path"),
        "--bench-spec",
        "Rex7",
        "--diff-spec",
        "FutureFork9000",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--diff-spec"),
        "the error should name the offending flag"
    );
}
