//! `--override.limits` on `run` and `tx`: a Satin transaction held to protocol limits other than
//! the spec's, the fields the override names replacing the spec's and every other staying.

use std::process::Command;

use mega_evm::{MegaSpecId, ProtocolLimits, TX_BODY_SIZE, WRITE_RECORD_SIZE};
use serde_json::{json, Value};

/// Two fresh `SSTORE`s, keeping two write records: 310 bytes of body and 40 per record.
const TWO_WRITES: &str = "0x60016000556001600155";

/// A transaction data-size limit with room for the body and one write record, which the second of
/// [`TWO_WRITES`] crosses.
const ONE_RECORD: u64 = TX_BODY_SIZE + WRITE_RECORD_SIZE;

/// What [`TWO_WRITES`] keeps: the body and its two write records.
const TWO_RECORDS: u64 = ONE_RECORD + WRITE_RECORD_SIZE;

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

/// Satin's limits with `txDataSizeLimit` at `limit`, as a report carries them.
fn defaults_with_tx_data_size(limit: u64) -> Value {
    let mut limits = serde_json::to_value(ProtocolLimits::for_spec(MegaSpecId::SATIN)).unwrap();
    limits["txRuntimeLimits"]["txDataSizeLimit"] = json!(limit);
    limits
}

/// Without an override the run keeps both writes; an override of the transaction data size stops
/// it at the second write, and takes both back. A run under an override reports the limits it ran
/// under, and one on the spec's limits reports none.
#[test]
fn test_an_override_holds_a_satin_run_to_other_limits() {
    let kept = run_two_writes(None);
    assert_eq!(kept["success"], true);
    assert_eq!(kept["satin"]["data_size"], 390);
    assert_eq!(kept["satin"]["write_records"], 2);
    assert_eq!(kept["satin"]["limit_exceeded"], Value::Null);
    assert!(kept["satin"].get("limits_override").is_none(), "{}", kept["satin"]);

    let data_size = run_two_writes(Some(r#"{"txRuntimeLimits":{"txDataSizeLimit":350}}"#));
    assert_eq!(data_size["success"], false);
    assert_eq!(
        data_size["satin"]["limit_exceeded"],
        json!({ "kind": "data_size", "limit": 350, "used": 390 })
    );
    assert_eq!(data_size["satin"]["write_records"], 0, "the stop takes the writes back");
    assert_eq!(data_size["satin"]["limits_override"], defaults_with_tx_data_size(350));
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

/// `tx` takes the override too: a body one byte over the transaction data-size limit is rejected
/// at validation, and a limit the body fits runs it and names the override.
#[test]
fn test_an_override_holds_a_satin_tx_to_other_limits() {
    let args = ["tx", "--spec", "Satin", "--input", "0x00", "--json", "--override.limits"];
    let limit = |bytes: u64| format!(r#"{{"txRuntimeLimits":{{"txDataSizeLimit":{bytes}}}}}"#);
    let rejected = evme(&[&args[..], &[limit(310).as_str()]].concat());
    assert_ne!(rejected.code, 0, "{}", rejected.stdout);
    assert!(
        rejected.stderr.contains(mega_evm::TX_BODY_OVER_DATA_SIZE_LIMIT),
        "{}",
        rejected.stderr
    );

    let ran = evme(&[&args[..], &[limit(311).as_str()]].concat()).json();
    assert_eq!(ran["success"], true);
    assert_eq!(ran["satin"]["limit_exceeded"], Value::Null);
    assert_eq!(ran["satin"]["limits_override"], defaults_with_tx_data_size(311));
}

/// The human-readable report names the limits a run was held to under an override, and says
/// nothing of them otherwise.
#[test]
fn test_the_report_names_an_override() {
    let limits = r#"{"txRuntimeLimits":{"txDataSizeLimit":350}}"#;
    let overridden = evme(&["run", TWO_WRITES, "--spec", "Satin", "--override.limits", limits]);
    assert_eq!(overridden.code, 0, "{}", overridden.stderr);
    let line = overridden
        .stdout
        .lines()
        .find(|line| line.starts_with("Limits Override:"))
        .unwrap_or_else(|| panic!("no override line: {}", overridden.stdout));
    let named: Value = serde_json::from_str(line["Limits Override:".len()..].trim()).unwrap();
    assert_eq!(named, defaults_with_tx_data_size(350));

    let plain = evme(&["run", TWO_WRITES, "--spec", "Satin"]);
    assert_eq!(plain.code, 0, "{}", plain.stderr);
    assert!(!plain.stdout.contains("Limits Override"), "{}", plain.stdout);
}

/// An override is held to the limits' shape: an unknown field or a value of the wrong type is an
/// error, with nothing on stdout. Any value of the right type is a counterfactual the tool runs,
/// a limit of zero included.
#[test]
fn test_an_override_of_the_wrong_shape_is_refused() {
    let run = |limits: &str| {
        evme(&["run", "0x00", "--spec", "Satin", "--json", "--override.limits", limits])
    };
    let refused = |limits: &str, code: i32, message: &str| {
        let output = run(limits);
        assert_eq!(output.code, code, "{limits}: {}", output.stderr);
        assert!(output.stdout.is_empty(), "{limits}: {}", output.stdout);
        assert!(output.stderr.contains(message), "{limits}: {}", output.stderr);
    };
    refused(r#"{"txDataSizeLimit":350}"#, 1, "unknown field `txDataSizeLimit`");
    refused(r#"{"blockTxsDataLimit":"1"}"#, 1, "--override.limits: invalid type");
    refused("[1]", 2, "must be a JSON object");

    let zero = run(r#"{"txRuntimeLimits":{"frameDataSizeLimit":0}}"#).json();
    assert_eq!(zero["satin"]["limits_override"]["txRuntimeLimits"]["frameDataSizeLimit"], 0);
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

/// A genesis file for chain `chain_id` activating Satin at `satin_time`, in `dir`.
fn genesis(dir: &tempfile::TempDir, chain_id: u64, satin_time: u64) -> String {
    let satin = mega_evm::SatinChainConfig { activation_time: satin_time };
    let mut config = serde_json::to_value(satin).unwrap();
    config["chainId"] = json!(chain_id);
    let path = dir.path().join(format!("genesis-{chain_id}-{satin_time}.json"));
    std::fs::write(&path, json!({ "config": config }).to_string()).unwrap();
    path.to_str().unwrap().to_string()
}

/// A genesis file configures a chain, not its limits: a Satin run given its chain's file is held to
/// Satin's limits, and keeps both writes. `run` and `tx` run Satin under the file without being
/// told `--spec Satin`.
#[test]
fn test_a_genesis_file_runs_satin_under_the_specs_limits() {
    let dir = tempfile::tempdir().unwrap();
    let genesis = genesis(&dir, 6342, 0);
    let run = |extra: &[&str]| {
        let args = [&["run", TWO_WRITES, "--json", "--genesis", &genesis], extra];
        evme(&args.concat())
    };
    for spec in [&["--spec", "Satin"][..], &[]] {
        let kept = run(spec).json();
        assert_eq!(kept["success"], true, "{spec:?}");
        assert_eq!(kept["satin"]["data_size"], TWO_RECORDS, "{spec:?}");
        assert_eq!(kept["satin"]["limit_exceeded"], Value::Null, "{spec:?}");
        assert!(kept["satin"].get("limits_override").is_none(), "{}", kept["satin"]);
    }
    let tx = evme(&["tx", "--input", "0x00", "--json", "--genesis", &genesis]).json();
    assert_eq!(tx["success"], true);
    assert_eq!(tx["satin"]["data_size"], 311, "a Satin report: {tx}");

    // An override replaces what it names under the file too.
    let limits = format!(r#"{{"txRuntimeLimits":{{"txDataSizeLimit":{ONE_RECORD}}}}}"#);
    let overridden = run(&["--override.limits", &limits]).json();
    assert_eq!(
        overridden["satin"]["limit_exceeded"],
        json!({ "kind": "data_size", "limit": ONE_RECORD, "used": TWO_RECORDS })
    );

    // The file is its chain's: a run on another chain is refused.
    let other = run(&["--chain-id", "6343"]);
    assert_eq!(other.code, 1, "{}", other.stderr);
    assert!(other.stderr.contains("--genesis configures chain 6342"), "{}", other.stderr);
}

/// A run the file cannot configure is refused rather than run on the engine's own schedule: one
/// before the file's `satinTime`, and one on a file without Satin keys. From the file's
/// `satinTime` on, the run is made.
#[test]
fn test_a_genesis_file_refuses_a_run_it_cannot_configure() {
    const SATIN_TIME: &str = "1800000000";
    let dir = tempfile::tempdir().unwrap();
    let later = genesis(&dir, 6342, SATIN_TIME.parse().unwrap());
    let without_satin = dir.path().join("without-satin.json");
    std::fs::write(&without_satin, json!({ "config": { "chainId": 6342 } }).to_string()).unwrap();
    let without_satin = without_satin.to_str().unwrap();

    let refused = |args: &[&str], message: &str| {
        let output = evme(args);
        assert_eq!(output.code, 1, "{args:?}: {}", output.stderr);
        assert!(output.stdout.is_empty(), "{args:?}: {}", output.stdout);
        assert!(output.stderr.contains(message), "{args:?}: {}", output.stderr);
    };
    let before = "--genesis activates Satin at timestamp 1800000000, and the run is at timestamp";
    for command in [&["run", TWO_WRITES][..], &["tx", "--input", "0x00"]] {
        for spec in [&["--spec", "Satin"][..], &[]] {
            let args = [command, spec, &["--json", "--genesis", &later]].concat();
            refused(&args, &format!("{before} 1:"));
            let args = [&args[..], &["--block.timestamp", "1799999999"]].concat();
            refused(&args, &format!("{before} 1799999999:"));
            let args = [command, spec, &["--json", "--genesis", without_satin]].concat();
            refused(&args, "--genesis: the file has no Satin keys");
        }
    }

    let from =
        evme(&["run", TWO_WRITES, "--json", "--genesis", &later, "--block.timestamp", SATIN_TIME])
            .json();
    assert_eq!(from["success"], true);
    assert_eq!(from["satin"]["data_size"], TWO_RECORDS);
}

/// The file applies to Satin only: a command on a legacy spec is refused with it, as with an
/// override, before it reaches the released CLI. The chain is checked first: a legacy run on
/// another chain than the file's is refused for its chain.
#[test]
fn test_a_genesis_file_is_refused_on_a_legacy_spec() {
    let dir = tempfile::tempdir().unwrap();
    let genesis = genesis(&dir, 6342, 0);
    for command in [&["run", "0x00"][..], &["tx", "--input", "0x00"]] {
        let args = [command, &["--spec", "Rex6", "--genesis", &genesis]].concat();
        let output = evme(&args);
        assert_eq!(output.code, 1, "{args:?}: {}", output.stderr);
        assert!(output.stderr.contains("--genesis applies to Satin only"), "{}", output.stderr);

        let args = [&args[..], &["--chain-id", "4326"]].concat();
        let output = evme(&args);
        assert_eq!(output.code, 1, "{args:?}: {}", output.stderr);
        assert!(
            output.stderr.contains("--genesis configures chain 6342, and the run is on chain 4326"),
            "{}",
            output.stderr
        );
    }
}
