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

/// Every field a legacy run prints, a Satin run of the same command prints too, at the same path
/// and of the same JSON kind: Satin's extra output is additive only.
///
/// Each `test_satin_<name>.json` is `test_<name>.json` run with `--spec Satin`; both pin their
/// full output, so comparing the pinned outputs compares what the two engines print.
#[test]
fn test_satin_output_is_additive_to_legacy_output() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut pairs = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let Some(rest) = name.strip_prefix("test_satin_") else { continue };
        let legacy_path = dir.join(format!("test_{rest}"));
        let load = |p: &Path| -> Fixture {
            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
        };
        let (satin, legacy) = (load(&path), load(&legacy_path));
        assert_eq!(
            satin.args[..satin.args.len() - 2],
            legacy.args[..],
            "{name} runs the legacy fixture's command with `--spec Satin` appended"
        );
        assert_eq!(satin.args[satin.args.len() - 2..], ["--spec", "Satin"], "{name}");
        assert_superset(&satin.expected, &legacy.expected, &name, "$");
        assert!(satin.expected.get("satin").is_some_and(|v| v.is_object()), "{name}");
        assert!(legacy.expected.get("satin").is_none(), "a legacy run has no `satin` field");
        pairs += 1;
    }
    assert!(pairs >= 14, "every run and tx fixture has its Satin twin, found {pairs}");
}

/// Asserts that every object key of `legacy` is in `satin` at the same path, with the same JSON
/// kind, recursively. Array elements are compared up to the shorter length: an opcode trace is
/// as long as the run, and Satin prices the run differently, not with other opcodes.
fn assert_superset(satin: &serde_json::Value, legacy: &serde_json::Value, name: &str, at: &str) {
    use serde_json::Value;
    let kind = |v: &Value| match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    assert_eq!(kind(satin), kind(legacy), "{name}: {at} changed kind");
    match (satin, legacy) {
        (Value::Object(s), Value::Object(l)) => {
            for (key, value) in l {
                let found = s.get(key).unwrap_or_else(|| panic!("{name}: {at}.{key} is missing"));
                assert_superset(found, value, name, &format!("{at}.{key}"));
            }
        }
        (Value::Array(s), Value::Array(l)) => {
            for (i, (sv, lv)) in s.iter().zip(l).enumerate() {
                assert_superset(sv, lv, name, &format!("{at}[{i}]"));
            }
        }
        _ => {}
    }
}
