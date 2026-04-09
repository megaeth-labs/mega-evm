//! Build script that validates system contract bytecode.
//!
//! When building from the repository (detected by the presence of `scripts/`):
//! 1. Foundry (`forge`) is **required** — the build will fail if it is not installed.
//! 2. Compiles the Solidity contracts using Foundry and validates that the compiled bytecode
//!    matches `*-latest.json`.
//!
//! When building from a published crate (`scripts/` is excluded from the package):
//! - Foundry is not required.
//!
//! In both cases:
//! 3. Regenerates Rust constants from the artifact JSON files and verifies they match the
//!    pre-generated files in `src/generated/`.
//!
//! The pre-generated Rust constants in `src/generated/` are always used directly by `lib.rs`.

use std::{
    env,
    fmt::Write,
    fs,
    path::Path,
    process::{Command, Stdio},
};

use alloy_primitives::{hex, keccak256, Bytes, B256};
use semver::Version;
use serde::Deserialize;

/// Artifact format for system contract JSON files
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContractArtifact {
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
    /// Pre-generated Rust file name (e.g., `oracle_artifacts.rs`)
    generated_file: &'a str,
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

/// Collects all versioned artifacts for a contract, validates code hashes, and returns sorted list.
fn collect_versioned_artifacts(artifacts_dir: &Path, prefix: &str) -> Vec<ContractArtifact> {
    let mut versions = Vec::new();

    for entry in fs::read_dir(artifacts_dir).expect("Failed to read artifacts directory") {
        let entry = entry.expect("Failed to read directory entry");
        let path = entry.path();
        let filename = path.file_name().unwrap().to_str().unwrap();

        // Skip non-versioned files and *-latest.json aliases (symlinks in repo, regular files
        // in packaged crates).
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

        versions.push(artifact);
    }

    // Sort by semantic version
    versions.sort_by_key(|a| a.version.clone());

    versions
}

/// Generates Rust source content with bytecode constants for a contract.
fn generate_rust_constants(
    config: &ContractConfig<'_>,
    versions: &[ContractArtifact],
    latest: &ContractArtifact,
) -> String {
    let mut content = String::new();

    writeln!(content, "// Auto-generated {} contract bytecode constants.", config.name).unwrap();
    writeln!(content, "// DO NOT EDIT - generated by build.rs from artifacts/").unwrap();
    writeln!(content).unwrap();
    writeln!(content, "use alloy_primitives::{{bytes, b256, Bytes, B256}};").unwrap();
    writeln!(content).unwrap();

    for artifact in versions {
        let version_underscore = artifact.version.to_string().replace('.', "_");
        let const_name = format!("V{}", version_underscore);

        writeln!(content, "/// `{}` contract bytecode v{}", config.name, artifact.version).unwrap();
        writeln!(
            content,
            "pub const {}_CODE: Bytes = bytes!(\"{}\");",
            const_name,
            hex::encode(&artifact.deployed_bytecode)
        )
        .unwrap();
        writeln!(content, "/// `{}` contract code hash v{}", config.name, artifact.version)
            .unwrap();
        writeln!(
            content,
            "pub const {}_CODE_HASH: B256 = b256!(\"{}\");",
            const_name,
            hex::encode(artifact.code_hash)
        )
        .unwrap();
        writeln!(content).unwrap();
    }

    // Add latest alias
    let latest_version_underscore = latest.version.to_string().replace('.', "_");
    writeln!(content, "/// Latest `{}` contract bytecode", config.name).unwrap();
    writeln!(content, "pub const LATEST_CODE: Bytes = V{}_CODE;", latest_version_underscore)
        .unwrap();
    writeln!(content, "/// Latest `{}` contract code hash", config.name).unwrap();
    writeln!(
        content,
        "pub const LATEST_CODE_HASH: B256 = V{}_CODE_HASH;",
        latest_version_underscore
    )
    .unwrap();

    content
}

/// Verifies that the pre-generated Rust constants in `src/generated/` match what would be
/// generated from the current artifact JSON files.
fn verify_generated_constants(
    crate_dir: &Path,
    config: &ContractConfig<'_>,
    versions: &[ContractArtifact],
    latest: &ContractArtifact,
) {
    let expected = generate_rust_constants(config, versions, latest);
    let generated_path = crate_dir.join("src/generated").join(config.generated_file);

    let actual = fs::read_to_string(&generated_path).unwrap_or_else(|_| {
        panic!(
            "Failed to read pre-generated file src/generated/{}. \
             Run the build script to regenerate it.",
            config.generated_file
        )
    });

    assert!(
        expected == actual,
        r#"
ERROR: Pre-generated constants in src/generated/{file} are out of sync with artifacts!

The artifact JSON files have changed but src/generated/{file} was not regenerated.

To fix this, run:
  cargo build -p mega-system-contracts

Then copy the generated file from the build output:
  cp target/debug/build/mega-system-contracts-*/out/{file} \
     crates/system-contracts/src/generated/{file}

Or run the regeneration script if available.
"#,
        file = config.generated_file,
    );
}

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let crate_dir = Path::new(&manifest_dir);

    // Define contract configurations
    let contracts = [
        ContractConfig {
            name: "Oracle",
            script_path: "scripts/OracleBytecode.s.sol:SaveOracleBytecode",
            generated_file: "oracle_artifacts.rs",
        },
        ContractConfig {
            name: "KeylessDeploy",
            script_path: "scripts/KeylessDeployBytecode.s.sol:SaveKeylessDeployBytecode",
            generated_file: "keyless_deploy_artifacts.rs",
        },
        ContractConfig {
            name: "MegaAccessControl",
            script_path: "scripts/MegaAccessControlBytecode.s.sol:SaveMegaAccessControlBytecode",
            generated_file: "access_control_artifacts.rs",
        },
        ContractConfig {
            name: "MegaLimitControl",
            script_path: "scripts/MegaLimitControlBytecode.s.sol:SaveMegaLimitControlBytecode",
            generated_file: "limit_control_artifacts.rs",
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

    let artifacts_dir = crate_dir.join("artifacts");

    // Detect whether we are building from the repository (scripts/ directory present) or from a
    // published crate (scripts/ excluded by Cargo.toml `exclude`).
    let is_repo_build = crate_dir.join("scripts").exists();

    // Phase 1: Forge validation — required when building from the repository.
    if is_repo_build {
        let forge_available = Command::new("forge")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        match forge_available {
            Ok(status) if status.success() => {}
            _ => {
                panic!(
                    r#"
ERROR: `forge` command not found

Foundry is required to build system-contracts from the repository.
Install it from: https://getfoundry.sh

Quick install:
  curl -L https://foundry.paradigm.xyz | bash
  foundryup
"#
                );
            }
        }

        for config in &contracts {
            validate_contract_bytecode(crate_dir, config);
        }
    }

    // Phase 2: Verify pre-generated constants match artifacts (always runs if artifacts exist).
    if artifacts_dir.exists() {
        for config in &contracts {
            let prefix = format!("{}-", config.name);
            let versions = collect_versioned_artifacts(&artifacts_dir, &prefix);

            // Determine the latest version from the *-latest.json file.
            let latest_path = artifacts_dir.join(format!("{}-latest.json", config.name));
            let latest_content = fs::read_to_string(&latest_path)
                .unwrap_or_else(|_| panic!("Failed to read {}-latest.json", config.name));
            let latest: ContractArtifact = serde_json::from_str(&latest_content)
                .unwrap_or_else(|_| panic!("Failed to parse {}-latest.json", config.name));

            verify_generated_constants(crate_dir, config, &versions, &latest);
        }
    }
}
