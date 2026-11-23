//! Developer commands for attesting Solidity bytecode and regenerating Rust sources.

use std::{env, fs, path::Path, process::Command};

use anyhow::{bail, ensure, Context, Result};
use walkdir::WalkDir;

#[path = "../../../crates/system-contracts/artifact_checks.rs"]
mod artifact_checks;

use artifact_checks::{check_generated, generated_sources, ContractArtifact, CONTRACTS};

fn main() -> Result<()> {
    let crate_dir = Path::new("crates/system-contracts");
    match env::args().nth(1).as_deref() {
        Some("attest") => attest(crate_dir),
        Some("check") => check_generated(crate_dir),
        Some("generate") => {
            let sources = generated_sources(crate_dir)?;
            fs::create_dir_all(crate_dir.join("src/generated"))?;
            for (path, content) in sources {
                fs::write(&path, content)
                    .with_context(|| format!("Could not write {}", path.display()))?;
            }
            Ok(())
        }
        _ => bail!("Usage: system-contract-artifacts <attest|check|generate>"),
    }
}

fn attest(crate_dir: &Path) -> Result<()> {
    let artifacts = crate_dir.join("artifacts");
    let project = tempfile::tempdir()?;
    stage_foundry_project(crate_dir, project.path())?;

    for contract in CONTRACTS {
        let name = contract.name;
        let expected = contract.latest_artifact(&artifacts)?;
        let script = format!("scripts/{name}Bytecode.s.sol:Save{name}Bytecode");
        let status = Command::new("forge")
            .args(["script", &script, "--sig", "run()"])
            .current_dir(project.path())
            .status()
            .with_context(|| format!("Could not execute forge script for {name}"))?;
        ensure!(status.success(), "Forge script for {name} failed: {status}");

        let compiled =
            ContractArtifact::read(&project.path().join(format!("artifacts/{name}.json")))?;
        ensure!(
            compiled == expected,
            "{name} contract bytecode mismatch with artifacts/{name}-latest.json.\n\
             Expected version {} and code hash {:#x}.\n\
             Compiled version {} and code hash {:#x}.\n\
             For an intentional upgrade, add a versioned artifact and update the latest alias.",
            expected.version,
            expected.code_hash,
            compiled.version,
            compiled.code_hash,
        );
        println!("Attested {name} v{} ({:#x})", expected.version, expected.code_hash);
    }
    Ok(())
}

fn stage_foundry_project(source: &Path, scratch: &Path) -> Result<()> {
    // Forge and the deployment scripts write paths relative to this project.
    fs::create_dir(scratch.join("artifacts"))?;
    for file in ["foundry.toml", "foundry.lock", "remappings.txt"] {
        fs::copy(source.join(file), scratch.join(file))?;
    }
    for tree in ["contracts", "scripts", "lib/forge-std/src"] {
        let root = source.join(tree);
        fs::create_dir_all(scratch.join(tree))?;
        for entry in WalkDir::new(&root).min_depth(1) {
            let entry = entry.with_context(|| format!("Could not walk {}", root.display()))?;
            let destination = scratch.join(entry.path().strip_prefix(source)?);
            if entry.file_type().is_dir() {
                fs::create_dir(&destination)?;
            } else {
                fs::copy(entry.path(), &destination)?;
            }
        }
    }
    Ok(())
}
