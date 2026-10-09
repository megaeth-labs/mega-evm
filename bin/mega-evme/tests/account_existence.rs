//! Integration tests for the forked backend's account-existence normalization.
//!
//! JSON-RPC cannot express "this account was never created": `eth_getBalance`,
//! `eth_getTransactionCount`, and `eth_getCode` all answer `0`/`0`/empty for
//! it, and the RPC backend would otherwise materialize that answer as an
//! *existing* empty account. `EvmeState` maps the all-zero answer back to
//! `None` (safe post-EIP-161, where existing-but-empty accounts cannot occur).
//!
//! The DB-level tests pin the normalization boundary: only the fully-zero
//! account maps to `None`; any single non-zero dimension keeps it existing.
//! The execution-level tests pin the consumer that made the bug observable:
//! EIP-7702 refunds 12,500 gas per authorization only when the authority
//! already exists in the trie, so a brand-new authority materialized as an
//! existing empty account made every replayed type-4 transaction with fresh
//! authorities under-report `gasUsed` by 12,500 per authorization.

use alloy_eips::{
    eip2930::{AccessList, AccessListItem},
    eip7702::{Authorization, RecoveredAuthority, RecoveredAuthorization},
};
use alloy_primitives::{address, Address, Bytes, B256, U256};
use clap::Parser;
use mega_evm::{
    revm::{
        context::{result::ExecutionResult, tx::TxEnvBuilder},
        state::EvmState,
        DatabaseRef, ExecuteEvm,
    },
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use mega_evme::common::{EvmeExternalEnvs, EvmeState, OpProvider, PreStateArgs};
use op_alloy_network::Optimism;
use rstest::rstest;

mod common;
use common::{test_rpc_args, MockRpcServer};

/// The transaction sender; funded through a prestate override, so its account
/// never reaches the RPC mock.
const CALLER: Address = address!("00000000000000000000000000000000c0ffee01");

/// The call target. Has no code on the mock chain.
const CALLEE: Address = address!("00000000000000000000000000000000c0ffee02");

/// The EIP-7702 authority whose existence the scenarios vary.
const AUTHORITY: Address = address!("00000000000000000000000000000000c0ffee03");

/// The delegation designator target. Never loaded (only written).
const DELEGATE: Address = address!("00000000000000000000000000000000c0ffee04");

/// EIP-7702 `PER_EMPTY_ACCOUNT_COST - PER_AUTH_BASE_COST`: the per-authorization
/// refund granted when the authority already exists in the trie.
const EXISTING_AUTHORITY_REFUND: u64 = 12_500;

/// Build a forked `EvmeState` against a mock whose `eth_getBalance` /
/// `eth_getTransactionCount` / `eth_getCode` answers are fixed for every
/// address. `eth_getStorageAt` answers the zero word so OP L1-fee loading
/// resolves without a live node.
async fn forked_state(
    server: &MockRpcServer,
    balance: &str,
    nonce: &str,
    code: &str,
) -> EvmeState<Optimism, OpProvider> {
    server.respond_eth_chain_id(4326, 1).await;
    server.respond_method_result("eth_getBalance", balance, 2).await;
    server.respond_method_result("eth_getTransactionCount", nonce, 2).await;
    server.respond_method_result("eth_getCode", code, 2).await;
    server
        .respond_method_result(
            "eth_getStorageAt",
            "0x0000000000000000000000000000000000000000000000000000000000000000",
            2,
        )
        .await;

    let prestate_args = PreStateArgs::parse_from(["mega-evme", "--fork", "--fork.block", "1"]);
    let rpc_args = test_rpc_args(&server.uri(), None);
    let (state, _cache_store) =
        prestate_args.create_initial_state(&CALLER, &rpc_args).await.expect("create_initial_state");
    state
}

// ─── DB-level: the normalization boundary ────────────────────────────────────

/// An account whose balance, nonce, and code are all zero does not exist.
#[tokio::test(flavor = "multi_thread")]
async fn test_forked_all_zero_account_reads_as_nonexistent() {
    let server = MockRpcServer::start().await;
    let state = forked_state(&server, "0x0", "0x0", "0x").await;

    let account = state.basic_ref(AUTHORITY).expect("basic_ref");
    assert_eq!(account, None, "an all-zero RPC answer must read as a nonexistent account");
}

/// A balance alone keeps the account existing (e.g. a plain EOA that only
/// ever received funds).
#[tokio::test(flavor = "multi_thread")]
async fn test_forked_balance_only_account_exists() {
    let server = MockRpcServer::start().await;
    let state = forked_state(&server, "0x1", "0x0", "0x").await;

    let account = state.basic_ref(AUTHORITY).expect("basic_ref").expect("account must exist");
    assert_eq!(account.balance, U256::from(1));
    assert_eq!(account.nonce, 0);
}

/// A nonce alone keeps the account existing (e.g. an EOA that spent its
/// entire balance on fees).
#[tokio::test(flavor = "multi_thread")]
async fn test_forked_nonce_only_account_exists() {
    let server = MockRpcServer::start().await;
    let state = forked_state(&server, "0x0", "0x1", "0x").await;

    let account = state.basic_ref(AUTHORITY).expect("basic_ref").expect("account must exist");
    assert_eq!(account.balance, U256::ZERO);
    assert_eq!(account.nonce, 1);
}

/// Code alone keeps the account existing (e.g. a contract with neither
/// balance nor nonce is still a contract).
#[tokio::test(flavor = "multi_thread")]
async fn test_forked_code_only_account_exists() {
    let server = MockRpcServer::start().await;
    let state = forked_state(&server, "0x0", "0x0", "0x6001").await;

    let account = state.basic_ref(AUTHORITY).expect("basic_ref").expect("account must exist");
    assert!(!account.is_empty_code_hash(), "the fetched code must be reflected in the code hash");
}

// ─── Execution-level: the EIP-7702 refund consumer ───────────────────────────

/// Replay a type-4 transaction whose single authorization names `AUTHORITY`,
/// returning the gas it used. The access list pads execution gas so the
/// EIP-3529 refund cap (`gas_used / 5`) stays above the full 12,500 refund —
/// otherwise the two scenarios' gas would differ by the cap, not the refund.
async fn type4_gas_used(server: &MockRpcServer, spec: MegaSpecId) -> u64 {
    let mut state = forked_state(server, "0x0", "0x0", "0x").await;
    state.set_account_balance(CALLER, U256::from(10).pow(U256::from(18)));

    let authorization = RecoveredAuthorization::new_unchecked(
        // chain_id 0 = valid on any chain, so the context's chain id is irrelevant.
        Authorization { chain_id: U256::ZERO, address: DELEGATE, nonce: 0 },
        RecoveredAuthority::Valid(AUTHORITY),
    );
    let access_list = AccessList(vec![AccessListItem {
        address: CALLEE,
        storage_keys: (0u64..40).map(|i| B256::from(U256::from(i))).collect(),
    }]);
    let tx_env = TxEnvBuilder::default()
        .caller(CALLER)
        .call(CALLEE)
        .gas_limit(1_000_000)
        .access_list(access_list)
        .authorization_list_recovered(vec![authorization])
        .build_fill();

    let context =
        MegaContext::new(&mut state, spec).with_external_envs(EvmeExternalEnvs::new().into());
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());
    let outcome = evm.transact(tx).expect("type-4 replay must execute");

    assert!(
        matches!(outcome.result, ExecutionResult::Success { .. }),
        "type-4 replay must succeed, got {:?}",
        outcome.result,
    );
    assert_delegated(&outcome.state);
    outcome.result.tx_gas_used()
}

/// The authorization must have actually applied — a skipped authorization
/// would make the gas comparison vacuous.
fn assert_delegated(state: &EvmState) {
    let authority = state.get(&AUTHORITY).expect("the authority must be in the post-state");
    assert_eq!(authority.info.nonce, 1, "the applied authorization must bump the authority nonce");
    assert!(
        authority.info.code.as_ref().is_some_and(|code| code.is_eip7702()),
        "the authority must carry the delegation designator",
    );
}

/// A brand-new authority (all-zero on RPC) must not earn the existing-account
/// refund: the replayed `gasUsed` is exactly 12,500 above the run whose
/// authority exists. Before normalization both runs were refunded and a replay
/// under-reported `gasUsed` against the on-chain receipt.
#[rstest]
#[case::rex5(MegaSpecId::REX5)]
#[case::rex6(MegaSpecId::REX6)]
#[tokio::test(flavor = "multi_thread")]
async fn test_type4_fresh_authority_is_not_refunded(#[case] spec: MegaSpecId) {
    let fresh_server = MockRpcServer::start().await;
    let gas_fresh_authority = type4_gas_used(&fresh_server, spec).await;

    // Same transaction, but every RPC account (including the authority) holds
    // 1 wei. The caller and its balance come from the prestate override in
    // both runs, so the authority's existence is the only difference.
    let existing_server = MockRpcServer::start().await;
    existing_server.respond_method_result("eth_getBalance", "0x1", 1).await;
    let gas_existing_authority = type4_gas_used(&existing_server, spec).await;

    assert_eq!(
        gas_fresh_authority,
        gas_existing_authority + EXISTING_AUTHORITY_REFUND,
        "a fresh authority must not earn the 12,500 existing-account refund",
    );
}
