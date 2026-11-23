//! Pins the code hashes and latest aliases for the checked-in system contracts.

use std::{fs, path::Path};

use mega_system_contracts::{
    access_control, alloy_primitives::b256, keyless_deploy, limit_control, oracle,
    sequencer_registry,
};

#[test]
fn test_versioned_artifact_hashes_are_frozen() {
    let cases = [
        (
            (
                "KeylessDeploy",
                "1.0.0",
                b256!("55020d41649acf7a84add6e628b887f802218d9ac86f142ef0994da43ea5eeb6"),
            ),
            keyless_deploy::V1_0_0_CODE_HASH,
        ),
        (
            (
                "MegaAccessControl",
                "1.0.0",
                b256!("96d0f3ba6b474e9684a97e4388ec8dbb7929818b51e6b4f19f885ccbdf642f9b"),
            ),
            access_control::V1_0_0_CODE_HASH,
        ),
        (
            (
                "MegaLimitControl",
                "1.0.0",
                b256!("3927f2a4803c5e18153ff5742d0fa1acd9ad04538e4e6037cb4a9b28694ca87f"),
            ),
            limit_control::V1_0_0_CODE_HASH,
        ),
        (
            (
                "Oracle",
                "1.0.0",
                b256!("e9b044afb735a0f569faeb248088b4f267578f60722f87d06ec3867b250a2c34"),
            ),
            oracle::V1_0_0_CODE_HASH,
        ),
        (
            (
                "Oracle",
                "1.1.0",
                b256!("06df675a69e53ea2a3c948521e330b3801740fede324a1cef2044418f8e09242"),
            ),
            oracle::V1_1_0_CODE_HASH,
        ),
        (
            (
                "Oracle",
                "2.0.0",
                b256!("71a65239db8d0f1bb765fad36e34f57600420d103a4401ef7555bd50b229dc55"),
            ),
            oracle::V2_0_0_CODE_HASH,
        ),
        (
            (
                "SequencerRegistry",
                "1.0.0",
                b256!("63cd411a379be1c198613ef1d15c3058e7b0db4a5d07d4bcf07014af90040315"),
            ),
            sequencer_registry::V1_0_0_CODE_HASH,
        ),
        (
            (
                "SequencerRegistry",
                "2.0.0",
                b256!("abd7e8f1c8f0f9ca0346df585b65d9791a7e9ba5c431cc4eda5d2c2f1f5e9c43"),
            ),
            sequencer_registry::V2_0_0_CODE_HASH,
        ),
    ];
    for ((contract, version, expected), actual) in cases {
        assert_eq!(actual, expected, "{contract} v{version} code hash changed");
    }
}

#[test]
fn test_latest_aliases_point_to_expected_versions() {
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("artifacts");
    let cases = [
        (
            "KeylessDeploy",
            "1.0.0",
            keyless_deploy::V1_0_0_CODE_HASH,
            keyless_deploy::LATEST_CODE_HASH,
        ),
        (
            "MegaAccessControl",
            "1.0.0",
            access_control::V1_0_0_CODE_HASH,
            access_control::LATEST_CODE_HASH,
        ),
        (
            "MegaLimitControl",
            "1.0.0",
            limit_control::V1_0_0_CODE_HASH,
            limit_control::LATEST_CODE_HASH,
        ),
        ("Oracle", "2.0.0", oracle::V2_0_0_CODE_HASH, oracle::LATEST_CODE_HASH),
        (
            "SequencerRegistry",
            "2.0.0",
            sequencer_registry::V2_0_0_CODE_HASH,
            sequencer_registry::LATEST_CODE_HASH,
        ),
    ];
    for (contract, version, expected_hash, latest_hash) in cases {
        assert_eq!(latest_hash, expected_hash, "{contract} should alias v{version}");
        if artifacts.is_dir() {
            let path = artifacts.join(format!("{contract}-latest.json"));
            let data = fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            let alias: serde_json::Value = serde_json::from_slice(&data)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            assert_eq!(alias["version"].as_str(), Some(version), "{}", path.display());
        }
    }
}
