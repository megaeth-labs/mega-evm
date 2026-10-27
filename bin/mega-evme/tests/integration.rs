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
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

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

/// An error prints once, on stderr, and stdout stays empty. A legacy spec keeps the line the
/// 1.7.1 binary printed on stderr (`Error:` plus the error's `Debug`). Satin prints the error's
/// message. Both exit with code 1.
#[cfg(feature = "legacy")]
#[test]
fn test_errors_print_as_their_engine_prints_them() {
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_mega-evme")).args(args).output().unwrap();
        let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
        (output.status.code(), text(output.stdout), text(output.stderr))
    };

    let (code, stdout, stderr) = run(&["run", "--codefile", "/nonexistent/code.hex"]);
    assert_eq!(code, Some(1));
    assert_eq!(stdout, "");
    assert!(stderr.starts_with("Error: Evme(FileRead(Os {"), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");

    let (code, stdout, stderr) =
        run(&["run", "--codefile", "/nonexistent/code.hex", "--spec", "Satin"]);
    assert_eq!(code, Some(1));
    assert_eq!(stdout, "");
    assert_eq!(
        stderr.lines().collect::<Vec<_>>(),
        ["Failed to read file: No such file or directory (os error 2)"]
    );
}

/// A legacy `run` or `tx` that leaves `--spec` out runs on the tool's default spec whatever shape
/// its arguments take: a `--` ending the options, which the 1.7.1 CLI accepts, still prints what
/// the pinned fixture and the same command with `--spec Rex6` print.
#[cfg(feature = "legacy")]
#[test]
fn test_a_legacy_command_ending_its_options_runs_on_the_default_spec() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let fixture = |name: &str| -> Fixture {
        serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
    };
    let run = |args: &[&str]| -> serde_json::Value {
        let output = Command::new(env!("CARGO_BIN_EXE_mega-evme")).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?} exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    // `run`, the code after the separator.
    let basic = fixture("test_run_basic_return.json");
    let ["run", code] = basic.args.iter().map(String::as_str).collect::<Vec<_>>()[..] else {
        panic!("the fixture runs one code: {:?}", basic.args)
    };
    assert_eq!(run(&["run", "--json", "--", code]), basic.expected);
    assert_eq!(run(&["run", "--spec", "Rex6", "--json", "--", code]), basic.expected);

    // `tx`, a separator with nothing after it.
    let transfer = fixture("test_tx_value_transfer.json");
    let mut args: Vec<&str> = transfer.args.iter().map(String::as_str).collect();
    args.extend(["--json", "--"]);
    assert_eq!(run(&args), transfer.expected);
}

/// Runs the binary and returns its stdout, failing on a non-zero exit.
fn run_ok(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme")).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?} exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// `SELFBALANCE PUSH0 MSTORE PUSH1 32 PUSH0 RETURN`: returns the executing account's balance.
const SELF_BALANCE_CODE: &str = "0x475f5260205ff3";

/// The 32-byte word `value` as the `Output:` line of a human run prints it.
fn word(value: u64) -> String {
    format!("0x{value:064x}")
}

/// A `--balance` or `--faucet` override of the `run` receiver survives the code `run` installs
/// at that address afterwards.
#[test]
fn test_a_balance_override_of_the_run_receiver_survives_its_code() {
    let receiver = "0x2000000000000000000000000000000000000002";
    let balance = format!("{receiver}=1000");
    let faucet = format!("{receiver}+=1000");
    for overrides in [["--balance", balance.as_str()], ["--faucet", faucet.as_str()]] {
        let mut args = vec!["run", "--spec", "Satin", SELF_BALANCE_CODE, "--receiver", receiver];
        args.extend(overrides);
        let stdout = run_ok(&args);
        assert!(stdout.contains(&word(1000)), "{overrides:?}: {stdout}");
    }
}

/// A `--balance` or `--faucet` override of a system predeploy survives the deployment of its code.
#[test]
fn test_a_balance_override_of_a_predeploy_survives_its_deployment() {
    // A contract that returns the balance of the MegaAccessControl predeploy:
    // PUSH20 <addr> BALANCE PUSH0 MSTORE PUSH1 32 PUSH0 RETURN.
    let predeploy = mega_evm::system::ACCESS_CONTROL_ADDRESS;
    let code = format!("0x73{}315f5260205ff3", alloy_primitives::hex::encode(predeploy));
    let balance = format!("{predeploy}=777");
    let faucet = format!("{predeploy}+=777");
    for overrides in [["--balance", balance.as_str()], ["--faucet", faucet.as_str()]] {
        let mut args = vec!["run", "--spec", "Satin", code.as_str()];
        args.extend(overrides);
        let stdout = run_ok(&args);
        assert!(stdout.contains(&word(777)), "{overrides:?}: {stdout}");
    }
}

/// A pipe whose read end is already closed: a write to it fails with a broken pipe.
fn closed_pipe() -> std::io::PipeWriter {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    writer
}

/// A command that fails exits 1 whatever happens to the line that reports it: with stderr a
/// closed pipe the exit status is the only failure signal left.
#[test]
fn test_a_failing_command_exits_1_when_stderr_is_a_closed_pipe() {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["run", "--spec", "Satin", "0xef01"])
        .stderr(closed_pipe())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

/// A run that succeeds, with stdout a closed pipe (`mega-evme … | head -0`), ends quietly: no
/// panic, no backtrace, and the exit status of the run.
#[test]
fn test_a_successful_run_does_not_panic_when_stdout_is_a_closed_pipe() {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["run", "--spec", "Satin", "0x00"])
        .stdout(closed_pipe())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr, "", "a closed stdout is not an error to report");
    assert_eq!(output.status.code(), Some(0));
}

/// Bytecode that reverts with `Error(reason)`.
fn revert_with(reason: &str) -> String {
    use alloy_sol_types::{Revert, SolError};

    let data = Revert::from(reason).abi_encode();
    let mut code = Vec::new();
    for (i, chunk) in data.chunks(32).enumerate() {
        let mut word = [0u8; 32];
        word[..chunk.len()].copy_from_slice(chunk);
        // PUSH32 word, PUSH2 offset, MSTORE.
        code.push(0x7f);
        code.extend_from_slice(&word);
        code.push(0x61);
        code.extend_from_slice(&((i * 32) as u16).to_be_bytes());
        code.push(0x52);
    }
    // PUSH2 length, PUSH0, REVERT.
    code.push(0x61);
    code.extend_from_slice(&(data.len() as u16).to_be_bytes());
    code.extend_from_slice(&[0x5f, 0xfd]);
    alloy_primitives::hex::encode_prefixed(code)
}

/// A revert reason is attacker-chosen text. The terminal output drops what would drive the
/// terminal and keeps its lines; the JSON output carries the reason's characters, escaped.
#[test]
fn test_a_revert_reason_is_filtered_for_the_terminal_and_not_in_json() {
    let reason = "one\n\u{9b}2Jtwo\ttab\u{1b}[H";
    let code = revert_with(reason);

    let human = run_ok(&["run", "--spec", "Satin", &code]);
    assert!(human.contains("Revert Reason:    Error(\"one\n2Jtwo\ttab\")"), "{human}");
    assert!(!human.contains('\u{9b}') && !human.contains('\u{1b}'), "{human:?}");

    let json: serde_json::Value =
        serde_json::from_str(&run_ok(&["run", "--spec", "Satin", &code, "--json"])).unwrap();
    assert_eq!(json["revert_reason"], format!("Error(\"{reason}\")"));
}

/// No place the tool writes the `--rpc` URL carries its userinfo: not the log of a replay, not an
/// error, whether the URL is complete, holds an `@` in its password, or lacks its scheme.
#[test]
fn test_the_rpc_url_userinfo_reaches_no_output() {
    for url in [
        "http://alice:s3cret@127.0.0.1:9/",
        "http://alice:s3c@ret@127.0.0.1:9/",
        "alice:s3cret@127.0.0.1:9",
        "http://alice:s3cret@127.0.0.1:9/v2/path-secret?key=query-secret#fragment-secret",
        "http://bad host/path-secret?key=query-secret",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
            .args(["replay", &format!("0x{}", "11".repeat(32)), "--override.spec", "Satin"])
            .args(["--rpc", url, "--rpc.max-retries", "0", "--rpc.no-cache-file", "-vvv"])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(1), "{url}: {text}");
        assert!(!text.contains("s3c") && !text.contains("ret@"), "{url}: {text}");
        assert!(!text.contains("alice"), "{url}: {text}");
        for secret in ["path-secret", "query-secret", "fragment-secret"] {
            assert!(!text.contains(secret), "{url}: {text}");
        }
    }
}

#[test]
fn test_a_log_file_creation_failure_filters_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("\u{1b}[2J\u{202e}missing-parent").join("tool.log");
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .env_remove("RUST_LOG")
        .arg("--log.no-color")
        .args(["run", "0x00", "--spec", "Satin", "-v"])
        .arg("--log.file")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Failed to create log file") && stderr.contains("tool.log"),
        "{stderr}"
    );
    assert!(!stderr.contains('\u{202e}') && !stderr.contains('\u{1b}'), "{stderr:?}");
}

/// The CLI must also hide raw response diagnostics and dependency spans, on both log writers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rpc_response_errors_hide_credentials_in_stderr_and_log_files() {
    let server = MockServer::start().await;
    let url = format!(
        "{}/v2/(path-secret)?key=query-secret#fragment-secret",
        server.uri().replacen("http://", "http://alice:userinfo-secret@", 1)
    );
    Mock::given(matchers::method("POST"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_string(format!("{{\"url\":\"{}\"}}", url.replace('/', "\\/"))),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    for to_file in [false, true] {
        let path = dir.path().join("rpc.log");
        let before = server.received_requests().await.unwrap().len();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mega-evme"));
        cmd.env("RUST_LOG", "mega_evme=debug,alloy_transport_http=trace,alloy_transport=trace")
            .args([
                "--log.no-color",
                "replay",
                &format!("0x{}", "11".repeat(32)),
                "--override.spec",
                "Satin",
            ])
            .args([
                "--rpc",
                &url,
                "--rpc.max-retries",
                "1",
                "--rpc.backoff-ms",
                "1",
                "--rpc.no-cache-file",
                "-vvvv",
            ]);
        if to_file {
            cmd.arg("--log.file").arg(&path);
        }
        let output = tokio::task::spawn_blocking(move || cmd.output()).await.unwrap().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(server.received_requests().await.unwrap().len() - before, 2);
        let mut text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if to_file {
            text.push_str(&std::fs::read_to_string(&path).unwrap());
        }
        assert!(
            text.contains("Failed to fetch chain ID") && text.contains("Max retries exceeded"),
            "{text}"
        );
        for secret in ["alice", "userinfo-secret", "path-secret", "query-secret", "fragment-secret"]
        {
            assert!(!text.contains(secret), "{text}");
        }
    }
}

const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
const SENDER: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const RECEIVER: &str = "0x0000000000000000000000000000000000000001";

/// The `transactionHash` of the receipt a `tx` run prints.
fn printed_hash(args: &[&str]) -> String {
    let mut full = vec!["tx", "--spec", "Satin", "--json"];
    full.extend_from_slice(args);
    let json: serde_json::Value = serde_json::from_str(&run_ok(&full)).unwrap();
    json["receipt"]["transactionHash"].as_str().unwrap().to_string()
}

/// The hash of the deposit `tx` runs for these flags: the keccak of its EIP-2718 encoding.
fn deposit_hash(source_hash: &str, mint: u128) -> String {
    use alloy_eips::Encodable2718;
    use op_alloy_consensus::{OpTxEnvelope, TxDeposit};

    let deposit = TxDeposit {
        source_hash: source_hash.parse().unwrap(),
        from: SENDER.parse().unwrap(),
        to: alloy_primitives::TxKind::Call(RECEIVER.parse().unwrap()),
        mint,
        value: alloy_primitives::U256::from(1000),
        gas_limit: 100_000,
        is_system_transaction: false,
        input: Default::default(),
    };
    let encoded =
        OpTxEnvelope::Deposit(alloy_consensus::Sealable::seal_slow(deposit)).encoded_2718();
    alloy_primitives::keccak256(encoded).to_string()
}

/// A deposit built from flags prints its own hash: the source hash and the mint are in it, so
/// deposits that differ in either print different hashes, and the hash is the encoding's.
#[test]
fn test_a_deposit_built_from_flags_prints_the_hash_of_what_ran() {
    let one = format!("0x{}", "11".repeat(32));
    let two = format!("0x{}", "22".repeat(32));
    let mut seen = std::collections::BTreeSet::new();
    for (source_hash, mint) in [(&one, 0u128), (&one, 1000), (&two, 0), (&two, 1000)] {
        let mint_flag = mint.to_string();
        let hash = printed_hash(&[
            "--tx-type",
            "126",
            "--sender",
            SENDER,
            "--receiver",
            RECEIVER,
            "--value",
            "1000",
            "--gas",
            "100000",
            "--source-hash",
            source_hash,
            "--mint",
            &mint_flag,
        ]);
        assert_eq!(hash, deposit_hash(source_hash, mint));
        seen.insert(hash);
    }
    assert_eq!(seen.len(), 4, "four different deposits, four hashes");
}

/// A signed transaction built from flags has no hash: the tool signs nothing, so it prints the
/// zero hash and not the hash of an unsigned stand-in no transaction has.
#[test]
fn test_a_signed_transaction_built_from_flags_prints_no_hash() {
    for tx_type in ["0", "1", "2"] {
        let hash = printed_hash(&[
            "--tx-type",
            tx_type,
            "--sender",
            SENDER,
            "--receiver",
            RECEIVER,
            "--value",
            "1000",
            "--sender.balance",
            "1ether",
        ]);
        assert_eq!(hash, ZERO_HASH, "type {tx_type}");
    }
}

/// A raw transaction prints its own hash while it runs as it was signed, and none once a flag
/// changed what runs.
#[test]
fn test_a_raw_transaction_prints_its_hash_until_a_flag_changes_it() {
    use alloy_eips::Encodable2718;
    use op_alloy_consensus::{OpTxEnvelope, TxDeposit};

    let deposit = TxDeposit {
        source_hash: format!("0x{}", "33".repeat(32)).parse().unwrap(),
        from: SENDER.parse().unwrap(),
        to: alloy_primitives::TxKind::Call(RECEIVER.parse().unwrap()),
        mint: 5,
        value: alloy_primitives::U256::from(1000),
        gas_limit: 100_000,
        is_system_transaction: false,
        input: Default::default(),
    };
    let raw = OpTxEnvelope::Deposit(alloy_consensus::Sealable::seal_slow(deposit)).encoded_2718();
    let raw_hex = alloy_primitives::hex::encode_prefixed(&raw);

    assert_eq!(printed_hash(&[&raw_hex]), alloy_primitives::keccak256(&raw).to_string());
    assert_eq!(printed_hash(&[&raw_hex, "--value", "1"]), ZERO_HASH);
}
