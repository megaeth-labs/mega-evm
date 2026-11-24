//! Validates versioned artifacts and renders the Rust constants that embed them.

use std::{
    fmt::Write,
    fs,
    path::{Path, PathBuf},
};

use alloy_primitives::{hex, keccak256, Bytes, B256};
use anyhow::{anyhow, ensure, Context, Result};
use semver::Version;
use serde::Deserialize;

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct ContractArtifact {
    pub(super) version: Version,
    pub(super) code_hash: B256,
    deployed_bytecode: Bytes,
}

impl ContractArtifact {
    pub(super) fn read(path: &Path) -> Result<Self> {
        let content =
            fs::read(path).with_context(|| format!("Could not read {}", path.display()))?;
        let artifact: Self = serde_json::from_slice(&content)
            .with_context(|| format!("Could not parse {}", path.display()))?;

        let computed_hash = keccak256(&artifact.deployed_bytecode);
        ensure!(
            computed_hash == artifact.code_hash,
            "Code hash mismatch for {}: recorded {:#x}, computed {computed_hash:#x}",
            path.display(),
            artifact.code_hash,
        );
        Ok(artifact)
    }
}

pub(super) struct Contract {
    pub(super) name: &'static str,
    generated_file: &'static str,
}

pub(super) const CONTRACTS: &[Contract] = &[
    Contract { name: "Oracle", generated_file: "oracle_artifacts.rs" },
    Contract { name: "KeylessDeploy", generated_file: "keyless_deploy_artifacts.rs" },
    Contract { name: "MegaAccessControl", generated_file: "access_control_artifacts.rs" },
    Contract { name: "MegaLimitControl", generated_file: "limit_control_artifacts.rs" },
    Contract { name: "SequencerRegistry", generated_file: "sequencer_registry_artifacts.rs" },
];

impl Contract {
    pub(super) fn latest_artifact(&self, artifacts_dir: &Path) -> Result<ContractArtifact> {
        ContractArtifact::read(&artifacts_dir.join(format!("{}-latest.json", self.name)))
    }

    fn versioned_artifacts(&self, artifacts_dir: &Path) -> Result<Vec<ContractArtifact>> {
        let prefix = format!("{}-", self.name);
        let mut versions = Vec::new();
        for entry in fs::read_dir(artifacts_dir)
            .with_context(|| format!("Could not list {}", artifacts_dir.display()))?
        {
            let entry = entry?;
            let filename = entry.file_name();
            let filename = filename.to_string_lossy();
            let Some(filename_version) =
                filename.strip_prefix(&prefix).and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            if filename_version == "latest" {
                continue;
            }

            let version = Version::parse(filename_version)
                .map_err(|err| anyhow!("Invalid version in artifact filename {filename}: {err}"))?;
            let artifact = ContractArtifact::read(&entry.path())?;
            ensure!(
                version == artifact.version,
                "Version mismatch for artifact {filename}: filename says {version}, JSON says {}",
                artifact.version
            );
            versions.push(artifact);
        }
        versions.sort_by(|a, b| a.version.cmp(&b.version));
        for pair in versions.windows(2) {
            ensure!(
                pair[0].version != pair[1].version,
                "Duplicate artifact version {} for {}",
                pair[0].version,
                self.name
            );
        }
        Ok(versions)
    }
}

pub(super) fn generated_sources(crate_dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    let artifacts_dir = crate_dir.join("artifacts");
    let generated_dir = crate_dir.join("src/generated");
    CONTRACTS
        .iter()
        .map(|contract| {
            let versions = contract.versioned_artifacts(&artifacts_dir)?;
            let latest = contract.latest_artifact(&artifacts_dir)?;

            // The generated alias names its target by version. Bind the version and code hash
            // together so an alias cannot point to a different version's bytecode.
            ensure!(
            versions.iter().any(|v| v.version == latest.version && v.code_hash == latest.code_hash),
            "{}-latest.json (version {}, code hash {:#x}) does not match any versioned artifact \
             on both version and code hash",
            contract.name,
            latest.version,
            latest.code_hash
        );

            Ok((
                generated_dir.join(contract.generated_file),
                generate_rust_constants(contract, &versions, &latest),
            ))
        })
        .collect()
}

pub(super) fn check_generated(crate_dir: &Path) -> Result<()> {
    for (path, expected) in generated_sources(crate_dir)? {
        let actual = fs::read(&path).with_context(|| {
            format!("Could not read {}. Run `mise run system-contracts:generate`.", path.display())
        })?;
        ensure!(
            actual == expected.as_bytes(),
            "{} is out of sync with artifacts. Run `mise run system-contracts:generate`.",
            path.display()
        );
    }
    Ok(())
}

/// Generates Rust source content with bytecode constants for a contract.
fn generate_rust_constants(
    contract: &Contract,
    versions: &[ContractArtifact],
    latest: &ContractArtifact,
) -> String {
    let mut content = String::new();

    writeln!(content, "// Pre-generated {} contract bytecode constants.", contract.name).unwrap();
    // Keep the original header so unchanged artifacts reproduce the checked-in files.
    writeln!(
        content,
        "// DO NOT EDIT MANUALLY - updated automatically by build.rs from artifacts/"
    )
    .unwrap();
    writeln!(content).unwrap();
    writeln!(content, "use alloy_primitives::{{bytes, b256, Bytes, B256}};").unwrap();
    writeln!(content).unwrap();

    for artifact in versions {
        let version_underscore = artifact.version.to_string().replace('.', "_");
        let const_name = format!("V{}", version_underscore);

        writeln!(content, "/// `{}` contract bytecode v{}", contract.name, artifact.version)
            .unwrap();
        writeln!(
            content,
            "pub const {}_CODE: Bytes = bytes!(\"{}\");",
            const_name,
            hex::encode(&artifact.deployed_bytecode)
        )
        .unwrap();
        writeln!(content, "/// `{}` contract code hash v{}", contract.name, artifact.version)
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
    writeln!(content, "/// Latest `{}` contract bytecode", contract.name).unwrap();
    writeln!(content, "pub const LATEST_CODE: Bytes = V{}_CODE;", latest_version_underscore)
        .unwrap();
    writeln!(content, "/// Latest `{}` contract code hash", contract.name).unwrap();
    writeln!(
        content,
        "pub const LATEST_CODE_HASH: B256 = V{}_CODE_HASH;",
        latest_version_underscore
    )
    .unwrap();

    content
}
