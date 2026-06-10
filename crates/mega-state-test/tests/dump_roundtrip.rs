//! End-to-end self-consistency test for the replay-fixture pipeline.
//!
//! This exercises the same code path `mega-evme replay --dump-fixture` uses to
//! compute a fixture's `post` expectation ([`execute_unit_collect`]) and then
//! verifies that the resulting fixture validates when re-run through the state
//! test runner ([`execute_test_suite`]) — without needing an RPC replay.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use state_test::{
    runner::{execute_test_suite, execute_unit_collect, fill_test_suite},
    types::{SpecName, Test, TestSuite, TestUnit},
};

/// A minimal `MegaETH` unit: a funded sender transfers value to a pre-existing
/// recipient, under a `megaEnv` carrying a non-default SALT bucket capacity.
fn sample_unit_json() -> &'static str {
    r#"{
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
            },
            "0x2000000000000000000000000000000000000002": {
                "balance": "0x0",
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
            "to": "0x2000000000000000000000000000000000000002",
            "value": ["0x3e8"]
        },
        "post": {},
        "megaEnv": {
            "bucketCapacities": [[100, 4096]],
            "oracleStorage": []
        }
    }"#
}

/// Build the dumped fixture suite (unit with computed `post`) the way
/// `mega-evme --dump-fixture` does, and write it as JSON.
fn dump_fixture_json() -> (String, state_test::runner::ExecutedUnit) {
    let mut unit: TestUnit = serde_json::from_str(sample_unit_json()).expect("parse unit");
    let spec = SpecName::Rex5;

    let executed = execute_unit_collect(&unit, &spec).expect("execute unit");

    unit.out = executed.output.clone();
    unit.post = std::collections::BTreeMap::from([(
        spec,
        vec![Test::for_dump(
            executed.state_root,
            executed.logs_root,
            executed.gas_used,
            executed.status.clone(),
        )],
    )]);

    let suite = TestSuite(std::collections::BTreeMap::from([("dump_test".to_string(), unit)]));
    (serde_json::to_string_pretty(&suite).expect("serialize"), executed)
}

#[test]
fn test_dumped_fixture_self_validates() {
    let (json, executed) = dump_fixture_json();

    // Sanity: the value transfer succeeds and consumes a positive amount of gas.
    assert!(executed.gas_used > 0);
    assert_eq!(executed.status, "success");

    let dir = std::env::temp_dir().join("mega_evme_dump_roundtrip");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("self_validate.json");
    std::fs::write(&path, &json).expect("write fixture");

    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    execute_test_suite(&path, &elapsed, false, false)
        .expect("dumped fixture should self-validate when re-run");
}

#[test]
fn test_fill_computes_post_for_empty_fixture() {
    // The sample unit ships with an empty `post`; fill must compute one that
    // then self-validates, leaving no temp file behind.
    let suite_json = format!("{{\"fill_test\": {}}}", sample_unit_json());
    let dir = std::env::temp_dir().join("mega_evme_dump_roundtrip");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("fill_empty.json");
    std::fs::write(&path, &suite_json).expect("write fixture");

    let n = fill_test_suite(&path, Some(SpecName::Rex5), false).expect("fill");
    assert_eq!(n, 1);
    assert!(!path.with_extension("json.tmp").exists(), "temp file must be renamed away");

    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    execute_test_suite(&path, &elapsed, false, false).expect("filled fixture self-validates");
}

#[test]
fn test_fill_refuses_existing_post_without_force() {
    let (json, _) = dump_fixture_json();
    let dir = std::env::temp_dir().join("mega_evme_dump_roundtrip");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("fill_force.json");
    std::fs::write(&path, &json).expect("write fixture");

    let err = fill_test_suite(&path, Some(SpecName::Rex5), false)
        .expect_err("non-empty post must be refused without force");
    assert!(format!("{err}").contains("--force"), "error should point at --force: {err}");
    // The refused file is untouched.
    assert_eq!(std::fs::read_to_string(&path).expect("read back"), json);

    let n = fill_test_suite(&path, Some(SpecName::Rex5), true).expect("fill with force");
    assert_eq!(n, 1);
    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    execute_test_suite(&path, &elapsed, false, false).expect("refilled fixture self-validates");
}

#[test]
fn test_undersized_bucket_capacity_fails_instead_of_panicking() {
    // A hand-edited capacity below MIN_BUCKET_SIZE must surface as a fixture
    // error, not abort the process via the DynamicGasCost assert.
    let bad = sample_unit_json().replace("[[100, 4096]]", "[[100, 1]]");
    assert_ne!(bad, sample_unit_json(), "capacity replacement applied");
    let unit: TestUnit = serde_json::from_str(&bad).expect("parse unit");

    let err = execute_unit_collect(&unit, &SpecName::Rex5)
        .expect_err("undersized capacity must fail execution");
    assert!(format!("{err}").contains("MIN_BUCKET_SIZE"), "unexpected error: {err}");
}

#[test]
fn test_tampered_gas_fails_validation() {
    let (json, executed) = dump_fixture_json();
    // Corrupt the recorded gas so the explicit gas check must fail.
    let from = format!("\"megaGasUsed\": {}", executed.gas_used);
    let to = format!("\"megaGasUsed\": {}", executed.gas_used + 42);
    let tampered = json.replace(&from, &to);
    assert_ne!(tampered, json, "tamper replacement applied");

    let dir = std::env::temp_dir().join("mega_evme_dump_roundtrip");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("tampered.json");
    std::fs::write(&path, &tampered).expect("write fixture");

    let elapsed = Arc::new(Mutex::new(Duration::ZERO));
    let result = execute_test_suite(&path, &elapsed, false, false);
    assert!(result.is_err(), "tampered gas must fail validation");
}
