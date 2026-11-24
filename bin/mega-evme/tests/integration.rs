//! Integration tests for mega-evme CLI commands.
//!
//! Test cases are defined as JSON fixture files in `tests/fixtures/`.
//! Each fixture contains `args` (the CLI arguments) and `expected` (the JSON output).
//! Tests are discovered automatically via rstest's `#[files]` attribute.
//!
//! To add a test, create a new `test_*.json` file in `tests/fixtures/`.
//! To regenerate a fixture's expected output, update the `expected` field manually.

use std::{path::Path, process::Command};

use rstest::rstest;
use serde::Deserialize;

/// A test fixture: CLI args + expected JSON output.
#[derive(Deserialize)]
struct Fixture {
    /// Human-readable description of what this test covers.
    #[allow(dead_code)]
    description: String,
    /// CLI arguments to pass to mega-evme (without `--json`).
    args: Vec<String>,
    /// Expected JSON output from stdout.
    expected: serde_json::Value,
}

/// Load a fixture, run mega-evme with `--json`, and assert output matches expected.
fn check(path: &Path) {
    let fixtures_dir = path.parent().unwrap();
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read fixture {}: {e}", path.display()));
    let fixture: Fixture = serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse fixture {}: {e}", path.display()));

    // Expand {fixtures} placeholder in args and append --json
    let mut args: Vec<String> = fixture
        .args
        .iter()
        .map(|a| a.replace("{fixtures}", fixtures_dir.to_str().unwrap()))
        .collect();
    args.push("--json".to_string());

    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(&args)
        .output()
        .expect("failed to execute mega-evme");

    assert!(
        output.status.success(),
        "mega-evme failed for {}.\nargs: {args:?}\nstdout: {}\nstderr: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let actual: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("Failed to parse JSON output for {}: {e}\nstdout: {stdout}", path.display())
    });

    assert_eq!(
        actual,
        fixture.expected,
        "JSON mismatch for {}.\n\nExpected:\n{}\n\nActual:\n{}",
        path.display(),
        serde_json::to_string_pretty(&fixture.expected).unwrap(),
        serde_json::to_string_pretty(&actual).unwrap()
    );
}

#[rstest]
fn test_fixture(
    #[base_dir = "./tests/fixtures"]
    #[files("test_*.json")]
    path: std::path::PathBuf,
) {
    check(&path);
}

/// `run` and `tx` announce how execution ended under their own module path, so
/// a `RUST_LOG` filter that names the command keeps matching the line.
#[test]
fn test_execution_result_is_logged_under_the_command_target() {
    for (target, args) in [
        ("mega_evme::run::cmd", vec!["run", "0x00"]),
        (
            "mega_evme::tx::cmd",
            vec!["tx", "--receiver", "0x00000000000000000000000000000000000000aa"],
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
            .args(&args)
            .arg("--log.no-color")
            .env("RUST_LOG", format!("{target}=info"))
            .output()
            .expect("failed to execute mega-evme");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?} failed.\nstderr: {stderr}");
        assert!(
            stderr.contains("Execution succeeded"),
            "the outcome line must be logged under {target}.\nstderr: {stderr}"
        );
        assert!(
            !stderr.contains("Evm executing"),
            "the filter must only match {target}.\nstderr: {stderr}"
        );
    }
}

/// The shared argument types stay reachable through `run`, the path library
/// users imported them from before they moved to `common`.
#[test]
fn test_run_reexports_the_shared_argument_types() {
    let _: mega_evme::run::TracerType = mega_evme::common::TracerType::Opcode;
    let _: Option<mega_evme::run::TraceArgs> = None::<mega_evme::common::TraceArgs>;
}
