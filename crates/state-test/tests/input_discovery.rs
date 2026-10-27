//! Discovery limits must fail the CLI rather than execute an incomplete collection.

use std::process::Command;

fn check_discovery_depth(args: &[&str]) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("visible.json"), "{}").unwrap();
    let mut nested = root.path().to_path_buf();
    for _ in 0..63 {
        nested.push("d");
    }
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("boundary.json"), "{}").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_state-test")).args(args).arg(root.path()).output().unwrap()
    };
    let boundary = run();
    assert!(boundary.status.success(), "{}", String::from_utf8_lossy(&boundary.stderr));
    assert!(String::from_utf8_lossy(&boundary.stdout).contains("2 files"));

    nested.push("one-too-deep");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("hidden.json"), "{}").unwrap();
    let output = run();
    assert!(!output.status.success(), "deep fixture tree was accepted");
    assert!(String::from_utf8_lossy(&output.stderr).contains("depth exceeds 64"));
}

#[test]
fn test_state_cli_refuses_excessive_fixture_depth() {
    check_discovery_depth(&["--fork", "Osaka"]);
}

#[test]
fn test_blockchain_cli_refuses_excessive_fixture_depth() {
    check_discovery_depth(&["btest"]);
}
