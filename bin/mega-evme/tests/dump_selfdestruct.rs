//! Integration tests for how `--dump` reports an account that `SELFDESTRUCT`
//! erased during the transaction, and for feeding such a dump back through
//! `--prestate`.
//!
//! Both tests execute one local `run` transaction with no network access: a
//! CREATE whose init code deploys a child, destroys it, and then keeps calling
//! it (its code and storage still answer for the rest of the transaction, and
//! are gone at commit time), while the deploying contract itself survives with
//! code and storage of its own.

use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

/// CREATE init code that, in a single transaction:
///
/// 1. deploys a child contract whose runtime `SELFDESTRUCT`s on an empty call and writes storage
///    slot `0x1` on a non-empty one,
/// 2. calls the child with no calldata (destroying it — it was created in this same transaction, so
///    EIP-6780 lets the destruction take effect),
/// 3. calls it again with calldata, which still runs the code the commit is about to erase and
///    writes `0x42` into its storage,
/// 4. writes `0x99` into its own slot `0x0` and returns an 11-byte runtime that answers `SLOAD(0)`,
///    so the deployer survives with both code and storage.
const DRIVER: &str = concat!(
    "0x766d3660075733ff005b604260015500600052600e6012f3600052601760096000f060006000600060006000",
    "855af15060006000600160006000855af150506a60005460005260206000f36000526099600055600b6015f3",
);

/// Address of the deployed driver (`CREATE` from the default sender at nonce 0).
const DRIVER_ADDRESS: &str = "0x5fbdb2315678afecb367f032d93f642f64180aa3";

/// Address of the child the driver creates and destroys.
const DESTROYED_ADDRESS: &str = "0xa16e02e87b7454126e5e10d957a927a7f5b5d2be";

/// Reads the driver's `SLOAD(0)` through a `CALL`, then `EXTCODEHASH`es the
/// destroyed address, and returns both words.
///
/// An address absent from the state hashes to zero (EIP-1052), so the second
/// word separates "not loaded" from a resurrected account, whose runtime would
/// hash to something else.
const PROBE: &str = concat!(
    "0x60206000600060006000735fbdb2315678afecb367f032d93f642f64180aa35af15073a16e02e87b7454126e",
    "5e10d957a927a7f5b5d2be3f60205260406000f3",
);

/// The driver's storage word followed by the destroyed address's `EXTCODEHASH`.
const EXPECTED_PROBE_OUTPUT: &str = concat!(
    "0x0000000000000000000000000000000000000000000000000000000000000099000000000000000000000000",
    "0000000000000000000000000000000000000000",
);

fn mega_evme() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mega-evme"))
}

/// Runs `run` with `--json` and returns the parsed summary, asserting success.
fn run_json(args: &[&str]) -> Value {
    let output = mega_evme().arg("run").args(args).arg("--json").output().expect("run mega-evme");
    assert!(
        output.status.success(),
        "mega-evme run {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).expect("summary is JSON")
}

/// The account the transaction destroyed is reported as destroyed — not as a
/// live account still holding the code and storage the commit erases — while
/// every surviving account keeps its full body.
#[test]
fn test_dump_reports_selfdestructed_account_as_marker() {
    let dir = tempdir().expect("tempdir");
    let dump_path = dir.path().join("post-state.json");

    run_json(&[
        DRIVER,
        "--create",
        "true",
        "--spec",
        "Rex6",
        "--gas",
        "30000000",
        "--sender.balance",
        "10ether",
        "--dump",
        "--dump.output",
        dump_path.to_str().expect("utf-8 path"),
    ]);

    let dumped: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(&dump_path).expect("read dump"))
            .expect("dump is JSON");

    assert_eq!(
        dumped.get(DESTROYED_ADDRESS),
        Some(&serde_json::json!({ "selfdestructed": true })),
        "destroyed account must be reported by the marker alone",
    );

    let driver = dumped.get(DRIVER_ADDRESS).expect("driver account is dumped");
    assert_eq!(driver["code"], "0x60005460005260206000f3", "surviving account keeps its code");
    assert_eq!(driver["storage"]["0x0"], "0x99", "surviving account keeps its storage");
    assert_eq!(driver["nonce"], "0x2", "surviving account keeps its nonce");

    for (address, account) in &dumped {
        if address == DESTROYED_ADDRESS {
            continue;
        }
        assert!(
            account.get("selfdestructed").is_none(),
            "{address} survived and must carry no marker",
        );
        for field in ["balance", "nonce", "code", "codeHash", "storage"] {
            assert!(account.get(field).is_some(), "{address} must keep its {field}");
        }
    }
}

/// A dump fed back through `--prestate` reconstructs the world the commit
/// produced: the destroyed address does not exist, and the accounts that
/// survived come back with their code and storage.
#[test]
fn test_dumped_state_round_trips_through_prestate() {
    let dir = tempdir().expect("tempdir");
    let dump_path = dir.path().join("post-state.json");

    run_json(&[
        DRIVER,
        "--create",
        "true",
        "--spec",
        "Rex6",
        "--gas",
        "30000000",
        "--sender.balance",
        "10ether",
        "--dump",
        "--dump.output",
        dump_path.to_str().expect("utf-8 path"),
    ]);

    // The dump carries the sender's post-execution nonce, so the reloaded run
    // has to continue from it.
    let summary = run_json(&[
        PROBE,
        "--spec",
        "Rex6",
        "--gas",
        "30000000",
        "--nonce",
        "1",
        "--prestate",
        dump_path.to_str().expect("utf-8 path"),
    ]);

    assert_eq!(
        summary["output"], EXPECTED_PROBE_OUTPUT,
        "reloaded world must keep the survivor's code and storage and lack the destroyed account",
    );
}
