//! A small chain served by a [`MockRpcServer`]: one signed EIP-1559 call, the
//! block it is mined in and that block's parent, its on-chain receipt, and
//! blanket account reads.
//!
//! The replay authenticates everything it fetches — a transaction's hash is
//! recomputed from its encoding and its sender re-derived from its signature,
//! and a block's hash from its header — so nothing here is an invented
//! constant: [`tx_identity`] computes the transaction's authentic pair, and the
//! chain is built from the bottom up, each block sealed under the hash its own
//! header produces.

use serde_json::{json, Value};

use super::MockRpcServer;

/// `MegaETH` mainnet, whose published schedule the replayed block runs under.
pub(crate) const CHAIN_ID: u64 = 4326;

/// Height of the block the mined transaction is in, and the height the
/// endpoint reports as `latest`.
pub(crate) const BLOCK: u64 = 18_172_461;

/// A mainnet timestamp inside the `MiniRex` window.
pub(crate) const TIMESTAMP: u64 = 1_764_000_000;

/// `parentHash` of the parent block, so its header is well formed too.
pub(crate) const GRANDPARENT_HASH: &str =
    "0x4444444444444444444444444444444444444444444444444444444444444444";

/// Signature of the transaction: a fixed, well-formed secp256k1 pair. The
/// sender is whatever address it recovers to, funded like every other account
/// by the blanket balance.
pub(crate) const SIG_R: &str = "0xa19f0f1f52e2951452711b4f4aa5d177442c9a56abeb609b803fe2412ed24946";
pub(crate) const SIG_S: &str = "0x7af21777b2e7d91c745d0077ba2726ee1bb75ccf00039a6218d64fdced768491";

/// Recipient of the transaction: an account with no code, so the call succeeds
/// without depending on any contract the mock does not serve.
pub(crate) const RECIPIENT: &str = "0x681e908b8ab57c49c74d770f369754ccc3e1ae09";

/// Gas the plain call uses, and therefore what the receipt reports so a
/// verification or a fidelity gate matches.
pub(crate) const GAS_USED: u64 = 21_000;

/// The transaction as a consensus object: the one the replay deserializes from
/// [`tx_json`].
pub(crate) fn tx_envelope() -> mega_evm::op_alloy_consensus::OpTxEnvelope {
    use mega_evm::{
        alloy_consensus::{SignableTransaction, TxEip1559},
        op_alloy_consensus::OpTxEnvelope,
    };

    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce: 0,
        gas_limit: 0x249f0,
        max_fee_per_gas: 0x200b20,
        max_priority_fee_per_gas: 0x186a0,
        to: alloy_primitives::TxKind::Call(RECIPIENT.parse().expect("`to` is an address")),
        value: alloy_primitives::U256::ZERO,
        access_list: Default::default(),
        input: alloy_primitives::Bytes::new(),
    };
    let signature = alloy_primitives::Signature::new(
        SIG_R.parse().expect("r is a hex word"),
        SIG_S.parse().expect("s is a hex word"),
        false,
    );
    OpTxEnvelope::Eip1559(tx.into_signed(signature))
}

/// The authentic identity of the transaction: `(hash, from)`.
///
/// Hashes the encoding of [`tx_envelope`] and recovers its signer — the two
/// values the replay authenticates the served answer against.
pub(crate) fn tx_identity() -> (String, String) {
    use mega_evm::alloy_consensus::transaction::SignerRecoverable;

    let envelope = tx_envelope();
    let hash = format!("{:#x}", envelope.tx_hash());
    let from = envelope.recover_signer().expect("signature recovers");
    (hash, format!("{from:#x}"))
}

/// A block header the RPC backend and the replay accept, sealed under the hash
/// its own consensus fields produce.
pub(crate) fn block_json(number: u64, parent_hash: &str, transactions: Value) -> Value {
    super::sealed_block(json!({
        "parentHash": parent_hash,
        "number": format!("0x{number:x}"),
        "timestamp": format!("0x{TIMESTAMP:x}"),
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
        "transactions": transactions,
    }))
}

/// Hash of the parent of [`BLOCK`], the block a mined replay forks from.
pub(crate) fn parent_hash() -> String {
    super::block_hash_of(&block_json(BLOCK - 1, GRANDPARENT_HASH, json!([])))
}

/// Hash of [`BLOCK`], whose header names [`parent_hash`] as its parent.
///
/// The transaction list is not part of the consensus header, so the hash does
/// not depend on which body the mock serves under it.
pub(crate) fn block_hash() -> String {
    super::block_hash_of(&block_json(BLOCK, &parent_hash(), json!([])))
}

/// The transaction, carrying whichever `(blockNumber, blockHash)` pair the
/// endpoint reports for it. A transaction reported with a block number sits at
/// index 0 of its block.
pub(crate) fn tx_json(block_number: Value, block_hash: Value) -> Value {
    let (hash, from) = tx_identity();
    json!({
        "type": "0x2",
        "chainId": format!("0x{CHAIN_ID:x}"),
        "nonce": "0x0",
        "gas": "0x249f0",
        "maxFeePerGas": "0x200b20",
        "maxPriorityFeePerGas": "0x186a0",
        "gasPrice": "0x10c8e0",
        "to": RECIPIENT,
        "value": "0x0",
        "accessList": [],
        "input": "0x",
        "r": SIG_R,
        "s": SIG_S,
        "yParity": "0x0",
        "v": "0x0",
        "hash": hash,
        "from": from,
        "blockHash": block_hash,
        "blockNumber": block_number,
        "transactionIndex": if block_number.is_null() { Value::Null } else { json!("0x0") },
    })
}

/// The transaction, reported as mined in [`BLOCK`].
pub(crate) fn mined_tx_json() -> Value {
    tx_json(json!(format!("0x{BLOCK:x}")), json!(block_hash()))
}

/// The transaction, reported as pending: no block number and no inclusion hash.
pub(crate) fn pending_tx_json() -> Value {
    tx_json(Value::Null, Value::Null)
}

/// The on-chain receipt of the mined transaction, reporting the facts the local
/// replay reproduces.
pub(crate) fn receipt_json() -> Value {
    let (hash, from) = tx_identity();
    json!({
        "type": "0x2",
        "status": "0x1",
        "cumulativeGasUsed": format!("0x{GAS_USED:x}"),
        "logs": [],
        "logsBloom": format!("0x{}", "0".repeat(512)),
        "transactionHash": hash,
        "transactionIndex": "0x0",
        "blockHash": block_hash(),
        "blockNumber": format!("0x{BLOCK:x}"),
        "gasUsed": format!("0x{GAS_USED:x}"),
        "effectiveGasPrice": "0x10c8e0",
        "from": from,
        "to": RECIPIENT,
        "contractAddress": null,
        "l1GasPrice": "0x0",
        "l1GasUsed": "0x0",
        "l1Fee": "0x0",
        "l1BaseFeeScalar": "0x0",
        "l1BlobBaseFee": "0x0",
        "l1BlobBaseFeeScalar": "0x0",
    })
}

/// Answer every account read blanket, at `priority`: every account holds
/// 1 ETH, has nonce 0, no code, and zero storage.
pub(crate) async fn respond_account_reads(server: &MockRpcServer, priority: u8) {
    server.respond_method_result("eth_getBalance", "0xde0b6b3a7640000", priority).await;
    server.respond_method_result("eth_getTransactionCount", "0x0", priority).await;
    server.respond_method_result("eth_getCode", "0x", priority).await;
    server
        .respond_method_result(
            "eth_getStorageAt",
            "0x0000000000000000000000000000000000000000000000000000000000000000",
            priority,
        )
        .await;
}

/// A mock endpoint that resolves the transaction to `tx` and otherwise serves
/// one chain: [`BLOCK`] (also the `latest` height) listing the transaction, its
/// parent, the transaction's receipt, and blanket account reads.
///
/// Its answers never change, so any difference between two runs against it
/// comes from the run and nothing else.
pub(crate) async fn mock_chain_serving(tx: Value) -> MockRpcServer {
    let (tx_hash, _) = tx_identity();
    let server = MockRpcServer::start().await;
    server.respond_eth_chain_id(CHAIN_ID, 1).await;
    server.respond_method_result("eth_blockNumber", &format!("0x{BLOCK:x}"), 2).await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{BLOCK:x}"), false]),
            block_json(BLOCK, &parent_hash(), json!([tx_hash])),
            2,
        )
        .await;
    server
        .respond_method_params_json(
            "eth_getBlockByNumber",
            json!([format!("0x{:x}", BLOCK - 1), false]),
            block_json(BLOCK - 1, GRANDPARENT_HASH, json!([])),
            2,
        )
        .await;
    server.respond_method_json("eth_getTransactionByHash", tx, 3).await;
    server.respond_method_json("eth_getTransactionReceipt", receipt_json(), 3).await;
    respond_account_reads(&server, 4).await;
    server
}
