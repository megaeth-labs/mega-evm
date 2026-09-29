//! `--override.limits` on `run` and `tx`: a Satin transaction held to protocol limits other than
//! the chain's, the fields the override names replacing the chain's and every other staying.

use std::process::Command;

use serde_json::{json, Value};

/// Two fresh `SSTORE`s, keeping two write records: 310 bytes of body and 40 per record.
const TWO_WRITES: &str = "0x60016000556001600155";

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Output {
    fn json(&self) -> Value {
        assert_eq!(self.code, 0, "{}", self.stderr);
        serde_json::from_str(&self.stdout).unwrap()
    }
}

fn evme(args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme")).args(args).output().unwrap();
    Output {
        code: output.status.code().unwrap(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// `run --spec Satin` of [`TWO_WRITES`], under `limits` when given.
fn run_two_writes(limits: Option<&str>) -> Value {
    let mut args = vec!["run", TWO_WRITES, "--spec", "Satin", "--json"];
    if let Some(limits) = limits {
        args.extend(["--override.limits", limits]);
    }
    evme(&args).json()
}

/// Without an override the run keeps both writes; an override of the transaction data size or of
/// its write records stops it at the second write, and takes both back.
#[test]
fn test_an_override_holds_a_satin_run_to_other_limits() {
    let kept = run_two_writes(None);
    assert_eq!(kept["success"], true);
    assert_eq!(kept["satin"]["data_size"], 390);
    assert_eq!(kept["satin"]["write_records"], 2);
    assert_eq!(kept["satin"]["limit_exceeded"], Value::Null);

    let data_size = run_two_writes(Some(r#"{"txRuntimeLimits":{"txDataSizeLimit":350}}"#));
    assert_eq!(data_size["success"], false);
    assert_eq!(
        data_size["satin"]["limit_exceeded"],
        json!({ "kind": "data_size", "limit": 350, "used": 390 })
    );
    assert_eq!(data_size["satin"]["write_records"], 0, "the stop takes the writes back");

    let records = run_two_writes(Some(r#"{"txRuntimeLimits":{"txKvUpdateLimit":1}}"#));
    assert_eq!(
        records["satin"]["limit_exceeded"],
        json!({ "kind": "kv_update", "limit": 1, "used": 2 })
    );
}

/// The override is read from a file as it is given inline.
#[test]
fn test_an_override_is_read_from_a_file() {
    let limits = r#"{"txRuntimeLimits":{"txDataSizeLimit":350}}"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limits.json");
    std::fs::write(&path, limits).unwrap();
    let mut from_file = run_two_writes(Some(path.to_str().unwrap()));
    let mut inline = run_two_writes(Some(limits));
    for output in [&mut from_file, &mut inline] {
        output.as_object_mut().unwrap().remove("exec_time");
    }
    assert_eq!(from_file, inline);
}

/// `tx` takes the override too: a body one byte over the transaction data-size limit is stopped
/// before it runs.
#[test]
fn test_an_override_holds_a_satin_tx_to_other_limits() {
    let args = ["tx", "--spec", "Satin", "--input", "0x00", "--json", "--override.limits"];
    let output = evme(&[&args[..], &[r#"{"txRuntimeLimits":{"txDataSizeLimit":310}}"#]].concat());
    let stopped = output.json();
    assert_eq!(stopped["success"], false);
    assert_eq!(
        stopped["satin"]["limit_exceeded"],
        json!({ "kind": "data_size", "limit": 310, "used": 311 })
    );
}

/// An override is held to what a chain configuration is: an unknown field, a value of the wrong
/// type or one the limits' own check refuses is an error, with nothing on stdout.
#[test]
fn test_an_override_no_chain_may_carry_is_refused() {
    let refused = |limits: &str, code: i32, message: &str| {
        let output =
            evme(&["run", "0x00", "--spec", "Satin", "--json", "--override.limits", limits]);
        assert_eq!(output.code, code, "{limits}: {}", output.stderr);
        assert!(output.stdout.is_empty(), "{limits}: {}", output.stdout);
        assert!(output.stderr.contains(message), "{limits}: {}", output.stderr);
    };
    refused(r#"{"txDataSizeLimit":350}"#, 1, "unknown field `txDataSizeLimit`");
    refused(
        r#"{"txRuntimeLimits":{"txKvUpdateLimit":0}}"#,
        1,
        "tx_runtime_limits.tx_kv_update_limit must not be zero",
    );
    refused(r#"{"blockTxsDataLimit":"1"}"#, 1, "--override.limits: invalid type");
    refused("[1]", 2, "must be a JSON object");
}

/// A command on a legacy spec, the default one included, is refused with the override before it
/// reaches the released CLI, which knows no such flag: `run`, `tx` and a transaction replay.
#[test]
fn test_an_override_is_refused_on_a_legacy_spec() {
    for args in [
        &["run", "0x00", "--override.limits", "{}"][..],
        &["run", "0x00", "--spec", "Rex3", "--override.limits", "{}"],
        &["tx", "--spec", "Rex6", "--override.limits", "{}"],
        // The engine comes from the forced spec, so the refusal needs no RPC.
        &[
            "replay",
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            "--override.spec",
            "Rex6",
            "--override.limits",
            "{}",
        ],
    ] {
        let output = evme(args);
        assert_eq!(output.code, 1, "{args:?}: {}", output.stderr);
        assert!(output.stdout.is_empty(), "{args:?}: {}", output.stdout);
        assert!(
            output.stderr.contains("--override.limits applies to Satin only"),
            "{args:?}: {}",
            output.stderr
        );
    }
}
