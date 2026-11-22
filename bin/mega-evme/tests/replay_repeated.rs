//! Whole-block replay of transactions whose consensus bytes appear more than once.

mod common;

use alloy_consensus::{proofs::ordered_trie_root_with_encoder, Sealable};
use mega_evm::{
    alloy_eips::Encodable2718,
    op_alloy_consensus::{OpTxEnvelope, TxDeposit},
};
use op_alloy_rpc_types::OpTransactionReceipt;
use serde_json::{json, Value};

use common::{mock_chain as chain, MockRpcServer, Run};

const BLOCK: u64 = chain::BLOCK;
const DEPOSIT_FROM: &str = "0x1111111111111111111111111111111111111111";

struct RepeatedBlock {
    number: u64,
    block: Value,
    transaction: Value,
    receipts: Vec<Value>,
}

impl RepeatedBlock {
    fn new(number: u64, parent_hash: &str, statuses: &[bool]) -> Self {
        let deposit = TxDeposit {
            from: DEPOSIT_FROM.parse().expect("deposit sender"),
            to: alloy_primitives::TxKind::Call(
                chain::RECIPIENT.parse().expect("deposit recipient"),
            ),
            gas_limit: 100_000,
            ..Default::default()
        };
        let tx = OpTxEnvelope::Deposit(deposit.clone().seal_slow());
        let tx_hash = tx.tx_hash();
        let block_number = format!("0x{number:x}");
        let mut receipts: Vec<_> = statuses
            .iter()
            .enumerate()
            .map(|(index, &success)| {
                let mut receipt = chain::receipt_json();
                receipt["type"] = json!("0x7e");
                receipt["status"] = json!(if success { "0x1" } else { "0x0" });
                receipt["transactionHash"] = json!(tx_hash);
                receipt["transactionIndex"] = json!(format!("0x{index:x}"));
                receipt["blockNumber"] = json!(&block_number);
                receipt["from"] = json!(DEPOSIT_FROM);
                receipt["effectiveGasPrice"] = json!("0x0");
                receipt["depositNonce"] = json!(format!("0x{index:x}"));
                receipt["depositReceiptVersion"] = json!("0x1");
                receipt["cumulativeGasUsed"] = json!(format!("0x{:x}", 21_000 * (index + 1)));
                receipt
            })
            .collect();
        let receipt_envelopes: Vec<_> = receipts
            .iter()
            .map(|receipt| {
                let rpc: OpTransactionReceipt =
                    serde_json::from_value(receipt.clone()).expect("deposit receipt");
                let envelope: mega_evm::op_alloy_consensus::OpReceiptEnvelope<_> =
                    rpc.inner.inner.into();
                envelope.map_logs(|log| log.inner).encoded_2718()
            })
            .collect();
        let transactions = vec![&tx; statuses.len()];
        let mut block =
            chain::block_json(number, parent_hash, json!(vec![tx_hash; statuses.len()]));
        block["transactionsRoot"] =
            json!(ordered_trie_root_with_encoder(&transactions, |tx, out| tx.encode_2718(out)));
        block["receiptsRoot"] =
            json!(ordered_trie_root_with_encoder(&receipt_envelopes, |receipt, out| {
                out.extend_from_slice(receipt);
            }));
        block["gasUsed"] = json!(format!("0x{:x}", 21_000 * statuses.len()));
        let block = common::sealed_block(block);

        for receipt in &mut receipts {
            receipt["blockHash"] = block["hash"].clone();
        }

        let mut transaction = serde_json::to_value(deposit).expect("deposit transaction");
        transaction["type"] = json!("0x7e");
        transaction["hash"] = json!(tx_hash);
        transaction["blockHash"] = block["hash"].clone();
        transaction["blockNumber"] = json!(&block_number);
        transaction["transactionIndex"] = json!(format!("0x{:x}", statuses.len() - 1));
        transaction["gasPrice"] = json!("0x0");

        Self { number, block, transaction, receipts }
    }

    async fn serve(&self, server: &MockRpcServer) {
        let number = format!("0x{:x}", self.number);
        server
            .respond_method_params_json(
                "eth_getBlockByNumber",
                json!([number, false]),
                self.block.clone(),
                2,
            )
            .await;
        server.respond_method_json("eth_getTransactionByHash", self.transaction.clone(), 3).await;
        server
            .respond_method_json(
                "eth_getTransactionReceipt",
                self.receipts.last().expect("by-hash receipt").clone(),
                3,
            )
            .await;
    }

    async fn serve_receipts(&self, server: &MockRpcServer) {
        server
            .respond_method_params_json(
                "eth_getBlockReceipts",
                json!([format!("0x{:x}", self.number)]),
                json!(&self.receipts),
                2,
            )
            .await;
    }
}

async fn mock_chain() -> MockRpcServer {
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(chain::CHAIN_ID, 1).await;
    server.respond_method_result("eth_blockNumber", &format!("0x{:x}", BLOCK + 1), 2).await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{:x}", BLOCK - 1), false]),
            chain::block_json(BLOCK - 1, chain::GRANDPARENT_HASH, json!([])),
            2,
        )
        .await;
    chain::respond_account_reads(&server, 4).await;
    server
}

fn replay(number: u64, rpc_args: &[&str]) -> Run {
    common::mega_evme()
        .args(["replay", "--block", &number.to_string(), "--verify-receipt", "--json"])
        .args(rpc_args)
        .output()
        .expect("run mega-evme")
        .into()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_hash_uses_each_index_and_receipt() {
    let server = mock_chain().await;
    let block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true]);
    block.serve(&server).await;
    block.serve_receipts(&server).await;

    let run = replay(BLOCK, &["--rpc", &server.uri(), "--rpc.no-cache-file"]);

    let lines = run.results();
    assert_eq!(lines.len(), 2, "{}", run.stdout);
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line["tx_index"], index, "{line}");
        assert_eq!(line["tx_hash"], block.transaction["hash"], "{line}");
        assert_eq!(line["verification"], json!({"match": true}), "{line}");
    }
    assert_eq!(run.code(), 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(server.received_method_count("eth_getBlockReceipts").await, 1);
    assert_eq!(server.received_method_count("eth_getTransactionReceipt").await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_receipt_mismatch_is_at_its_own_index() {
    for mismatch in 0..2 {
        let mut statuses = [true, true];
        statuses[mismatch] = false;
        let server = mock_chain().await;
        let block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &statuses);
        block.serve(&server).await;
        block.serve_receipts(&server).await;

        let run = replay(BLOCK, &["--rpc", &server.uri(), "--rpc.no-cache-file"]);

        let lines = run.results();
        assert_eq!(run.code(), 2, "case={mismatch}: {}\n{}", run.stdout, run.stderr);
        assert_eq!(lines.len(), 2, "case={mismatch}");
        assert_eq!(lines[mismatch]["tx_index"], mismatch);
        assert_eq!(lines[mismatch]["verification"]["match"], false);
        assert_eq!(
            lines[mismatch]["verification"]["diff"]["status"],
            json!({"onchain": false, "replay": true}),
            "case={mismatch}"
        );
        assert_eq!(lines[1 - mismatch]["verification"], json!({"match": true}), "case={mismatch}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_receipts_must_rebuild_the_receipts_root() {
    let server = mock_chain().await;
    let mut block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true]);
    block.receipts[0]["status"] = json!("0x0");
    block.serve(&server).await;
    block.serve_receipts(&server).await;

    let run = replay(BLOCK, &["--rpc", &server.uri(), "--rpc.no-cache-file", "--verify-block"]);

    assert_eq!(run.code(), 3, "{}\n{}", run.stdout, run.stderr);
    let (lines, verdict) = common::split_one_block(run.results());
    assert_eq!(lines.len(), 2);
    assert_eq!(verdict["block_verification"], json!({"match": true}));
    for line in lines {
        let error = line["verification"]["error"].as_str().expect("receipt remains unverified");
        assert!(error.contains("receipts the block does not commit to"), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_receipts_require_distinct_rpc_indexes() {
    let server = mock_chain().await;
    let mut block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true]);
    block.receipts[0]["transactionIndex"] = json!("0x1");
    block.serve(&server).await;
    block.serve_receipts(&server).await;

    let run = replay(BLOCK, &["--rpc", &server.uri(), "--rpc.no-cache-file"]);

    assert_eq!(run.code(), 3, "{}\n{}", run.stdout, run.stderr);
    let lines = run.results();
    assert_eq!(lines.len(), 2);
    for line in lines {
        let error = line["verification"]["error"].as_str().expect("receipt remains unverified");
        assert!(error.contains("expected body index 0"), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_hash_across_blocks_uses_each_blocks_receipts() {
    let server = mock_chain().await;
    let first = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true]);
    let first_hash = first.block["hash"].as_str().expect("first block hash");
    let second = RepeatedBlock::new(BLOCK + 1, first_hash, &[true, false]);
    // The by-hash index only serves the later block. Both block bodies still contain the hash.
    second.serve(&server).await;
    first.serve(&server).await;
    first.serve_receipts(&server).await;
    second.serve_receipts(&server).await;
    let dir = tempfile::tempdir().expect("cache directory");
    let args =
        ["--rpc", &server.uri(), "--rpc.cache-dir", dir.path().to_str().expect("cache path")];

    let first_run = replay(BLOCK, &args);
    let second_run = replay(BLOCK + 1, &args);

    assert_eq!(first_run.code(), 0, "{}\n{}", first_run.stdout, first_run.stderr);
    assert_eq!(second_run.code(), 2, "{}\n{}", second_run.stdout, second_run.stderr);
    let first_lines = first_run.results();
    let second_lines = second_run.results();
    assert_eq!((first_lines.len(), second_lines.len()), (2, 2));
    assert_eq!(first_lines[1]["tx_hash"], second_lines[1]["tx_hash"]);
    assert_eq!(first_lines[1]["block_number"], BLOCK);
    assert_eq!(second_lines[1]["block_number"], BLOCK + 1);
    assert_eq!(first_lines[1]["verification"], json!({"match": true}));
    assert_eq!(second_lines[1]["verification"]["match"], false);
    assert_eq!(server.received_method_count("eth_getBlockReceipts").await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_block_receipts_survive_online_cache_and_offline_replay() {
    let server = mock_chain().await;
    let mut block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true]);
    block.receipts.reverse();
    block.serve(&server).await;
    block.serve_receipts(&server).await;
    let dir = tempfile::tempdir().expect("cache directory");
    let uri = server.uri();
    let args = ["--rpc", &uri, "--rpc.cache-dir", dir.path().to_str().expect("cache path")];
    let cache = dir.path().join(format!("rpc-cache-{}.json", chain::CHAIN_ID));

    let live = replay(BLOCK, &args);
    let cached = replay(BLOCK, &args);
    let offline = replay(BLOCK, &["--rpc.replay-file", cache.to_str().expect("envelope path")]);

    assert_eq!(live.code(), 0, "{}\n{}", live.stdout, live.stderr);
    assert_eq!(cached.code(), 0, "{}\n{}", cached.stdout, cached.stderr);
    assert_eq!(offline.code(), 0, "{}\n{}", offline.stdout, offline.stderr);
    assert_eq!(cached.stdout, live.stdout);
    assert_eq!(offline.stdout, live.stdout);
    assert_eq!(offline.results().len(), 2);
    assert_eq!(server.received_method_count("eth_getBlockReceipts").await, 1);
    assert_eq!(server.received_method_count("eth_getTransactionReceipt").await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_hash_requires_block_receipts_when_rpc_method_is_unsupported() {
    let server = mock_chain().await;
    let block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true]);
    block.serve(&server).await;
    server.respond_jsonrpc_error(-32601, "Method not found", 10).await;

    let run = replay(BLOCK, &["--rpc", &server.uri(), "--rpc.no-cache-file"]);

    assert_eq!(run.code(), 3, "{}\n{}", run.stdout, run.stderr);
    let lines = run.results();
    assert_eq!(lines.len(), 2);
    for line in lines {
        let error = line["verification"]["error"].as_str().expect("receipt remains unverified");
        assert!(
            error.contains("eth_getBlockReceipts") && error.contains("Method not found"),
            "{line}"
        );
    }
    assert_eq!(server.received_method_count("eth_getTransactionReceipt").await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_hash_offline_miss_cannot_use_a_by_hash_receipt() {
    use common::doctor::{cache_key, DoctoredEnvelope};

    let server = mock_chain().await;
    let block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true]);
    block.serve(&server).await;
    block.serve_receipts(&server).await;
    let dir = tempfile::tempdir().expect("capture directory");
    let path = dir.path().join("rpc.json");
    let capture =
        ["--rpc", &server.uri(), "--rpc.capture-file", path.to_str().expect("capture path")];
    let captured = replay(BLOCK, &capture);
    assert_eq!(captured.code(), 0, "{}\n{}", captured.stdout, captured.stderr);
    let by_hash: Run = common::mega_evme()
        .args(["replay", "--verify-receipt", "--json"])
        .arg(block.transaction["hash"].as_str().expect("transaction hash"))
        .args(capture)
        .output()
        .expect("capture the by-hash result")
        .into();
    assert_eq!(by_hash.code(), 0, "{}\n{}", by_hash.stdout, by_hash.stderr);
    assert_eq!(by_hash.summary()["receipt"]["transactionIndex"], "0x1");
    let key = cache_key("eth_getBlockReceipts", &format!(r#"["0x{BLOCK:x}"]"#));
    let legacy =
        DoctoredEnvelope::load(&path).drop_entry(&key).write_to_temp("repeated_receipt_miss");

    let run = replay(BLOCK, &["--rpc.replay-file", legacy.to_str().expect("legacy path")]);
    std::fs::remove_file(legacy).expect("remove temporary envelope");

    assert_eq!(run.code(), 3, "{}\n{}", run.stdout, run.stderr);
    let lines = run.results();
    assert_eq!(lines.len(), 2);
    for line in lines {
        let error = line["verification"]["error"].as_str().expect("receipt remains unverified");
        assert!(error.contains("cache miss") && error.contains("eth_getBlockReceipts"), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_without_repeated_hashes_an_unsupported_method_falls_back_by_hash() {
    let server = mock_chain().await;
    let block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true]);
    block.serve(&server).await;
    server.respond_jsonrpc_error(-32601, "Method not found", 10).await;

    let run = replay(BLOCK, &["--rpc", &server.uri(), "--rpc.no-cache-file"]);

    assert_eq!(run.code(), 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.results()[0]["verification"], json!({"match": true}));
    assert_eq!(server.received_method_count("eth_getBlockReceipts").await, 1);
    assert_eq!(server.received_method_count("eth_getTransactionReceipt").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_hash_abort_is_attributed_to_the_exact_occurrence() {
    let server = mock_chain().await;
    let mut block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true, true]);
    // The first deposit fits. Once it used 21,000 gas, the next cannot start.
    block.block["gasLimit"] = json!("0x186a0");
    block.block = common::sealed_block(block.block);
    block.serve(&server).await;

    let run: Run = common::mega_evme()
        .args(["replay", "--block", &BLOCK.to_string(), "--json"])
        .args(["--rpc", &server.uri(), "--rpc.no-cache-file"])
        .output()
        .expect("run mega-evme")
        .into();

    assert_eq!(run.code(), 1, "{}\n{}", run.stdout, run.stderr);
    let lines = run.results();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["tx_index"], 0);
    assert_eq!(lines[0]["success"], true);
    assert_eq!(lines[1]["tx_index"], 1);
    assert_eq!(lines[1]["error"]["kind"], "execution");
    assert_eq!(lines[2]["tx_index"], 2);
    assert_eq!(lines[2]["error"]["kind"], "rpc");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_hash_in_tx_file_uses_the_occurrence_returned_by_hash() {
    let server = mock_chain().await;
    let block = RepeatedBlock::new(BLOCK, &chain::parent_hash(), &[true, true]);
    block.serve(&server).await;
    let dir = tempfile::tempdir().expect("target list directory");
    let list = dir.path().join("targets.txt");
    let tx_hash = block.transaction["hash"].as_str().expect("transaction hash");
    std::fs::write(&list, format!("{tx_hash}\n{tx_hash}\n")).expect("write the two requests");

    let run: Run = common::mega_evme()
        .args(["replay", "--tx-file"])
        .arg(list)
        .args(["--verify-receipt", "--json", "--rpc", &server.uri(), "--rpc.no-cache-file"])
        .output()
        .expect("run mega-evme")
        .into();

    assert_eq!(run.code(), 0, "{}\n{}", run.stdout, run.stderr);
    let lines = run.results();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["tx_index"], 1);
    assert_eq!(lines[0]["verification"], json!({"match": true}));
    assert_eq!(server.received_method_count("eth_getBlockReceipts").await, 0);
    assert_eq!(server.received_method_count("eth_getTransactionReceipt").await, 1);
}
