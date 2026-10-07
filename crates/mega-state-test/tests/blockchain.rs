//! The blockchain-test runner on chains written here, whose expectations come from revm's own
//! mainnet EVM on Osaka — Ethereum, and not `MegaEvm` — so a chain that passes passes because
//! Satin's block executor agrees with Ethereum on it, and one that fails is judged the way the
//! gate judges it.
//!
//! A chain is filled block by block: the EIP-2935 and EIP-4788 pre-block system calls, then every
//! transaction, on revm's mainnet EVM; the header carries the gas used, logs bloom, receipts root
//! and state root that produced. Each transaction is signed with one fixed signature, which
//! recovers to a different sender for each transaction, and the pre-state funds every sender.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    eip7685::EMPTY_REQUESTS_HASH,
};
use mega_evm::{
    alloy_consensus::{
        proofs::{calculate_receipt_root, calculate_transaction_root},
        transaction::SignerRecoverable,
        Block, BlockBody, Header, Receipt, ReceiptEnvelope, Signed, TxEnvelope, TxLegacy,
        EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH,
    },
    alloy_primitives::{Bloom, Signature},
    op_revm::constants::BASE_FEE_RECIPIENT,
    revm::{
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, TxEnv},
        database::{CacheState, State},
        handler::{MainBuilder, MainContext},
        primitives::{address, b256, keccak256, Address, Bytes, TxKind, B256, U256},
        state::AccountInfo,
        Context, ExecuteCommitEvm, SystemCallCommitEvm,
    },
};
use serde_json::{json, Value};
use state_test::{
    blockchain::{run, Config, FailureKind, Outcome, Produced, Report, SkipReason},
    deviations::{BlockchainEntry, Deviation, DEVIATIONS},
    roots::state_root,
    Fork,
};

/// The contract the transactions call: `SSTORE(0, 42)`, then `LOG0` of the word 7.
const STORE_AND_LOG: &str = "0x602a5f5560075f5260205fa000";

/// A contract that stores the hash of block 1 at slot 0.
const STORE_BLOCKHASH_1: &str = "0x6001405f5500";

const CONTRACT: Address = address!("0x00000000000000000000000000000000000c0de0");
const BLOCKHASH_READER: Address = address!("0x00000000000000000000000000000000000c0de1");
const COINBASE: Address = address!("0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba");
const BASE_FEE: u64 = 7;
const GAS_LIMIT: u64 = 0x0727_0e00;
const CHAIN_ID: u64 = 1;
/// Prague's and Osaka's blob base fee update fraction.
const BLOB_FRACTION: u64 = 5_007_716;

/// A transaction to be filled into a block.
#[derive(Clone)]
struct Tx {
    nonce: u64,
    to: Address,
    value: U256,
    gas_limit: u64,
}

impl Tx {
    fn call(to: Address) -> Self {
        Self { nonce: 0, to, value: U256::ZERO, gas_limit: 100_000 }
    }

    /// The transaction, signed with the fixed signature, and the sender it recovers to.
    fn signed(&self) -> (TxEnvelope, Address) {
        let tx = TxLegacy {
            chain_id: Some(CHAIN_ID),
            nonce: self.nonce,
            gas_price: 10,
            gas_limit: self.gas_limit,
            to: TxKind::Call(self.to),
            value: self.value,
            input: Bytes::new(),
        };
        let signature = Signature::new(
            U256::from_be_bytes(
                b256!("0x23f5c4da0aaeea7f2b85535873ef6c0665d118aa4db9eaa01fdde520abeda0da").0,
            ),
            U256::from_be_bytes(
                b256!("0x42f9035fed519beffb0d2ac54f577ea12c24a4f5857d1fa51c786061ee30387a").0,
            ),
            false,
        );
        let signed = TxEnvelope::Legacy(Signed::new_unhashed(tx, signature));
        let sender = signed.recover_signer().expect("the fixed signature recovers a sender");
        (signed, sender)
    }
}

/// A block to be filled: its transactions, and the exception it is expected to raise, which
/// leaves it unfilled.
struct BlockSpec {
    txs: Vec<Tx>,
    expect_exception: Option<&'static str>,
}

impl BlockSpec {
    fn valid(txs: Vec<Tx>) -> Self {
        Self { txs, expect_exception: None }
    }

    fn invalid(txs: Vec<Tx>, exception: &'static str) -> Self {
        Self { txs, expect_exception: Some(exception) }
    }
}

fn account(balance: U256, code: &str, nonce: u64) -> Value {
    json!({ "balance": balance, "code": code, "nonce": U256::from(nonce), "storage": {} })
}

/// The pre-state: the two pre-block system contracts, the two contracts the transactions call,
/// every sender the blocks' transactions recover to, funded, and `extra`.
fn pre_state(blocks: &[BlockSpec], extra: &[(Address, Value)]) -> BTreeMap<Address, Value> {
    let ether = U256::from(10).pow(U256::from(21));
    let mut pre = BTreeMap::from([
        (HISTORY_STORAGE_ADDRESS, account(U256::ZERO, &HISTORY_STORAGE_CODE.to_string(), 1)),
        (BEACON_ROOTS_ADDRESS, account(U256::ZERO, &BEACON_ROOTS_CODE.to_string(), 1)),
        (CONTRACT, account(U256::ZERO, STORE_AND_LOG, 1)),
        (BLOCKHASH_READER, account(U256::ZERO, STORE_BLOCKHASH_1, 1)),
    ]);
    for tx in blocks.iter().flat_map(|block| &block.txs) {
        pre.insert(tx.signed().1, account(ether, "0x", 0));
    }
    pre.extend(extra.iter().cloned());
    pre
}

/// The state `pre` describes, as revm's runner loads it.
fn cache(pre: &BTreeMap<Address, Value>) -> CacheState {
    let mut cache = CacheState::new();
    for (address, value) in pre {
        let code: Bytes = serde_json::from_value(value["code"].clone()).unwrap();
        let code_hash = keccak256(&code);
        if !code.is_empty() {
            cache.contracts.insert(code_hash, Bytecode::new_raw(code));
        }
        let info = AccountInfo {
            balance: serde_json::from_value(value["balance"].clone()).unwrap(),
            nonce: serde_json::from_value::<U256>(value["nonce"].clone()).unwrap().to(),
            code_hash,
            code: None,
            ..Default::default()
        };
        cache.insert_account_with_storage(*address, info, Default::default());
    }
    cache
}

fn header_of(parent: &Header, number: u64) -> Header {
    Header {
        parent_hash: parent.hash_slow(),
        ommers_hash: EMPTY_OMMER_ROOT_HASH,
        beneficiary: COINBASE,
        number,
        gas_limit: GAS_LIMIT,
        timestamp: number * 12,
        base_fee_per_gas: Some(BASE_FEE),
        withdrawals_root: Some(EMPTY_ROOT_HASH),
        blob_gas_used: Some(0),
        excess_blob_gas: Some(0),
        parent_beacon_block_root: Some(B256::repeat_byte(number as u8)),
        requests_hash: Some(EMPTY_REQUESTS_HASH),
        ..Default::default()
    }
}

fn rlp(header: Header, txs: Vec<TxEnvelope>) -> Bytes {
    let block = Block {
        header,
        body: BlockBody {
            transactions: txs,
            ommers: vec![],
            withdrawals: Some(Default::default()),
        },
    };
    alloy_rlp::encode(&block).into()
}

/// A chain filled from revm's mainnet EVM on Osaka: the test's JSON.
fn chain(blocks: &[BlockSpec]) -> Value {
    chain_with(blocks, &[])
}

/// [`chain`], with `extra` accounts in its pre-state.
fn chain_with(blocks: &[BlockSpec], extra: &[(Address, Value)]) -> Value {
    let pre = pre_state(blocks, extra);
    let mut state = State::builder().with_cached_prestate(cache(&pre)).build();
    let genesis = Header {
        state_root: state_root(state.cache.trie_account()),
        ..header_of(&Header::default(), 0)
    };
    let genesis = Header { parent_hash: B256::ZERO, timestamp: 0, ..genesis };
    let mut cfg = CfgEnv::new();
    cfg.set_spec_and_mainnet_gas_params(Fork::Osaka.spec_id());
    cfg.chain_id = CHAIN_ID;

    let mut parent = genesis.clone();
    let mut fixture_blocks = Vec::new();
    for spec in blocks {
        let mut header = header_of(&parent, parent.number + 1);
        let txs: Vec<_> = spec.txs.iter().map(Tx::signed).collect();
        let envelopes: Vec<_> = txs.iter().map(|(tx, _)| tx.clone()).collect();
        header.transactions_root = calculate_transaction_root(&envelopes);
        if let Some(exception) = spec.expect_exception {
            fixture_blocks.push(json!({
                "rlp": rlp(header, envelopes),
                "expectException": exception,
                "rlp_decoded": { "transactions": [], "withdrawals": [] },
            }));
            continue;
        }
        let mut block_env = BlockEnv {
            number: U256::from(header.number),
            beneficiary: header.beneficiary,
            timestamp: U256::from(header.timestamp),
            gas_limit: header.gas_limit,
            basefee: BASE_FEE,
            prevrandao: Some(header.mix_hash),
            ..Default::default()
        };
        block_env.set_blob_excess_gas_and_price(0, BLOB_FRACTION);
        state.block_hashes.insert(parent.number, parent.hash_slow());
        let mut evm = Context::mainnet()
            .with_block(block_env)
            .with_cfg(cfg.clone())
            .with_db(&mut state)
            .build_mainnet();
        evm.system_call_commit(HISTORY_STORAGE_ADDRESS, header.parent_hash.0.into()).unwrap();
        let root = header.parent_beacon_block_root.unwrap();
        evm.system_call_commit(BEACON_ROOTS_ADDRESS, root.0.into()).unwrap();
        let mut receipts = Vec::new();
        let mut cumulative = 0;
        for (tx, sender) in &txs {
            let TxEnvelope::Legacy(signed) = tx else { unreachable!() };
            let legacy = signed.tx();
            let env = TxEnv::builder()
                .tx_type(Some(0))
                .caller(*sender)
                .gas_limit(legacy.gas_limit)
                .gas_price(legacy.gas_price)
                .kind(legacy.to)
                .value(legacy.value)
                .nonce(legacy.nonce)
                .chain_id(legacy.chain_id)
                .build()
                .unwrap();
            let result = evm.transact_commit(env).expect("Ethereum executes the transaction");
            cumulative += result.tx_gas_used();
            let receipt = Receipt {
                status: result.is_success().into(),
                cumulative_gas_used: cumulative,
                logs: result.into_logs(),
            };
            receipts.push(ReceiptEnvelope::Legacy(receipt.with_bloom()));
        }
        drop(evm);
        header.gas_used = cumulative;
        header.receipts_root = calculate_receipt_root(&receipts);
        let mut bloom = Bloom::ZERO;
        for receipt in &receipts {
            bloom.accrue_bloom(receipt.logs_bloom());
        }
        header.logs_bloom = bloom;
        header.state_root = state_root(state.cache.trie_account());
        fixture_blocks.push(json!({
            "rlp": rlp(header.clone(), envelopes),
            "transactions": spec.txs.iter().map(|_| json!({ "type": "0x00" })).collect::<Vec<_>>(),
            "withdrawals": [],
        }));
        parent = header;
    }

    let post: BTreeMap<_, _> = state
        .cache
        .trie_account()
        .into_iter()
        .map(|(address, account)| {
            let code = state.cache.contracts.get(&account.info.code_hash);
            let code = code.map_or_else(Bytes::new, |code| code.original_bytes());
            let storage: BTreeMap<_, _> =
                account.storage.iter().filter(|(_, v)| !v.is_zero()).collect();
            let value = json!({
                "balance": account.info.balance,
                "code": code,
                "nonce": U256::from(account.info.nonce),
                "storage": storage,
            });
            (address, value)
        })
        .collect();
    json!({
        "network": "Osaka",
        "genesisRLP": rlp(genesis, vec![]),
        "blocks": fixture_blocks,
        "pre": pre,
        "postState": post,
        "lastblockhash": parent.hash_slow(),
        "config": {
            "network": "Osaka",
            "chainid": U256::from(CHAIN_ID),
            "blobSchedule": { "Osaka": { "target": "0x06", "max": "0x09",
                "baseFeeUpdateFraction": U256::from(BLOB_FRACTION) } },
        },
    })
}

/// Writes `tests` as one fixture file at `relative` under `dir`.
fn write(dir: &Path, relative: &str, tests: Value) -> PathBuf {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
    std::fs::write(&path, serde_json::to_string_pretty(&tests).expect("json")).expect("a file");
    path
}

fn config(deviations: &'static [Deviation]) -> Config {
    Config { threads: 2, json_outcome: false, deviations }
}

fn run_one(test: Value) -> Report {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "a.json", json!({ "t": test }));
    run(&[path], config(DEVIATIONS))
}

fn outcome(report: &Report) -> &Outcome {
    assert_eq!(report.results.len(), 1, "{:?}", report.file_failures);
    &report.results[0].outcome
}

fn failure_kind(report: &Report) -> Option<FailureKind> {
    match outcome(report) {
        Outcome::Failed(failure) => Some(failure.kind),
        _ => None,
    }
}

/// The block at `index` of `test`, decoded.
fn decoded(test: &Value, index: usize) -> Block<TxEnvelope> {
    let rlp: Bytes = serde_json::from_value(test["blocks"][index]["rlp"].clone()).unwrap();
    alloy_rlp::Decodable::decode(&mut rlp.as_ref()).unwrap()
}

/// Replaces the block at `index` of `test` with one whose header `edit` changed.
fn edit_header(test: &mut Value, index: usize, edit: impl FnOnce(&mut Header)) {
    let mut block = decoded(test, index);
    edit(&mut block.header);
    test["blocks"][index]["rlp"] = json!(rlp(block.header, block.body.transactions));
}

/// Two blocks of calls filled from Ethereum pass on Satin's block executor: the pre-block system
/// calls, the gas and the receipts of several transactions to a block, the base fee Ethereum burns
/// and Satin routes to its vault, the predeploys Satin adds, and a `BLOCKHASH` of the first block
/// read in the second.
#[test]
fn test_a_chain_filled_from_ethereum_passes() {
    let test = chain(&[
        BlockSpec::valid(vec![Tx::call(CONTRACT), Tx { gas_limit: 100_001, ..Tx::call(CONTRACT) }]),
        BlockSpec::valid(vec![
            Tx::call(BLOCKHASH_READER),
            Tx { value: U256::from(5), ..Tx::call(COINBASE) },
        ]),
    ]);
    let report = run_one(test);
    assert_eq!(outcome(&report), &Outcome::Passed, "{report:?}");
    let summary = report.summary();
    assert_eq!((summary.executed, summary.passed, summary.blocks.accepted), (1, 1, 2));
    assert!(report
        .gate(Some(1), Some(&BTreeMap::new()), true)
        .iter()
        .all(|p| !p.contains("executed")));
}

/// A block the fixture expects to be invalid for its nonce is refused for that reason, and the
/// chain stays at the previous block: the fixture's last block and post-state are the first
/// block's.
#[test]
fn test_a_refused_block_leaves_the_chain_at_the_previous_block() {
    let test = chain(&[
        BlockSpec::valid(vec![Tx::call(CONTRACT)]),
        BlockSpec::invalid(
            vec![Tx { nonce: 1, ..Tx::call(CONTRACT) }],
            "TransactionException.NONCE_MISMATCH_TOO_HIGH",
        ),
    ]);
    let report = run_one(test);
    assert_eq!(outcome(&report), &Outcome::Passed, "{report:?}");
    assert_eq!(report.summary().blocks, state_test::blockchain::Blocks { accepted: 1, refused: 1 });
}

/// A refusal for another reason than the one named, and a refusal of a block the fixture expects
/// to be valid, fail.
#[test]
fn test_a_refusal_must_be_the_one_named_and_expected() {
    let blocks = [BlockSpec::invalid(
        vec![Tx { nonce: 1, ..Tx::call(CONTRACT) }],
        "TransactionException.INTRINSIC_GAS_TOO_LOW",
    )];
    assert_eq!(failure_kind(&run_one(chain(&blocks))), Some(FailureKind::WrongException));

    let mut test = chain(&[BlockSpec::invalid(
        vec![Tx { nonce: 1, ..Tx::call(CONTRACT) }],
        "TransactionException.NONCE_MISMATCH_TOO_HIGH",
    )]);
    test["blocks"][0].as_object_mut().unwrap().remove("expectException");
    assert_eq!(failure_kind(&run_one(test)), Some(FailureKind::UnexpectedException));
}

/// A block the executor accepts fails when the fixture expects it to be refused.
#[test]
fn test_an_accepted_block_that_should_be_refused_fails() {
    let mut test = chain(&[BlockSpec::valid(vec![Tx::call(CONTRACT)])]);
    test["blocks"][0]["expectException"] = json!("TransactionException.NONCE_MISMATCH_TOO_HIGH");
    let report = run_one(test);
    assert_eq!(failure_kind(&report), Some(FailureKind::MissingException));
}

/// A change to a block's header.
type HeaderEdit = fn(&mut Header);

/// Each of the four header fields an accepted block is held to fails the test when the header
/// says otherwise, and the failure carries what Satin produced.
#[test]
fn test_every_header_comparison_can_fail() {
    let clean = chain(&[BlockSpec::valid(vec![Tx::call(CONTRACT)])]);
    let header = decoded(&clean, 0).header;
    let cases: [(FailureKind, HeaderEdit); 4] = [
        (FailureKind::GasUsedMismatch, |h| h.gas_used += 1),
        (FailureKind::LogsBloomMismatch, |h| h.logs_bloom = Bloom::ZERO),
        (FailureKind::ReceiptsRootMismatch, |h| h.receipts_root = B256::repeat_byte(1)),
        (FailureKind::StateRootMismatch, |h| h.state_root = B256::repeat_byte(1)),
    ];
    for (kind, edit) in cases {
        let mut test = clean.clone();
        edit_header(&mut test, 0, edit);
        let report = run_one(test);
        let Outcome::Failed(failure) = outcome(&report) else { panic!("{kind:?} passed") };
        assert_eq!(failure.kind, kind);
        assert_eq!(failure.block, Some(0));
        assert_eq!(
            failure.produced,
            Some(Produced {
                block: 0,
                gas_used: header.gas_used,
                receipts_root: header.receipts_root,
                state_root: header.state_root,
            })
        );
    }
}

/// The chain must end where the fixture's does, holding its post-state.
#[test]
fn test_the_chain_must_end_at_the_fixture_s_post_state() {
    let mut test = chain(&[BlockSpec::valid(vec![Tx::call(CONTRACT)])]);
    let mut post: BTreeMap<Address, Value> =
        serde_json::from_value(test["postState"].clone()).unwrap();
    post.get_mut(&CONTRACT).unwrap()["balance"] = json!("0x01");
    test["postState"] = json!(post);
    assert_eq!(failure_kind(&run_one(test)), Some(FailureKind::PostStateMismatch));

    let mut test = chain(&[BlockSpec::valid(vec![Tx::call(CONTRACT)])]);
    test["lastblockhash"] = json!(B256::repeat_byte(9));
    assert_eq!(failure_kind(&run_one(test)), Some(FailureKind::PostStateMismatch));
}

/// An account the removal list names is taken out only when the pre-state does not hold it and it
/// holds exactly what Satin puts there: a base-fee vault the fixture holds is Ethereum's to
/// compare, and Satin's credit to it fails the block; a vault a transaction sent value to holds
/// more than the base fees, and stays, and the credit beside the value fails the block.
#[test]
fn test_only_what_satin_added_is_taken_out() {
    let blocks = [BlockSpec::valid(vec![Tx::call(CONTRACT)])];
    let vault = [(BASE_FEE_RECIPIENT, account(U256::ZERO, "0x", 1))];
    let report = run_one(chain_with(&blocks, &vault));
    assert_eq!(failure_kind(&report), Some(FailureKind::StateRootMismatch));

    let blocks =
        [BlockSpec::valid(vec![Tx { value: U256::from(1), ..Tx::call(BASE_FEE_RECIPIENT) }])];
    let report = run_one(chain(&blocks));
    assert_eq!(failure_kind(&report), Some(FailureKind::StateRootMismatch));

    // The predeploys the executor adds are taken out: without them the root is Ethereum's.
    assert_eq!(outcome(&run_one(chain(&[BlockSpec::valid(vec![])]))), &Outcome::Passed);
}

/// Tests are skipped by their content before anything runs, and only Osaka's run at all.
#[test]
fn test_skips_and_other_networks() {
    let dir = tempfile::tempdir().unwrap();
    let mut withdrawals = chain(&[BlockSpec::valid(vec![Tx::call(CONTRACT)])]);
    withdrawals["blocks"][0]["withdrawals"] = json!([{ "index": "0x00" }]);
    let mut prague = chain(&[BlockSpec::valid(vec![Tx::call(CONTRACT)])]);
    prague["network"] = json!("Prague");
    let path = write(dir.path(), "a.json", json!({ "w": withdrawals, "p": prague }));
    let report = run(&[path], config(DEVIATIONS));
    assert_eq!(report.results.len(), 1, "the Prague test is not part of the run");
    assert_eq!(report.results[0].outcome, Outcome::Skipped { reason: SkipReason::Withdrawals });
    let pins = BTreeMap::from([(SkipReason::Withdrawals, 1)]);
    assert!(report.gate(Some(0), Some(&pins), false).is_empty());
    let problems = report.gate(None, Some(&BTreeMap::new()), false);
    assert_eq!(problems, ["1 tests skipped for withdrawals, 0 pinned"]);
}

/// A failure is a deviation's only when the deviation lists the test with exactly what it
/// produced; a listed test that passes, or fails otherwise, is unreproduced.
#[test]
fn test_attribution_by_exact_outcome() {
    let mut test = chain(&[BlockSpec::valid(vec![Tx::call(CONTRACT)])]);
    let header = decoded(&test, 0).header;
    edit_header(&mut test, 0, |h| h.state_root = B256::repeat_byte(1));
    let produced = Produced {
        block: 0,
        gas_used: header.gas_used,
        receipts_root: header.receipts_root,
        state_root: header.state_root,
    };
    let registry = |produced: Produced| -> &'static [Deviation] {
        let entries = vec![BlockchainEntry { path: "a.json", name: "t", produced }];
        Box::leak(Box::new([Deviation {
            id: "listed",
            rule: "the rule",
            reason: "the reason",
            fork: Fork::Osaka,
            entries: &[],
            blockchain_entries: entries.leak(),
        }]))
    };
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "a.json", json!({ "t": test }));

    let report = run(std::slice::from_ref(&path), config(registry(produced)));
    let summary = report.summary();
    assert_eq!((summary.unattributed, summary.deviated.get("listed").copied()), (0, Some(1)));
    assert!(report.gate(None, None, true).is_empty(), "{:?}", report.gate(None, None, true));

    let other = Produced { gas_used: produced.gas_used + 1, ..produced };
    let report = run(&[path], config(registry(other)));
    let summary = report.summary();
    assert_eq!(summary.unattributed, 1);
    assert_eq!(summary.unreproduced.get("listed").copied(), Some(1));
    assert_eq!(report.gate(None, None, true).len(), 3);
}
