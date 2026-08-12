//! Integration tests for `mega-evme replay --override.spec`.
//!
//! The override is a coherent what-if: the whole execution world switches to the
//! forced spec, as if the block had run on a chain at that spec. These tests pin
//! the consequences that are visible from outside the process — the pre-block
//! predeploys (their presence and their version) and the block-level resource
//! limits follow the override rather than the block's position in the chain's
//! schedule, and a fork whose parameters the chain never published cannot be
//! forced at all.
//!
//! They run against a mock JSON-RPC endpoint rather than a recorded capture: a
//! higher-spec override reads state the historical replay never touched (the
//! `SequencerRegistry` account, for one), and an offline capture answers a miss
//! with a hard error rather than the state the forced world needs.

use std::process::Command;

use serde_json::{json, Value};

mod common;
use common::MockRpcServer;

/// `MegaETH` mainnet: the chain whose published schedule carries the Rex5
/// `SequencerRegistry` parameters that a Rex5+ override needs.
const CHAIN_ID: u64 = 4326;

/// The replayed block. Its parent is `BLOCK_NUMBER - 1`.
const BLOCK_NUMBER: u64 = 18_172_461;

/// A mainnet timestamp inside the `MiniRex` window, well before Rex4 (the first
/// fork that deploys `MegaLimitControl`) and Rex5 (the first that deploys the
/// `SequencerRegistry`). Forcing a newer spec on this block is what makes the
/// two worlds — historical and forced — visibly different.
const MINI_REX_TIMESTAMP: u64 = 1_764_000_000;

/// Hash of the replayed transaction, and the only transaction in the block.
const TX_HASH: &str = "0x41d34e7e13dfe0f85da9d407e2b2c381955d8c7eed428b17dc82327b2616b000";

/// Hash of the replayed block.
const BLOCK_HASH: &str = "0x2801837c261826beb8047e46139dfc4eb93ab5b3196ce23f312d3c7658262a62";

/// Hash of the parent block, which the replay forks its state from.
const PARENT_HASH: &str = "0xd482d481e9d11dd116ef6c41bf95ca608f159206c8f07900b1b53936d196ccb3";

/// Hash of the grandparent, so the parent block is a well-formed header.
const GRANDPARENT_HASH: &str = "0x152b00e0c659a9ea0827f7d3b7666951c100bb6a6761a90e20ed7f79099a82e1";

/// Sender of the replayed transaction. Funded by the mock's blanket balance.
const SENDER: &str = "0x14112799a39f2905b901067d3cd4a1f63c1cebda";

/// `SequencerRegistry`, deployed pre-block from Rex5 on.
const SEQUENCER_REGISTRY: &str = "0x6342000000000000000000000000000000000006";

/// `MegaLimitControl`, deployed pre-block from Rex4 on.
const LIMIT_CONTROL: &str = "0x6342000000000000000000000000000000000005";

/// `version()` — declared by `ISemver`, implemented by the registry bytecode,
/// and answering with the deployed version string.
const VERSION_SELECTOR: &str = "0x54fd4d50";

/// ABI-encoded `"1.0.0"`: the version the pre-Rex6 `SequencerRegistry` reports.
const VERSION_1_0_0: &str = concat!(
    "0x",
    "0000000000000000000000000000000000000000000000000000000000000020",
    "0000000000000000000000000000000000000000000000000000000000000005",
    "312e302e30000000000000000000000000000000000000000000000000000000",
);

/// ABI-encoded `"2.0.0"`: the version the Rex6 `SequencerRegistry` reports.
const VERSION_2_0_0: &str = concat!(
    "0x",
    "0000000000000000000000000000000000000000000000000000000000000020",
    "0000000000000000000000000000000000000000000000000000000000000005",
    "322e302e30000000000000000000000000000000000000000000000000000000",
);

/// A chain id with no published schedule. `mega-evm` answers those with an
/// all-activated schedule that carries every registry parameter type, so it is
/// the counterpart to mainnet for the parameter-availability cases below.
const UNKNOWN_CHAIN_ID: u64 = 0xdead_beef;

/// Outcome of one `mega-evme replay` invocation.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The single `--json` summary the run printed.
    fn summary(&self) -> Value {
        let mut values = common::json_values(&self.stdout);
        if values.last().is_some_and(common::is_run_error) {
            values.pop();
        }
        assert_eq!(
            values.len(),
            1,
            "expected one summary on stdout:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr,
        );
        values.pop().expect("checked above")
    }

    /// The hex return data of a successful run, or `None` when the call
    /// returned nothing (an account with no code answers empty).
    fn output(&self) -> Option<String> {
        let summary = self.summary();
        assert_eq!(
            summary["success"],
            json!(true),
            "the replay must succeed:\n{}\nstderr:\n{}",
            self.stdout,
            self.stderr,
        );
        summary["output"].as_str().map(str::to_string)
    }
}

/// A block header the RPC backend and the replay accept, carrying only the
/// fields either of them reads.
fn block_json(number: u64, hash: &str, parent_hash: &str, timestamp: u64, txs: Vec<&str>) -> Value {
    json!({
        "hash": hash,
        "parentHash": parent_hash,
        "number": format!("0x{number:x}"),
        "timestamp": format!("0x{timestamp:x}"),
        "gasLimit": "0x2540be400",
        "gasUsed": "0x0",
        "baseFeePerGas": "0xf4240",
        "blobGasUsed": "0x0",
        "excessBlobGas": "0x0",
        "difficulty": "0x0",
        "extraData": "0x00000000fa00000001",
        "logsBloom": format!("0x{}", "0".repeat(512)),
        "miner": "0x4200000000000000000000000000000000000011",
        "mixHash": "0x5cd8791a477b467456670744425e11d5bd91fd54575d6d3bf80d761ab39d957f",
        "nonce": "0x0000000000000000",
        "parentBeaconBlockRoot":
            "0x67123956bf748ccfcfa68f03531dd12c1c647f9f31cc91935ce4271fa7399e24",
        "receiptsRoot": "0x16fe124682128dd43a5da7f2cee0a3bf076deaf12682d19c656914bbea4615e3",
        "requestsHash": "0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
        "size": "0x43e7",
        "stateRoot": "0xa342aba318978654abcf7f09f9494ed271e2136040b628edacb6d384e9074416",
        "transactionsRoot": "0x2f3c5d0b0c4c8d34dd4e1c8bb4b4a4b6d6a2a3d3b8f6a9a2c1d0e9f8a7b6c5d4",
        "withdrawalsRoot": "0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
        "uncles": [],
        "withdrawals": [],
        "transactions": txs,
    })
}

/// The replayed transaction: an EIP-1559 call to `to` with `input` as calldata.
fn tx_json(chain_id: u64, to: &str, input: &str) -> Value {
    json!({
        "type": "0x2",
        "chainId": format!("0x{chain_id:x}"),
        "nonce": "0x0",
        "gas": "0x249f0",
        "maxFeePerGas": "0x200b20",
        "maxPriorityFeePerGas": "0x186a0",
        "gasPrice": "0x10c8e0",
        "to": to,
        "value": "0x0",
        "accessList": [],
        "input": input,
        "r": "0xa19f0f1f52e2951452711b4f4aa5d177442c9a56abeb609b803fe2412ed24946",
        "s": "0x7af21777b2e7d91c745d0077ba2726ee1bb75ccf00039a6218d64fdced768491",
        "yParity": "0x0",
        "v": "0x0",
        "hash": TX_HASH,
        "from": SENDER,
        "blockHash": BLOCK_HASH,
        "blockNumber": format!("0x{BLOCK_NUMBER:x}"),
        "transactionIndex": "0x0",
    })
}

/// A mock endpoint serving a one-transaction mainnet block at `timestamp`,
/// whose transaction calls `to` with `input`.
///
/// Account reads are answered blanket: every account holds 1 ETH, has nonce 0,
/// no code, and zero storage. That leaves the pre-block deploys as the only
/// source of code on the forked state, which is what makes "did this spec's
/// predeploys land" observable from the transaction's own return data.
async fn mock_chain(to: &str, input: &str, timestamp: u64) -> MockRpcServer {
    mock_chain_with_id(CHAIN_ID, to, input, timestamp).await
}

/// [`mock_chain`], on the chain id of the caller's choosing.
async fn mock_chain_with_id(chain_id: u64, to: &str, input: &str, timestamp: u64) -> MockRpcServer {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(chain_id, 1).await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{BLOCK_NUMBER:x}"), false]),
            block_json(BLOCK_NUMBER, BLOCK_HASH, PARENT_HASH, timestamp, vec![TX_HASH]),
            2,
        )
        .await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{:x}", BLOCK_NUMBER - 1), false]),
            block_json(BLOCK_NUMBER - 1, PARENT_HASH, GRANDPARENT_HASH, timestamp - 1, vec![]),
            2,
        )
        .await;
    server.respond_method_json("eth_getTransactionByHash", tx_json(chain_id, to, input), 3).await;
    server.respond_method_result("eth_getBalance", "0xde0b6b3a7640000", 4).await;
    server.respond_method_result("eth_getTransactionCount", "0x0", 4).await;
    server.respond_method_result("eth_getCode", "0x", 4).await;
    server
        .respond_method_result(
            "eth_getStorageAt",
            "0x0000000000000000000000000000000000000000000000000000000000000000",
            4,
        )
        .await;
    server
}

/// Replay the mock's transaction, optionally with extra flags.
fn replay(server: &MockRpcServer, args: &[&str]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_mega-evme"))
        .args(["replay", TX_HASH, "--rpc", &server.uri()])
        .args(["--rpc.no-cache-file", "--rpc.max-retries", "0", "--rpc.backoff-ms", "1", "--json"])
        .args(args)
        .output()
        .expect("failed to run mega-evme");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is utf-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is utf-8"),
    }
}

/// Without an override the block executes where it sits in the chain's
/// schedule: on a MiniRex-era block the Rex5 `SequencerRegistry` was never
/// deployed, so the call reaches an account with no code.
#[tokio::test(flavor = "multi_thread")]
async fn test_without_override_predeploys_follow_the_block_timestamp() {
    let server = mock_chain(SEQUENCER_REGISTRY, VERSION_SELECTOR, MINI_REX_TIMESTAMP).await;
    let run = replay(&server, &[]);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(run.output(), None, "a MiniRex-era block must not carry the Rex5 SequencerRegistry",);
}

/// Forcing Rex5 on a MiniRex-era block installs the Rex5 predeploys, including
/// the `SequencerRegistry` — whose pre-block deploy needs the chain's Rex5
/// parameters. A synthesized schedule that dropped them would fail the run
/// before any transaction executed.
#[tokio::test(flavor = "multi_thread")]
async fn test_override_installs_the_forced_spec_predeploys() {
    let server = mock_chain(SEQUENCER_REGISTRY, VERSION_SELECTOR, MINI_REX_TIMESTAMP).await;
    let run = replay(&server, &["--override.spec", "Rex5"]);

    assert!(
        !run.stderr.contains("SequencerRegistryConfig not configured"),
        "the forced world must keep the chain's Rex5 registry parameters:\n{}",
        run.stderr,
    );
    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(
        run.output().as_deref(),
        Some(VERSION_1_0_0),
        "the forced spec's registry version must answer the call",
    );
}

/// Forcing an older spec withholds predeploys the block did have:
/// `MegaLimitControl` arrives with Rex4, and neither its bytecode nor its
/// interception is present in a forced `MiniRex` world.
#[tokio::test(flavor = "multi_thread")]
async fn test_override_to_an_older_spec_withholds_later_predeploys() {
    let selector = remaining_compute_gas_selector();
    let server = mock_chain(LIMIT_CONTROL, &selector, MINI_REX_TIMESTAMP).await;
    let run = replay(&server, &["--override.spec", "MiniRex"]);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    assert_eq!(run.output(), None, "MegaLimitControl must not answer in a forced MiniRex world");
}

/// The block-level resource limits follow the override too. `MegaLimitControl`
/// reports the compute gas left in the current call, which is the forced spec's
/// per-transaction compute budget minus what the transaction has spent — the
/// `MiniRex` budget the block's own schedule carries is five times larger, so the
/// two are never confusable.
#[tokio::test(flavor = "multi_thread")]
async fn test_override_switches_the_block_limits() {
    let selector = remaining_compute_gas_selector();
    let server = mock_chain(LIMIT_CONTROL, &selector, MINI_REX_TIMESTAMP).await;
    let run = replay(&server, &["--override.spec", "Rex5"]);

    assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
    let output = run.output().expect("MegaLimitControl must answer under the forced Rex5 world");
    let remaining = decode_remaining_compute_gas(&output);

    let forced_budget = compute_gas_budget(mega_evm::MegaSpecId::REX5);
    let historical_budget = compute_gas_budget(mega_evm::MegaSpecId::MINI_REX);
    assert!(
        historical_budget > forced_budget,
        "the scenario needs the two budgets to differ to tell the worlds apart",
    );
    assert!(
        remaining <= forced_budget && remaining > forced_budget - 1_000_000,
        "remaining compute gas {remaining} must come from the forced Rex5 budget \
         {forced_budget}, not the block's MiniRex budget {historical_budget}",
    );
}

/// An override below the chain's own spec moves the predeploys back to that
/// spec's versions, not just their presence: on a chain running Rex6 the
/// registry is v2.0.0, and forcing Rex5 deploys v1.0.0 instead.
#[tokio::test(flavor = "multi_thread")]
async fn test_override_downgrade_switches_the_predeploy_version() {
    let server = mock_chain_with_id(
        UNKNOWN_CHAIN_ID,
        SEQUENCER_REGISTRY,
        VERSION_SELECTOR,
        MINI_REX_TIMESTAMP,
    )
    .await;

    let historical = replay(&server, &[]);
    assert_eq!(
        historical.output().as_deref(),
        Some(VERSION_2_0_0),
        "a chain with no published schedule runs the latest spec",
    );

    let forced = replay(&server, &["--override.spec", "Rex5"]);
    assert_eq!(forced.code, Some(0), "stdout:\n{}\nstderr:\n{}", forced.stdout, forced.stderr);
    assert_eq!(
        forced.output().as_deref(),
        Some(VERSION_1_0_0),
        "the forced Rex5 world must deploy the Rex5 registry version",
    );
}

/// A forced spec needs the chain's parameters for every fork it activates. Where
/// the chain's schedule carries them the what-if runs; where it does not, the
/// pre-block deploy fails closed with the missing-parameter error rather than
/// inventing a value the chain never published. Mainnet carries the Rex5
/// parameters but not the Rex6 ones, so which arm applies is read from the
/// chain configuration instead of being assumed here.
#[tokio::test(flavor = "multi_thread")]
async fn test_override_needs_the_chain_params_of_every_fork_it_activates() {
    use mega_evm::{MegaHardforks, SequencerRegistryRex6Config};

    let configured = mega_evm::hardfork_schedule(CHAIN_ID)
        .fork_params::<SequencerRegistryRex6Config>()
        .is_some();

    let server = mock_chain(SEQUENCER_REGISTRY, VERSION_SELECTOR, MINI_REX_TIMESTAMP).await;
    let run = replay(&server, &["--override.spec", "Rex6"]);

    if configured {
        assert_eq!(run.code, Some(0), "stdout:\n{}\nstderr:\n{}", run.stdout, run.stderr);
        assert_eq!(run.output().as_deref(), Some(VERSION_2_0_0));
    } else {
        assert_eq!(
            run.code,
            Some(1),
            "a fork whose parameters the chain does not carry must not be forced silently:\n{}",
            run.stdout,
        );
        assert!(
            run.stderr.contains("SequencerRegistryRex6Config not configured"),
            "the refusal must name the missing parameters:\n{}",
            run.stderr,
        );
    }
}

/// A spec's per-transaction compute gas budget, which the block limits carry
/// into execution.
fn compute_gas_budget(spec: mega_evm::MegaSpecId) -> u64 {
    mega_evm::EvmTxRuntimeLimits::from_spec(spec).tx_compute_gas_limit
}

/// Calldata for `IMegaLimitControl.remainingComputeGas()`.
fn remaining_compute_gas_selector() -> String {
    use mega_evm::{alloy_sol_types::SolCall, IMegaLimitControl};

    format!(
        "0x{}",
        alloy_primitives::hex::encode(IMegaLimitControl::remainingComputeGasCall {}.abi_encode())
    )
}

/// Decode the `uint64` `MegaLimitControl` returns.
fn decode_remaining_compute_gas(output: &str) -> u64 {
    use mega_evm::{alloy_sol_types::SolCall, IMegaLimitControl};

    let bytes = alloy_primitives::hex::decode(output).expect("output is hex");
    IMegaLimitControl::remainingComputeGasCall::abi_decode_returns(&bytes)
        .expect("output decodes as uint64")
}
