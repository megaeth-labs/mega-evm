//! Inventory validation at the attestation command boundary.

use std::{fs, path::Path, process::Command};

use anyhow::Result;

#[test]
fn test_attest_rejects_a_short_inventory_before_compiling() -> Result<()> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/system-contracts");
    for (dir, missing_file) in [
        ("artifacts", "Oracle-latest.json"),
        ("contracts", "Oracle.sol"),
        ("scripts", "OracleBytecode.s.sol"),
    ] {
        let fixture = tempfile::tempdir()?;
        for relative_path in ["artifacts", "contracts", "scripts"] {
            let target = fixture.path().join(relative_path);
            fs::create_dir(&target)?;
            for entry in fs::read_dir(source.join(relative_path))? {
                let entry = entry?;
                if entry.path().is_file() {
                    fs::copy(entry.path(), target.join(entry.file_name()))?;
                }
            }
        }
        fs::remove_file(fixture.path().join(dir).join(missing_file))?;

        let output = Command::new(env!("CARGO_BIN_EXE_system-contract-artifacts"))
            .arg("attest")
            .arg(fixture.path())
            .output()?;

        let stderr = String::from_utf8(output.stderr)?;
        assert!(!output.status.success(), "{missing_file}");
        assert!(stderr.contains("System contract inventory mismatch"), "{missing_file}: {stderr}");
        assert!(
            stderr.contains(&format!("{dir}: missing [\"Oracle\"]")),
            "{missing_file}: {stderr}"
        );
    }
    Ok(())
}
