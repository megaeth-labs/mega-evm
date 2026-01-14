//! Build script that validates and exports system contract bytecode.
//!
//! This script:
//! 1. Compiles the Solidity contracts using Foundry
//! 2. Validates that the compiled bytecode matches *-latest.json
//! 3. Generates Rust constants from all versioned artifact files

use std::{
    env, fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use alloy_primitives::{hex, keccak256};
use serde::Deserialize;

/// Artifact format for system contract JSON files
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContractArtifact {
    #[serde(default)]
    version: String,
    code_hash: String,
    deployed_bytecode: String,
}

/// Configuration for a system contract to be validated and processed
struct ContractConfig<'a> {
    /// Contract name (e.g., "Oracle")
    name: &'a str,
    /// Forge script path (e.g., "scripts/OracleBytecode.s.sol:SaveOracleBytecode")
    script_path: &'a str,
    /// Output Rust file name (e.g., `oracle_artifacts.rs`)
    output_file: &'a str,
}

/// Runs a forge script, validates bytecode against expected artifact, and returns the expected
/// artifact.
fn validate_contract_bytecode(crate_dir: &Path, config: &ContractConfig<'_>) -> ContractArtifact {
    // Run the deploy script to generate bytecode with constructor args embedded
    let script_status = Command::new("forge")
        .args(["script", config.script_path, "--sig", "run()"])
        .current_dir(crate_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .unwrap_or_else(|_| panic!("Failed to execute {} forge script", config.name));

    assert!(script_status.success(), "{} forge script failed", config.name);

    // Read the generated artifact
    let generated_path = crate_dir.join(format!("artifacts/{}.json", config.name));
    let generated_content = fs::read_to_string(&generated_path)
        .unwrap_or_else(|_| panic!("Failed to read {} generated artifact", config.name));
    let generated: ContractArtifact = serde_json::from_str(&generated_content)
        .unwrap_or_else(|_| panic!("Failed to parse {} generated artifact", config.name));

    // Read the expected artifact
    let expected_path = crate_dir.join(format!("artifacts/{}-latest.json", config.name));
    let expected_content = fs::read_to_string(&expected_path)
        .unwrap_or_else(|_| panic!("Failed to read {}-latest.json", config.name));
    let expected: ContractArtifact = serde_json::from_str(&expected_content)
        .unwrap_or_else(|_| panic!("Failed to parse {}-latest.json", config.name));

    // Compare code hash
    assert!(
        generated.code_hash == expected.code_hash,
        r#"
ERROR: {name} contract bytecode mismatch!

The compiled {name}.sol bytecode does not match artifacts/{name}-latest.json.

If this change is intentional (new spec version):
  1. Create a new artifacts/{name}-X.Y.Z.json file
  2. Update {name}-latest.json symlink
  3. Commit all changes together

If this change is accidental:
  Revert your changes to contracts/{name}.sol

Expected:  {expected}
Generated: {generated}
"#,
        expected = expected.code_hash,
        generated = generated.code_hash,
        name = config.name,
    );

    // Clean up generated artifact
    let _ = fs::remove_file(&generated_path);

    expected
}

/// Collects all versioned artifacts for a contract, validates code hashes, and returns sorted list.
fn collect_versioned_artifacts(artifacts_dir: &Path, prefix: &str) -> Vec<ContractArtifact> {
    let mut versions = Vec::new();

    for entry in fs::read_dir(artifacts_dir).expect("Failed to read artifacts directory") {
        let entry = entry.expect("Failed to read directory entry");
        let path = entry.path();
        let filename = path.file_name().unwrap().to_str().unwrap();

        // Skip symlinks and non-versioned files
        if path.is_symlink() || !filename.starts_with(prefix) || !filename.ends_with(".json") {
            continue;
        }

        let content = fs::read_to_string(&path).expect("Failed to read artifact");
        let artifact: ContractArtifact =
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

        versions.push(artifact);
    }

    versions
}

/// Generates Rust source file with bytecode constants for a contract.
fn generate_rust_constants(
    out_dir: &Path,
    config: &ContractConfig<'_>,
    versions: &[ContractArtifact],
    latest: &ContractArtifact,
) {
    let generated_path = out_dir.join(config.output_file);
    let mut file = fs::File::create(&generated_path).expect("Failed to create generated file");

    writeln!(file, "// Auto-generated {} contract bytecode constants.", config.name).unwrap();
    writeln!(file, "// DO NOT EDIT - generated by build.rs from artifacts/").unwrap();
    writeln!(file).unwrap();
    writeln!(file, "use alloy_primitives::{{bytes, b256, Bytes, B256}};").unwrap();
    writeln!(file).unwrap();

    for artifact in versions {
        let version_underscore = artifact.version.replace('.', "_");
        let const_name = format!("V{}", version_underscore);

        writeln!(file, "/// {} contract bytecode v{}", config.name, artifact.version).unwrap();
        writeln!(
            file,
            "pub const {}_CODE: Bytes = bytes!(\"{}\");",
            const_name, artifact.deployed_bytecode
        )
        .unwrap();
        writeln!(file, "/// {} contract code hash v{}", config.name, artifact.version).unwrap();
        writeln!(
            file,
            "pub const {}_CODE_HASH: B256 = b256!(\"{}\");",
            const_name, artifact.code_hash
        )
        .unwrap();
        writeln!(file).unwrap();
    }

    // Add latest alias
    let latest_version_underscore = latest.version.replace('.', "_");
    writeln!(file, "/// Latest {} contract bytecode", config.name).unwrap();
    writeln!(file, "pub const LATEST_CODE: Bytes = V{}_CODE;", latest_version_underscore).unwrap();
    writeln!(file, "/// Latest {} contract code hash", config.name).unwrap();
    writeln!(file, "pub const LATEST_CODE_HASH: B256 = V{}_CODE_HASH;", latest_version_underscore)
        .unwrap();
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
    let crate_dir = Path::new(&manifest_dir);
    let out_path = Path::new(&out_dir);

    // Define contract configurations
    let contracts = [ContractConfig {
        name: "Oracle",
        script_path: "scripts/OracleBytecode.s.sol:SaveOracleBytecode",
        output_file: "oracle_artifacts.rs",
    }];

    // Set up rerun-if-changed triggers
    for config in &contracts {
        println!(
            "cargo::rerun-if-changed={}",
            crate_dir.join(format!("contracts/{}.sol", config.name)).display()
        );
        println!(
            "cargo::rerun-if-changed={}",
            crate_dir.join(format!("artifacts/{}-latest.json", config.name)).display()
        );
    }
    println!("cargo::rerun-if-changed={}", crate_dir.join("foundry.toml").display());

    // Check if forge is available
    let forge_check =
        Command::new("forge").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status();

    match forge_check {
        Ok(status) if status.success() => {}
        _ => {
            panic!(
                r#"
ERROR: `forge` command not found

Foundry is required to build system-contracts.
Install it from: https://getfoundry.sh

Quick install:
  curl -L https://foundry.paradigm.xyz | bash
  foundryup
"#
            );
        }
    }

    let artifacts_dir = crate_dir.join("artifacts");

    // Process each contract
    for config in &contracts {
        // Validate bytecode and get expected artifact (contains latest version info)
        let latest = validate_contract_bytecode(crate_dir, config);

        // Collect all versioned artifacts
        let prefix = format!("{}-", config.name);
        let versions = collect_versioned_artifacts(&artifacts_dir, &prefix);

        // Generate Rust constants
        generate_rust_constants(out_path, config, &versions, &latest);
    }
}
