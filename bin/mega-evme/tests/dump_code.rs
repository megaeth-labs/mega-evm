//! A prestate account that names its code only by hash (`code: null` with a `codeHash`): offline
//! there is nothing to load it from and the run is refused; forked, its code is fetched and
//! checked against the hash, and the dumped state carries the code, not a null.

use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;
use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

/// The contract the fork serves, and the account the prestate names by hash.
const CONTRACT: &str = "0x00000000000000000000000000000000000000aa";
/// `PUSH1 1 PUSH0 MSTORE PUSH1 32 PUSH0 RETURN`: returns the word 1.
const CONTRACT_CODE: &str = "0x600160005260206000f3";
const CONTRACT_HASH: &str = "0xc1d5b4ce3e2a6227293fccce2904121c8647bbe16c1216340b851bf12d12560e";
/// `PUSH1 0xaa BALANCE POP STOP`: touches the contract's account without loading its code.
const TOUCH_CONTRACT: &str = "0x60aa315000";
/// A second account, holding `TOUCH_CONTRACT`'s code, for a transaction to touch the first.
const TOUCHER: &str = "0x00000000000000000000000000000000000000bb";
/// `STATICCALL` of the contract with all gas and no input, returning its 32-byte output.
const CALL_CONTRACT: &str = "0x60205f5f5f60aa5afa5060205ff3";

/// Serves `eth_chainId` and one contract; every other account is empty.
async fn mount_fork(server: &MockServer) {
    Mock::given(matchers::method("POST"))
        .respond_with(|req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let address = body["params"][0].as_str().unwrap_or("");
            let is_contract = address.eq_ignore_ascii_case(CONTRACT);
            let result = match body["method"].as_str().unwrap_or("") {
                "eth_chainId" => "0x10e6",
                "eth_getBalance" if is_contract => "0x64",
                "eth_getTransactionCount" if is_contract => "0x7",
                "eth_getCode" if is_contract => CONTRACT_CODE,
                "eth_getCode" => "0x",
                _ => "0x0",
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0", "id": body["id"].clone(), "result": result,
            }))
        })
        .mount(server)
        .await;
}

/// Writes a prestate file holding the contract by hash and returns its path.
fn lazy_prestate(dir: &std::path::Path, code_hash: &str) -> String {
    let path = dir.join("prestate.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"{CONTRACT}":{{"balance":"0x64","nonce":"0x7","code":null,"codeHash":"{code_hash}","storage":{{}}}},"{TOUCHER}":{{"balance":"0x0","nonce":"0x1","code":"{TOUCH_CONTRACT}","storage":{{}}}}}}"#
        ),
    )
    .unwrap();
    path.to_str().unwrap().to_string()
}

async fn mega_evme(args: Vec<String>) -> Output {
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_mega-evme")).args(&args).output().unwrap()
    })
    .await
    .unwrap()
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(ToString::to_string).collect()
}

fn fork_args(uri: &str) -> Vec<String> {
    strings(&["--fork", "--fork.block", "100", "--rpc", uri, "--rpc.no-cache-file"])
}

fn dumped_code(stdout: &[u8], address: &str) -> Value {
    let json: Value = serde_json::from_slice(stdout).expect("json output");
    json["state"][address]["code"].clone()
}

/// Offline, a prestate account whose code is not in the file is refused, as a hash that does not
/// match the code was: it would otherwise run as empty code under a non-empty `EXTCODEHASH`.
#[tokio::test]
async fn test_an_unloadable_account_is_refused_offline() {
    let dir = tempdir().unwrap();
    let prestate = lazy_prestate(dir.path(), CONTRACT_HASH);
    let mut args = strings(&["run", "--spec", "Satin", CALL_CONTRACT, "--prestate"]);
    args.push(prestate);
    let output = mega_evme(args).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains(CONTRACT) && stderr.contains("`code: null`"),
        "{stderr}"
    );
}

/// Forked, the code of such an account is fetched, checked against its hash and run.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_unloaded_account_runs_its_fetched_code() {
    let server = MockServer::start().await;
    mount_fork(&server).await;
    let dir = tempdir().unwrap();
    let mut args = strings(&["run", "--spec", "Satin", CALL_CONTRACT, "--json", "--prestate"]);
    args.push(lazy_prestate(dir.path(), CONTRACT_HASH));
    args.extend(fork_args(&server.uri()));
    let output = mega_evme(args).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["output"], format!("0x{:064x}", 1), "{json}");
}

/// Forked, code that does not match the hash is an error, never a panic or an empty run.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_unloaded_account_with_another_hash_is_refused() {
    let server = MockServer::start().await;
    mount_fork(&server).await;
    let dir = tempdir().unwrap();
    let mut args = strings(&["run", "--spec", "Satin", CALL_CONTRACT, "--prestate"]);
    args.push(lazy_prestate(dir.path(), &format!("0x{}", "ab".repeat(32))));
    args.extend(fork_args(&server.uri()));
    let output = mega_evme(args).await;
    assert_eq!(output.status.code(), Some(1), "no panic");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.to_lowercase().contains(CONTRACT), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// `run --dump` carries the code of an account the prestate named by hash.
#[tokio::test(flavor = "multi_thread")]
async fn test_run_dump_carries_the_code_of_an_unloaded_account() {
    let server = MockServer::start().await;
    mount_fork(&server).await;
    let dir = tempdir().unwrap();
    let mut args =
        strings(&["run", "--spec", "Satin", TOUCH_CONTRACT, "--json", "--dump", "--prestate"]);
    args.push(lazy_prestate(dir.path(), CONTRACT_HASH));
    args.extend(fork_args(&server.uri()));
    let output = mega_evme(args).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(dumped_code(&output.stdout, CONTRACT), CONTRACT_CODE);
}

/// `tx --dump` carries the code of an account the prestate named by hash.
#[tokio::test(flavor = "multi_thread")]
async fn test_tx_dump_carries_the_code_of_an_unloaded_account() {
    let server = MockServer::start().await;
    mount_fork(&server).await;
    let dir = tempdir().unwrap();
    let mut args = strings(&[
        "tx",
        "--spec",
        "Satin",
        "--receiver",
        TOUCHER,
        "--sender.balance",
        "1ether",
        "--json",
        "--dump",
        "--prestate",
    ]);
    args.push(lazy_prestate(dir.path(), CONTRACT_HASH));
    args.extend(fork_args(&server.uri()));
    let output = mega_evme(args).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(dumped_code(&output.stdout, CONTRACT), CONTRACT_CODE);
}
