//! Command-line checks for the generated system contract sources.

use std::{fs, path::Path, process::Command};

use anyhow::Result;
use tempfile::TempDir;

const ORACLE_SOURCE: &str = "crates/system-contracts/src/generated/oracle_artifacts.rs";

fn contract_fixture() -> Result<TempDir> {
    let repo = tempfile::tempdir()?;
    let crate_dir = repo.path().join("crates/system-contracts");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/system-contracts");
    for relative_path in ["artifacts", "src/generated"] {
        let destination = crate_dir.join(relative_path);
        fs::create_dir_all(&destination)?;
        for entry in fs::read_dir(source.join(relative_path))? {
            let entry = entry?;
            fs::copy(entry.path(), destination.join(entry.file_name()))?;
        }
    }
    Ok(repo)
}

#[test]
fn test_check_reports_stale_constants_without_writing() -> Result<()> {
    let repo = contract_fixture()?;
    let generated = repo.path().join(ORACLE_SOURCE);
    let stale_contents = "// Stale constants\n";
    fs::write(&generated, stale_contents)?;

    let result = Command::new(env!("CARGO_BIN_EXE_system-contract-artifacts"))
        .arg("check")
        .current_dir(repo.path())
        .output()?;

    assert!(!result.status.success());
    let stderr = String::from_utf8(result.stderr)?;
    assert!(stderr.contains("oracle_artifacts.rs is out of sync"), "{stderr}");
    assert!(stderr.contains("mise run system-contracts:generate"), "{stderr}");
    assert_eq!(fs::read_to_string(generated)?, stale_contents);
    Ok(())
}

#[test]
fn test_generate_restores_stale_constants_from_artifacts() -> Result<()> {
    let repo = contract_fixture()?;
    let generated = repo.path().join(ORACLE_SOURCE);
    let expected = fs::read(&generated)?;
    fs::write(&generated, "// Stale constants\n")?;

    let result = Command::new(env!("CARGO_BIN_EXE_system-contract-artifacts"))
        .arg("generate")
        .current_dir(repo.path())
        .output()?;

    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(fs::read(generated)?, expected);
    Ok(())
}

#[test]
fn test_invalid_artifact_version_is_rejected_before_writing() -> Result<()> {
    let repo = contract_fixture()?;
    let artifacts = repo.path().join("crates/system-contracts/artifacts");
    let filename = "SequencerRegistry-1.0.1.json";
    fs::copy(artifacts.join("SequencerRegistry-1.0.0.json"), artifacts.join(filename))?;
    let generated = repo.path().join(ORACLE_SOURCE);
    let stale_contents = "// Stale constants\n";
    fs::write(&generated, stale_contents)?;

    for operation in ["generate", "check"] {
        let result = Command::new(env!("CARGO_BIN_EXE_system-contract-artifacts"))
            .arg(operation)
            .current_dir(repo.path())
            .output()?;

        let stderr = String::from_utf8(result.stderr)?;
        assert!(!result.status.success(), "{operation}: {stderr}");
        assert!(
            stderr.contains(&format!("Version mismatch for artifact {filename}")),
            "{operation}: {stderr}"
        );
        assert_eq!(fs::read_to_string(&generated)?, stale_contents, "{operation}");
    }
    Ok(())
}
