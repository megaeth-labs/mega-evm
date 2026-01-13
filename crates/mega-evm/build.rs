//! Build script that validates system contract bytecode.
//!
//! This script compiles and deploys the Solidity contracts using Foundry,
//! then validates that the deployed bytecode matches the frozen artifacts.
//! If they differ, the build fails to prevent accidental contract modifications.

use std::{
    env, fs,
    path::Path,
    process::{Command, Stdio},
};

use serde::Deserialize;

/// Artifact format used by our custom artifacts/Oracle.json
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OracleArtifact {
    deployed_bytecode: String,
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let system_contracts_dir = Path::new(&manifest_dir).join("../system-contracts");

    // Set up rerun-if-changed triggers
    println!(
        "cargo::rerun-if-changed={}",
        system_contracts_dir.join("src/Oracle.sol").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        system_contracts_dir.join("artifacts/Oracle.json").display()
    );
    println!(
        "cargo::rerun-if-changed={}",
        system_contracts_dir.join("foundry.toml").display()
    );

    // Check if forge is available
    let forge_check = Command::new("forge")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match forge_check {
        Ok(status) if status.success() => {}
        _ => {
            panic!(
                "\n\
                 ╔══════════════════════════════════════════════════════════════╗\n\
                 ║  ERROR: `forge` command not found                            ║\n\
                 ║                                                              ║\n\
                 ║  Foundry is required to build mega-evm.                      ║\n\
                 ║  Install it from: https://getfoundry.sh                      ║\n\
                 ║                                                              ║\n\
                 ║  Quick install:                                              ║\n\
                 ║    curl -L https://foundry.paradigm.xyz | bash               ║\n\
                 ║    foundryup                                                 ║\n\
                 ╚══════════════════════════════════════════════════════════════╝\n"
            );
        }
    }

    // Run the deploy script to generate bytecode with constructor args embedded.
    // This writes to a temp file which we'll compare against the committed artifact.
    let temp_artifact = system_contracts_dir.join("artifacts/Oracle.json.tmp");

    let script_status = Command::new("forge")
        .args([
            "script",
            "script/OracleBytecode.s.sol:SaveOracleBytecode",
            "--sig",
            "run()",
        ])
        .current_dir(&system_contracts_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("Failed to execute forge script");

    if !script_status.success() {
        panic!("forge script failed");
    }

    // The script writes directly to artifacts/Oracle.json, so we need to:
    // 1. Read what the script just wrote
    // 2. Restore the original from git
    // 3. Compare

    // Read the newly generated artifact
    let generated_artifact_path = system_contracts_dir.join("artifacts/Oracle.json");
    let generated_content =
        fs::read_to_string(&generated_artifact_path).expect("Failed to read generated artifact");
    let generated_artifact: OracleArtifact =
        serde_json::from_str(&generated_content).expect("Failed to parse generated artifact");
    let generated_bytecode = &generated_artifact.deployed_bytecode;

    // Restore original artifact from git
    let _restore_status = Command::new("git")
        .args(["checkout", "artifacts/Oracle.json"])
        .current_dir(&system_contracts_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // Read the expected (committed) artifact
    let expected_content =
        fs::read_to_string(&generated_artifact_path).expect("Failed to read expected artifact");
    let expected_artifact: OracleArtifact =
        serde_json::from_str(&expected_content).expect("Failed to parse expected artifact");
    let expected_bytecode = &expected_artifact.deployed_bytecode;

    // Clean up temp file if it exists
    let _ = fs::remove_file(&temp_artifact);

    // Compare bytecodes
    if generated_bytecode != expected_bytecode {
        panic!(
            "\n\
             ╔══════════════════════════════════════════════════════════════╗\n\
             ║  ERROR: Oracle contract bytecode mismatch!                   ║\n\
             ║                                                              ║\n\
             ║  The compiled Oracle.sol bytecode does not match the        ║\n\
             ║  frozen artifact in artifacts/Oracle.json.                  ║\n\
             ║                                                              ║\n\
             ║  If this change is intentional (new spec version):          ║\n\
             ║    1. cd crates/system-contracts                            ║\n\
             ║    2. forge script script/OracleBytecode.s.sol              ║\n\
             ║    3. Update oracle.rs constants to match                   ║\n\
             ║    4. Commit all changes together                           ║\n\
             ║                                                              ║\n\
             ║  If this change is accidental:                              ║\n\
             ║    Revert your changes to Oracle.sol                        ║\n\
             ╚══════════════════════════════════════════════════════════════╝\n\
             \n\
             Expected (first 100 chars): {}...\n\
             Generated (first 100 chars): {}...\n",
            &expected_bytecode[..expected_bytecode.len().min(100)],
            &generated_bytecode[..generated_bytecode.len().min(100)]
        );
    }
}
