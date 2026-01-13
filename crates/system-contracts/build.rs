//! Build script that validates and exports system contract bytecode.
//!
//! This script:
//! 1. Compiles the Solidity contracts using Foundry
//! 2. Validates that the compiled bytecode matches Oracle-latest.json
//! 3. Generates Rust constants from all versioned artifact files

use std::{
    env, fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use alloy_primitives::{hex, keccak256};
use serde::Deserialize;

/// Artifact format for Oracle JSON files
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OracleArtifact {
    #[serde(default)]
    version: String,
    #[serde(rename = "codeHash")]
    code_hash: String,
    deployed_bytecode: String,
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
    let crate_dir = Path::new(&manifest_dir);

    // Set up rerun-if-changed triggers
    println!("cargo::rerun-if-changed={}", crate_dir.join("contracts/Oracle.sol").display());
    println!(
        "cargo::rerun-if-changed={}",
        crate_dir.join("artifacts/Oracle-latest.json").display()
    );
    println!("cargo::rerun-if-changed={}", crate_dir.join("foundry.toml").display());

    // Check if forge is available
    let forge_check =
        Command::new("forge").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status();

    match forge_check {
        Ok(status) if status.success() => {}
        _ => {
            panic!(
                "\n\
                 ╔══════════════════════════════════════════════════════════════╗\n\
                 ║  ERROR: `forge` command not found                            ║\n\
                 ║                                                              ║\n\
                 ║  Foundry is required to build system-contracts.              ║\n\
                 ║  Install it from: https://getfoundry.sh                      ║\n\
                 ║                                                              ║\n\
                 ║  Quick install:                                              ║\n\
                 ║    curl -L https://foundry.paradigm.xyz | bash               ║\n\
                 ║    foundryup                                                 ║\n\
                 ╚══════════════════════════════════════════════════════════════╝\n"
            );
        }
    }

    // Run the deploy script to generate bytecode with constructor args embedded
    let script_status = Command::new("forge")
        .args(["script", "scripts/OracleBytecode.s.sol:SaveOracleBytecode", "--sig", "run()"])
        .current_dir(crate_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("Failed to execute forge script");

    assert!(script_status.success(), "forge script failed");

    // Read the generated artifact (script writes to artifacts/Oracle.json)
    let generated_path = crate_dir.join("artifacts/Oracle.json");
    let generated_content =
        fs::read_to_string(&generated_path).expect("Failed to read generated artifact");
    let generated: OracleArtifact =
        serde_json::from_str(&generated_content).expect("Failed to parse generated artifact");

    // Read the expected artifact (Oracle-latest.json)
    let expected_path = crate_dir.join("artifacts/Oracle-latest.json");
    let expected_content =
        fs::read_to_string(&expected_path).expect("Failed to read Oracle-latest.json");
    let expected: OracleArtifact =
        serde_json::from_str(&expected_content).expect("Failed to parse Oracle-latest.json");

    // Compare bytecode directly (bytecode_hash = "none" ensures deterministic output)
    assert!(
        generated.deployed_bytecode == expected.deployed_bytecode,
        "\n\
         ╔══════════════════════════════════════════════════════════════╗\n\
         ║  ERROR: Oracle contract bytecode mismatch!                   ║\n\
         ║                                                              ║\n\
         ║  The compiled Oracle.sol bytecode does not match             ║\n\
         ║  artifacts/Oracle-latest.json.                               ║\n\
         ║                                                              ║\n\
         ║  If this change is intentional (new spec version):           ║\n\
         ║    1. Create a new artifacts/Oracle-X.Y.Z.json file          ║\n\
         ║    2. Update Oracle-latest.json symlink                      ║\n\
         ║    3. Commit all changes together                            ║\n\
         ║                                                              ║\n\
         ║  If this change is accidental:                               ║\n\
         ║    Revert your changes to contracts/Oracle.sol               ║\n\
         ╚══════════════════════════════════════════════════════════════╝\n\
         \n\
         Expected: {}...\n\
         Generated: {}...\n",
        &expected.deployed_bytecode[..expected.deployed_bytecode.len().min(80)],
        &generated.deployed_bytecode[..generated.deployed_bytecode.len().min(80)]
    );

    // Clean up generated artifact
    let _ = fs::remove_file(&generated_path);

    // Read all versioned artifacts and generate Rust constants
    let artifacts_dir = crate_dir.join("artifacts");
    let mut oracle_versions = Vec::new();

    for entry in fs::read_dir(&artifacts_dir).expect("Failed to read artifacts directory") {
        let entry = entry.expect("Failed to read directory entry");
        let path = entry.path();
        let filename = path.file_name().unwrap().to_str().unwrap();

        // Skip symlinks and non-versioned files
        if path.is_symlink() || !filename.starts_with("Oracle-") || !filename.ends_with(".json") {
            continue;
        }

        let content = fs::read_to_string(&path).expect("Failed to read artifact");
        let artifact: OracleArtifact =
            serde_json::from_str(&content).expect("Failed to parse artifact");

        // Sanity check, the code hash must match the expected code hash.
        let bytecode = hex::decode(&artifact.deployed_bytecode).expect("Invalid bytecode hex");
        let computed_hash = keccak256(&bytecode);
        let expected_hash = hex::decode(&artifact.code_hash).expect("Invalid code hash hex");
        assert!(
            computed_hash.as_slice() == expected_hash.as_slice(),
            "Code hash mismatch for artifact {}: expected {}, got {:x}",
            filename,
            artifact.code_hash,
            computed_hash
        );

        oracle_versions.push(artifact);
    }

    // Sort by semantic version (major.minor.patch)
    oracle_versions.sort_by(|a, b| {
        let parse_version = |v: &str| -> (u32, u32, u32) {
            let parts: Vec<u32> = v.split('.').filter_map(|s| s.parse().ok()).collect();
            (
                parts.first().copied().unwrap_or(0),
                parts.get(1).copied().unwrap_or(0),
                parts.get(2).copied().unwrap_or(0),
            )
        };
        parse_version(&a.version).cmp(&parse_version(&b.version))
    });

    // Generate Rust code
    let generated_path = Path::new(&out_dir).join("oracle_artifacts.rs");
    let mut file = fs::File::create(&generated_path).expect("Failed to create generated file");

    writeln!(file, "// Auto-generated Oracle contract bytecode constants.").unwrap();
    writeln!(file, "// DO NOT EDIT - generated by build.rs from artifacts/").unwrap();
    writeln!(file).unwrap();
    writeln!(file, "use alloy_primitives::{{bytes, b256, Bytes, B256}};").unwrap();
    writeln!(file).unwrap();

    for artifact in &oracle_versions {
        let version_underscore = artifact.version.replace('.', "_");
        let const_name = format!("V{}", version_underscore);

        writeln!(file, "/// Oracle contract bytecode v{}", artifact.version).unwrap();
        writeln!(
            file,
            "pub const {}_CODE: Bytes = bytes!(\"{}\");",
            const_name, artifact.deployed_bytecode
        )
        .unwrap();
        writeln!(file, "/// Oracle contract code hash v{}", artifact.version).unwrap();
        writeln!(
            file,
            "pub const {}_CODE_HASH: B256 = b256!(\"{}\");",
            const_name, artifact.code_hash
        )
        .unwrap();
        writeln!(file).unwrap();
    }

    // Add latest alias (based on Oracle-latest.json symlink, not max version)
    let latest_version_underscore = expected.version.replace('.', "_");
    writeln!(file, "/// Latest Oracle contract bytecode").unwrap();
    writeln!(file, "pub const LATEST_CODE: Bytes = V{}_CODE;", latest_version_underscore).unwrap();
    writeln!(file, "/// Latest Oracle contract code hash").unwrap();
    writeln!(file, "pub const LATEST_CODE_HASH: B256 = V{}_CODE_HASH;", latest_version_underscore)
        .unwrap();
}
