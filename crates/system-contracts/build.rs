//! Build script that validates system contract bytecode.
//!
//! When Foundry (`forge`) is available (i.e., building from the repository), this script:
//! 1. Compiles the Solidity contracts using Foundry
//! 2. Validates that the compiled bytecode matches `*-latest.json`
//!
//! When Foundry is not available (i.e., building from a published crate), validation is skipped.
//! The pre-generated Rust constants in `src/generated/` are always used directly by `lib.rs`.

use std::{
    env, fs,
    path::Path,
    process::{Command, Stdio},
};

use alloy_primitives::{keccak256, Bytes, B256};
use semver::Version;
use serde::Deserialize;

/// Artifact format for system contract JSON files
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContractArtifact {
    #[allow(dead_code)]
    version: Version,
    code_hash: B256,
    deployed_bytecode: Bytes,
}

/// Configuration for a system contract to be validated
struct ContractConfig<'a> {
    /// Contract name (e.g., "Oracle")
    name: &'a str,
    /// Forge script path (e.g., "scripts/OracleBytecode.s.sol:SaveOracleBytecode")
    script_path: &'a str,
}

/// Runs a forge script and validates bytecode against expected artifact.
fn validate_contract_bytecode(crate_dir: &Path, config: &ContractConfig<'_>) {
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

Expected:  {expected:x}
Generated: {generated:x}
"#,
        expected = expected.code_hash,
        generated = generated.code_hash,
        name = config.name,
    );

    // Clean up generated artifact
    let _ = fs::remove_file(&generated_path);
}

/// Validates that all versioned artifacts have correct code hashes.
fn validate_versioned_artifacts(artifacts_dir: &Path, prefix: &str) {
    for entry in fs::read_dir(artifacts_dir).expect("Failed to read artifacts directory") {
        let entry = entry.expect("Failed to read directory entry");
        let path = entry.path();
        let filename = path.file_name().unwrap().to_str().unwrap();

        // Skip non-versioned files and *-latest.json aliases.
        if !filename.starts_with(prefix) ||
            !filename.ends_with(".json") ||
            filename.contains("-latest")
        {
            continue;
        }

        let content = fs::read_to_string(&path).expect("Failed to read artifact");
        let artifact: ContractArtifact =
            serde_json::from_str(&content).expect("Failed to parse artifact");

        let computed_hash = keccak256(&artifact.deployed_bytecode);
        assert!(
            computed_hash == artifact.code_hash,
            "Code hash mismatch for artifact {}: expected {:x}, got {:x}",
            filename,
            artifact.code_hash,
            computed_hash
        );
    }
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let crate_dir = Path::new(&manifest_dir);

    // Define contract configurations
    let contracts = [
        ContractConfig {
            name: "Oracle",
            script_path: "scripts/OracleBytecode.s.sol:SaveOracleBytecode",
        },
        ContractConfig {
            name: "KeylessDeploy",
            script_path: "scripts/KeylessDeployBytecode.s.sol:SaveKeylessDeployBytecode",
        },
        ContractConfig {
            name: "MegaAccessControl",
            script_path: "scripts/MegaAccessControlBytecode.s.sol:SaveMegaAccessControlBytecode",
        },
        ContractConfig {
            name: "MegaLimitControl",
            script_path: "scripts/MegaLimitControlBytecode.s.sol:SaveMegaLimitControlBytecode",
        },
    ];

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

    // Skip validation when building from a published crate (scripts/ directory absent) or when
    // Foundry is not installed.
    let scripts_dir = crate_dir.join("scripts");
    if !scripts_dir.exists() {
        return;
    }

    let forge_available =
        Command::new("forge").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status();

    match forge_available {
        Ok(status) if status.success() => {}
        _ => {
            println!(
                "cargo::warning=Foundry not found, skipping system contract bytecode validation"
            );
            return;
        }
    }

    let artifacts_dir = crate_dir.join("artifacts");

    // Validate each contract
    for config in &contracts {
        validate_contract_bytecode(crate_dir, config);

        let prefix = format!("{}-", config.name);
        validate_versioned_artifacts(&artifacts_dir, &prefix);
    }
}
