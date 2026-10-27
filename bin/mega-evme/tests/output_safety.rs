//! Diagnostic output must not render terminal controls or endpoint credentials.

use std::process::Command;

use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

fn cli() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mega-evme"));
    cmd.env_remove("RUST_LOG").arg("--log.no-color");
    cmd
}

#[test]
fn test_verbose_command_errors_are_filtered_on_stderr_and_in_a_log_file() {
    let dir = tempfile::tempdir().unwrap();
    for to_file in [false, true] {
        let log = dir.path().join("command.log");
        let mut cmd = cli();
        cmd.args([
            "run",
            "0x00",
            "--spec",
            "Satin",
            "--sender.balance",
            "1bogus\u{202e}unit",
            "-v",
        ]);
        if to_file {
            cmd.arg("--log.file").arg(&log);
        }
        let output = cmd.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).unwrap();
        let logged = if to_file { std::fs::read_to_string(&log).unwrap() } else { stderr.clone() };
        assert!(logged.contains("Error executing command"), "{logged}");
        for text in [&stderr, &logged] {
            assert!(!text.contains('\u{202e}') && !text.contains('\u{1b}'), "{text:?}");
        }
    }
}

#[test]
fn test_a_log_file_creation_failure_filters_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("\u{1b}[2J\u{202e}missing-parent").join("tool.log");
    let output = cli()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_endpoint_paths_and_queries_reach_no_diagnostic_output() {
    for response_kind in [
        "connection",
        "http",
        "malformed",
        "escaped-http",
        "escaped-malformed",
        "wrong-type",
        "rpc",
        "large-number",
        "deep-data",
    ] {
        let server = MockServer::start().await;
        let url = if response_kind == "connection" {
            "http://alice:userinfo-secret@127.0.0.1:9/v2/(path-secret)?key=query-secret#fragment-secret".to_string()
        } else {
            format!("{}/v2/(path-secret)?key=query-secret#fragment-secret", server.uri())
        };
        if response_kind != "connection" {
            let body = if matches!(response_kind, "large-number" | "deep-data") {
                let data = if response_kind == "large-number" {
                    "1e400".to_string()
                } else {
                    format!("{}\"{url}\"{}", "[".repeat(200), "]".repeat(200))
                };
                format!("{{\"jsonrpc\":\"2.0\",\"id\":0,\"error\":{{\"code\":-32603,\"message\":\"failed\",\"data\":{data}}}}}")
            } else if response_kind == "wrong-type" {
                serde_json::to_string(&url).unwrap()
            } else if matches!(response_kind, "escaped-http" | "escaped-malformed") {
                format!("{{\"url\":\"{}\"}}", url.replace('/', "\\/"))
            } else if response_kind == "rpc" {
                serde_json::json!({"jsonrpc":"2.0", "id":0, "error":{
                    "code":-32603, "message":format!("failed {url}"), "data":{(url.clone()):url.clone()}
                }}).to_string()
            } else {
                format!("upstream rejected {url}")
            };
            Mock::given(matchers::method("POST"))
                .respond_with(
                    ResponseTemplate::new(if matches!(response_kind, "http" | "escaped-http") {
                        503
                    } else {
                        200
                    })
                    .set_body_string(body),
                )
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        for to_file in [false, true] {
            let log = dir.path().join("rpc.log");
            let mut cmd = cli();
            cmd.env("RUST_LOG", "mega_evme=debug,alloy_transport_http=debug");
            cmd.args(["replay", &format!("0x{}", "11".repeat(32)), "--override.spec", "Satin"])
                .args(["--rpc", &url, "--rpc.max-retries", "0", "--rpc.no-cache-file", "-vvvv"]);
            if to_file {
                cmd.arg("--log.file").arg(&log);
            }
            let output = tokio::task::spawn_blocking(move || cmd.output()).await.unwrap().unwrap();
            assert_eq!(output.status.code(), Some(1), "{response_kind}");
            let mut text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if to_file {
                text.push_str(&std::fs::read_to_string(&log).unwrap());
            }
            assert!(text.contains("Provider mode: online RPC"), "{response_kind}: {text}");
            assert!(
                !text.contains("Custom backtrace") && !text.contains("panicked"),
                "{response_kind}: {text}"
            );
            for secret in
                ["path-secret", "query-secret", "fragment-secret", "userinfo-secret", "alice"]
            {
                assert!(!text.contains(secret), "{response_kind}, {to_file}: {text}");
            }
        }
    }
}
