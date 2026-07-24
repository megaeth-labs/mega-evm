//! REX5 replay protection for `MEGA_SYSTEM_ADDRESS` legacy transactions.
//!
//! A legacy transaction whose caller is the runtime `system_address` is otherwise
//! normalized into a deposit-style transaction by `MegaHandler::before_run`, which
//! bypasses signature validation, nonce verification, and chain-id binding. REX5+
//! restores nonce and chain-id checks and uses canonical `InvalidTransaction` variants
//! so the rejections are classified as tx-validation errors by `IsTxError`.
//!
//! The restored checks honor the same `CfgEnv` toggles the canonical revm validate
//! path honors (`tx_chain_id_check`, `disable_nonce_check`, `disable_eip3607`). In
//! production these all default to "checks enabled"; debug / state-test / replay tools
//! that legitimately toggle them see consistent behavior across ordinary user txs and
//! system txs.
//!
//! Test layout:
//!
//! - Stale nonce, in-block double-tx, and happy path use the real block-executor flow
//!   (`MegaBlockExecutor::run_transaction` → `commit_transaction_outcome`) so the journal cache /
//!   DB commit boundary is exercised exactly as in production.
//! - Wrong chain id, missing chain id, and the cfg-toggle pin use the simpler `transact_raw` flow
//!   on a fresh EVM — those rejections (or acceptances) fire on the very first tx so a
//!   block-executor wrapping is unnecessary.

use std::convert::Infallible;

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::{block::BlockExecutor, Evm as _, EvmEnv};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    test_utils::MemoryDatabase, BlockLimits, EVMError, IOracle, MegaBlockExecutionCtx,
    MegaBlockExecutor, MegaBlockExecutorFactory, MegaContext, MegaEvm, MegaEvmFactory,
    MegaHardfork, MegaHardforkConfig, MegaSpecId, MegaTransaction, MegaTransactionError,
    MegaTxEnvelope, SequencerRegistryConfig, TestExternalEnvs, MEGA_SYSTEM_ADDRESS,
    ORACLE_CONTRACT_ADDRESS,
};
use revm::{
    context::{result::InvalidTransaction, BlockEnv, CfgEnv, ContextTr as _, TxEnv},
    database::State,
    Database as _,
};

const REGULAR_CALLER: Address = address!("0000000000000000000000000000000000100000");
const FOREIGN_CHAIN_ID: u64 = 31337;
const MEGA_CHAIN_ID: u64 = 4326;
const SYSTEM_TX_GAS_LIMIT: u64 = 4_020_000;
const BLOCK_GAS_LIMIT: u64 = 100_000_000;
const ORACLE_SLOT: U256 = U256::ZERO;

const BOOTSTRAP_SEQUENCER: Address = address!("4000000000000000000000000000000000000004");
const BOOTSTRAP_ADMIN: Address = address!("5000000000000000000000000000000000000005");

type PocExecutor<'a> = MegaBlockExecutor<
    MegaHardforkConfig,
    MegaEvm<
        &'a mut State<MemoryDatabase>,
        revm::inspector::NoOpInspector,
        TestExternalEnvs<Infallible>,
    >,
    OpAlloyReceiptBuilder,
>;

fn rex5_hardforks() -> MegaHardforkConfig {
    // Rex6 is excluded: this suite pins Rex5 semantics (v1.0.0 registry, REX5 spec).
    MegaHardforkConfig::default().with_all_activated().without(MegaHardfork::Rex6).with_params(
        SequencerRegistryConfig {
            rex5_initial_sequencer: BOOTSTRAP_SEQUENCER,
            rex5_initial_admin: BOOTSTRAP_ADMIN,
        },
    )
}

/// Per-test cfg toggles that affect the REX5 system-tx guards. Defaults match the
/// production-equivalent `CfgEnv` shape (chain-id check on, nonce check on, EIP-3607
/// on), so most tests can call `Rex5Cfg::default()`; tests that pin the cfg-toggle
/// escape hatch override one field via struct-update syntax.
#[derive(Copy, Clone)]
struct Rex5Cfg {
    /// Mirrors `CfgEnv::tx_chain_id_check` — production default `true`.
    tx_chain_id_check: bool,
    /// Mirrors `CfgEnv::disable_nonce_check` — production default `false`.
    disable_nonce_check: bool,
    /// Mirrors `CfgEnv::disable_eip3607` — production default `false`.
    disable_eip3607: bool,
}

impl Default for Rex5Cfg {
    fn default() -> Self {
        Self { tx_chain_id_check: true, disable_nonce_check: false, disable_eip3607: false }
    }
}

fn rex5_evm_env(cfg: Rex5Cfg) -> EvmEnv<MegaSpecId> {
    let mut cfg_env = CfgEnv::default();
    cfg_env.spec = MegaSpecId::REX5;
    cfg_env.chain_id = MEGA_CHAIN_ID;
    cfg_env.tx_chain_id_check = cfg.tx_chain_id_check;
    cfg_env.disable_nonce_check = cfg.disable_nonce_check;
    cfg_env.disable_eip3607 = cfg.disable_eip3607;

    let block_env = BlockEnv {
        number: U256::from(10),
        timestamp: U256::from(1_800_000_000),
        gas_limit: BLOCK_GAS_LIMIT,
        basefee: 0,
        ..Default::default()
    };
    EvmEnv::new(cfg_env, block_env)
}

fn create_rex5_block_executor<'a>(
    state: &'a mut State<MemoryDatabase>,
    cfg: Rex5Cfg,
) -> PocExecutor<'a> {
    let external_envs = TestExternalEnvs::<Infallible>::new();
    let evm_factory = MegaEvmFactory::new().with_external_env_factory(external_envs);
    let executor_factory = MegaBlockExecutorFactory::new(
        rex5_hardforks(),
        evm_factory,
        OpAlloyReceiptBuilder::default(),
    );
    let block_limits =
        BlockLimits::from_hardfork_and_block_gas_limit(MegaHardfork::Rex5, BLOCK_GAS_LIMIT);
    let block_ctx =
        MegaBlockExecutionCtx::new(B256::ZERO, Some(B256::ZERO), Bytes::new(), block_limits);
    let mut executor = executor_factory.create_executor(state, block_ctx, rex5_evm_env(cfg));
    executor.evm.ctx.chain_mut().operator_fee_scalar = Some(U256::ZERO);
    executor.evm.ctx.chain_mut().operator_fee_constant = Some(U256::ZERO);
    executor
}

fn oracle_set_slots_calldata(slot: U256, value: B256) -> Bytes {
    IOracle::setSlotsCall { slots: vec![slot], values: vec![value] }.abi_encode().into()
}

fn recovered_legacy_tx(
    signer: Address,
    nonce: u64,
    chain_id: Option<u64>,
    to: Address,
    data: Bytes,
    gas_limit: u64,
    gas_price: u128,
) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id,
        nonce,
        gas_price,
        gas_limit,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input: data,
    };
    let signed = Signed::new_unchecked(
        tx,
        Signature::test_signature(),
        B256::with_last_byte(nonce.saturating_add(1) as u8),
    );
    Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), signer)
}

fn system_tx_with(
    nonce: u64,
    chain_id: Option<u64>,
    slot: U256,
    value: B256,
) -> Recovered<MegaTxEnvelope> {
    recovered_legacy_tx(
        MEGA_SYSTEM_ADDRESS,
        nonce,
        chain_id,
        ORACLE_CONTRACT_ADDRESS,
        oracle_set_slots_calldata(slot, value),
        SYSTEM_TX_GAS_LIMIT,
        0,
    )
}

fn assert_invalid_tx_contains<E: core::fmt::Debug>(err: &E, marker: &str) {
    let dbg = format!("{err:?}");
    assert!(dbg.contains(marker), "expected error containing {marker}, got {dbg}");
}

fn oracle_storage_at<DB: revm::Database>(db: &mut DB, slot: U256) -> U256
where
    <DB as revm::Database>::Error: core::fmt::Debug,
{
    db.storage(ORACLE_CONTRACT_ADDRESS, slot).expect("oracle storage read should succeed")
}

// ============================================================================
// 1. Stale-nonce replay rejected (block executor flow)
// ============================================================================

/// REX5: a legacy `MEGA_SYSTEM_ADDRESS` tx with `tx.nonce < state.nonce` is rejected
/// at `before_run` with `InvalidTransaction::NonceTooLow`. Pre-fix, the deposit
/// promotion would skip the canonical nonce check and let the stale tx commit again,
/// rolling oracle state back. Post-fix, replay is blocked before any state mutation.
#[test]
fn test_legacy_system_address_tx_with_stale_nonce_is_rejected() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let mut executor = create_rex5_block_executor(&mut state, Rex5Cfg::default());
    executor
        .apply_pre_execution_changes()
        .expect("REX5 pre-execution should deploy the oracle and seed the registry");

    // tx0: nonce=0 → succeeds, oracle slot = 0x1111
    let old_value = B256::with_last_byte(0x11);
    let tx0 = system_tx_with(0, Some(MEGA_CHAIN_ID), ORACLE_SLOT, old_value);
    let outcome0 = executor
        .run_transaction(&tx0)
        .expect("nonce=0 system tx must succeed under REX5 happy path");
    assert!(outcome0.result.is_success());
    executor.commit_transaction_outcome(outcome0).expect("commit nonce=0");

    // tx1: nonce=1 → succeeds, oracle slot = 0x2222
    let new_value = B256::with_last_byte(0x22);
    let tx1 = system_tx_with(1, Some(MEGA_CHAIN_ID), ORACLE_SLOT, new_value);
    let outcome1 = executor.run_transaction(&tx1).expect("nonce=1 system tx must succeed");
    assert!(outcome1.result.is_success());
    executor.commit_transaction_outcome(outcome1).expect("commit nonce=1");

    // Replay tx0 (nonce=0) — must be rejected with NonceTooLow now that state.nonce == 2.
    let replay_err = executor
        .run_transaction(&tx0)
        .expect_err("stale nonce=0 replay must be rejected at validation");
    assert_invalid_tx_contains(&replay_err, "NonceTooLow");

    // Oracle slot must still hold the new_value — replay did NOT roll the value back.
    let oracle_after = oracle_storage_at(executor.evm.db_mut(), ORACLE_SLOT);
    assert_eq!(
        oracle_after,
        U256::from_be_bytes(new_value.0),
        "stale replay must not overwrite the latest oracle value",
    );
    // Sender nonce must stay at 2 (not bumped by the rejected replay).
    let nonce_after = executor.evm.db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().nonce;
    assert_eq!(nonce_after, 2, "rejected replay must not bump the system address nonce");
}

/// REX5: a legacy `MEGA_SYSTEM_ADDRESS` tx with `tx.nonce > state.nonce` is rejected
/// at `before_run` with `InvalidTransaction::NonceTooHigh`. Pins the second branch of
/// the canonical nonce comparison so a regression in either direction is caught.
#[test]
fn test_legacy_system_address_tx_with_future_nonce_is_rejected() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let mut executor = create_rex5_block_executor(&mut state, Rex5Cfg::default());
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    // state.nonce starts at 0; submit tx with nonce=5 (large gap).
    let value = B256::with_last_byte(0x77);
    let future_tx = system_tx_with(5, Some(MEGA_CHAIN_ID), ORACLE_SLOT, value);

    let err = executor
        .run_transaction(&future_tx)
        .expect_err("future-nonce system tx must be rejected at validation");
    assert_invalid_tx_contains(&err, "NonceTooHigh");

    // No state mutation: oracle slot untouched, system nonce still at 0.
    let oracle_after = oracle_storage_at(executor.evm.db_mut(), ORACLE_SLOT);
    assert_eq!(oracle_after, U256::ZERO);
    let nonce_after = executor
        .evm
        .db_mut()
        .basic(MEGA_SYSTEM_ADDRESS)
        .expect("db read should succeed")
        .map(|info| info.nonce)
        .unwrap_or(0);
    assert_eq!(nonce_after, 0);
}

// ============================================================================
// 2. In-block double-tx happy path (journal-cache correctness)
// ============================================================================

/// REX5: two legitimate sequencer system transactions in the same block must both
/// succeed, with `state.nonce` advancing 0 → 1 → 2. The nonce read uses
/// `journal.inspect_account` (cache → DB fallback, no access-list warming), so the
/// second tx in the same block sees the first tx's committed nonce bump even though
/// no DB-level flush has happened yet.
#[test]
fn test_two_legitimate_system_txs_in_same_block_both_succeed() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let mut executor = create_rex5_block_executor(&mut state, Rex5Cfg::default());
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    let value0 = B256::with_last_byte(0xAA);
    let value1 = B256::with_last_byte(0xBB);

    let tx0 = system_tx_with(0, Some(MEGA_CHAIN_ID), ORACLE_SLOT, value0);
    let tx1 = system_tx_with(1, Some(MEGA_CHAIN_ID), ORACLE_SLOT, value1);

    let outcome0 = executor.run_transaction(&tx0).expect("first in-block system tx must succeed");
    assert!(outcome0.result.is_success());
    executor.commit_transaction_outcome(outcome0).expect("commit tx0");

    let outcome1 = executor.run_transaction(&tx1).expect(
        "second in-block system tx must succeed; journal cache must reflect tx0's nonce bump",
    );
    assert!(outcome1.result.is_success());
    executor.commit_transaction_outcome(outcome1).expect("commit tx1");

    // Final state: oracle slot reflects the second update; sender nonce = 2.
    let oracle_after = oracle_storage_at(executor.evm.db_mut(), ORACLE_SLOT);
    assert_eq!(oracle_after, U256::from_be_bytes(value1.0));
    let nonce_after = executor.evm.db_mut().basic(MEGA_SYSTEM_ADDRESS).unwrap().unwrap().nonce;
    assert_eq!(nonce_after, 2);
}

// ============================================================================
// 3. Wrong chain id rejected (block executor)
// ============================================================================

/// REX5: a legacy system-address tx with a foreign chain id is rejected with
/// `InvalidTransaction::InvalidChainId`. Pre-fix, the deposit promotion would skip
/// the chain-id check and let the foreign envelope commit.
#[test]
fn test_legacy_system_address_tx_with_wrong_chain_id_is_rejected() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let mut executor = create_rex5_block_executor(&mut state, Rex5Cfg::default());
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    let value = B256::with_last_byte(0xCC);
    let foreign_tx = system_tx_with(0, Some(FOREIGN_CHAIN_ID), ORACLE_SLOT, value);

    let err = executor
        .run_transaction(&foreign_tx)
        .expect_err("foreign-chain-id system tx must be rejected at validation");
    assert_invalid_tx_contains(&err, "InvalidChainId");

    // Sender nonce unchanged at 0 — no state mutation. The system address may not even
    // exist in the DB after a pre-promotion rejection, so default to 0.
    let nonce_after = executor
        .evm
        .db_mut()
        .basic(MEGA_SYSTEM_ADDRESS)
        .expect("db read should succeed")
        .map(|info| info.nonce)
        .unwrap_or(0);
    assert_eq!(nonce_after, 0);
}

// ============================================================================
// 4. chain_id = None rejected (block executor)
// ============================================================================

/// REX5: a legacy system-address tx with `chain_id = None` (pre-EIP-155 shape) is
/// rejected with `InvalidTransaction::MissingChainId`. Production sequencer
/// transactions must carry a chain id on REX5+; this test pins that requirement.
#[test]
fn test_legacy_system_address_tx_with_chain_id_none_is_rejected() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let mut executor = create_rex5_block_executor(&mut state, Rex5Cfg::default());
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    let value = B256::with_last_byte(0xDD);
    let no_chain_id_tx = system_tx_with(0, None, ORACLE_SLOT, value);

    let err = executor
        .run_transaction(&no_chain_id_tx)
        .expect_err("system tx with chain_id=None must be rejected at validation");
    assert_invalid_tx_contains(&err, "MissingChainId");

    let nonce_after = executor
        .evm
        .db_mut()
        .basic(MEGA_SYSTEM_ADDRESS)
        .expect("db read should succeed")
        .map(|info| info.nonce)
        .unwrap_or(0);
    assert_eq!(nonce_after, 0);
}

// ============================================================================
// 5. cfg.tx_chain_id_check = false skips the system-tx chain-id guard (block executor)
// ============================================================================

/// REX5: when the operator disables `cfg.tx_chain_id_check`, the system-address
/// chain-id guard is skipped — system txs follow the same cfg-honoring policy as
/// ordinary user txs. Production defaults `tx_chain_id_check = true` so the canonical
/// chain-id binding remains in force; this test pins the cfg-toggle escape hatch for
/// debug / state-test / replay tooling.
#[test]
fn test_cfg_tx_chain_id_check_disabled_skips_system_tx_chain_id_guard() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let mut executor = create_rex5_block_executor(
        &mut state,
        Rex5Cfg { tx_chain_id_check: false, ..Default::default() },
    );
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    let value = B256::with_last_byte(0xEE);
    let foreign_tx = system_tx_with(0, Some(FOREIGN_CHAIN_ID), ORACLE_SLOT, value);

    let outcome = executor
        .run_transaction(&foreign_tx)
        .expect("system tx with foreign chain id must succeed when cfg.tx_chain_id_check = false");
    assert!(
        outcome.result.is_success(),
        "system tx must commit when cfg.tx_chain_id_check = false; got {:?}",
        outcome.result,
    );
    executor.commit_transaction_outcome(outcome).expect("commit cfg-disabled system tx");

    let oracle_after = oracle_storage_at(executor.evm.db_mut(), ORACLE_SLOT);
    assert_eq!(
        oracle_after,
        U256::from_be_bytes(value.0),
        "oracle storage must reflect the system tx write when cfg.tx_chain_id_check is off",
    );
}

// ============================================================================
// 5b. cfg.disable_nonce_check = true skips the system-tx nonce guard
// ============================================================================

/// REX5: when the operator sets `cfg.disable_nonce_check = true`, the system-address
/// nonce guard is skipped — same input that would be rejected as `NonceTooLow` under
/// the default cfg now commits. Mirrors
/// `test_legacy_system_address_tx_with_stale_nonce_is_rejected` to keep the cfg-toggle contract
/// symmetric.
#[test]
fn test_cfg_disable_nonce_check_skips_system_tx_nonce_guard() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let mut executor = create_rex5_block_executor(
        &mut state,
        Rex5Cfg { disable_nonce_check: true, ..Default::default() },
    );
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    // tx0: nonce=0 → state.nonce advances to 1.
    let v0 = B256::with_last_byte(0x11);
    let tx0 = system_tx_with(0, Some(MEGA_CHAIN_ID), ORACLE_SLOT, v0);
    let outcome0 = executor.run_transaction(&tx0).expect("nonce=0 system tx must succeed");
    assert!(outcome0.result.is_success());
    executor.commit_transaction_outcome(outcome0).expect("commit nonce=0");

    // tx_stale: nonce=0 again. Under prod cfg this is rejected (`NonceTooLow`); with
    // `disable_nonce_check = true` it must commit and overwrite the oracle slot.
    let v_stale = B256::with_last_byte(0x99);
    let tx_stale = system_tx_with(0, Some(MEGA_CHAIN_ID), ORACLE_SLOT, v_stale);
    let outcome_stale = executor
        .run_transaction(&tx_stale)
        .expect("stale-nonce system tx must succeed when cfg.disable_nonce_check = true");
    assert!(
        outcome_stale.result.is_success(),
        "system tx must commit when cfg.disable_nonce_check = true; got {:?}",
        outcome_stale.result,
    );
    executor.commit_transaction_outcome(outcome_stale).expect("commit stale-nonce tx");

    let oracle_after = oracle_storage_at(executor.evm.db_mut(), ORACLE_SLOT);
    assert_eq!(
        oracle_after,
        U256::from_be_bytes(v_stale.0),
        "stale-nonce tx must overwrite oracle slot when nonce check is off",
    );
}

// ============================================================================
// 5c. cfg.disable_eip3607 honoring for the system address
// ============================================================================

/// REX5: with the production-default `cfg.disable_eip3607 = false`, a system tx must be
/// rejected (`RejectCallerWithCode`) when `MEGA_SYSTEM_ADDRESS` has bytecode — the
/// canonical EIP-3607 contract. This is defense-in-depth: prod never deploys code at
/// the system address, but if some future code path or misconfiguration does, the
/// system-tx path hard-fails instead of silently executing against a code-bearing
/// caller.
#[test]
fn test_cfg_disable_eip3607_false_rejects_system_tx_when_system_address_has_code() {
    let mut db = MemoryDatabase::default();
    // Plant arbitrary bytecode at MEGA_SYSTEM_ADDRESS so the EIP-3607 check fires.
    db.set_account_code(MEGA_SYSTEM_ADDRESS, Bytes::from_static(&[0x00]));
    let mut state = State::builder().with_database(db).build();
    let mut executor = create_rex5_block_executor(&mut state, Rex5Cfg::default());
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    let value = B256::with_last_byte(0xCD);
    let tx = system_tx_with(0, Some(MEGA_CHAIN_ID), ORACLE_SLOT, value);

    let err = executor.run_transaction(&tx).expect_err(
        "system tx must be rejected by EIP-3607 when MEGA_SYSTEM_ADDRESS has code and cfg.disable_eip3607 = false",
    );
    assert_invalid_tx_contains(&err, "RejectCallerWithCode");
}

/// REX5: with `cfg.disable_eip3607 = true`, the same code-bearing-system-address input
/// commits — the cfg-toggle escape hatch lets debug / state-test / replay tooling
/// proceed even if the system address happens to carry bytecode in their fixture.
#[test]
fn test_cfg_disable_eip3607_true_accepts_system_tx_when_system_address_has_code() {
    let mut db = MemoryDatabase::default();
    db.set_account_code(MEGA_SYSTEM_ADDRESS, Bytes::from_static(&[0x00]));
    let mut state = State::builder().with_database(db).build();
    let mut executor = create_rex5_block_executor(
        &mut state,
        Rex5Cfg { disable_eip3607: true, ..Default::default() },
    );
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    let value = B256::with_last_byte(0xCE);
    let tx = system_tx_with(0, Some(MEGA_CHAIN_ID), ORACLE_SLOT, value);
    let outcome = executor.run_transaction(&tx).expect(
        "system tx must succeed when cfg.disable_eip3607 = true even with code at system address",
    );
    assert!(outcome.result.is_success());
    executor.commit_transaction_outcome(outcome).expect("commit cfg-disabled-eip3607 tx");

    let oracle_after = oracle_storage_at(executor.evm.db_mut(), ORACLE_SLOT);
    assert_eq!(oracle_after, U256::from_be_bytes(value.0));
}

// ============================================================================
// 6. Happy path (block executor)
// ============================================================================

/// REX5: the canonical happy path — single legitimate system tx commits and updates
/// oracle state.
#[test]
fn test_legacy_system_address_tx_with_correct_nonce_and_chain_id_succeeds() {
    let mut state = State::builder().with_database(MemoryDatabase::default()).build();
    let mut executor = create_rex5_block_executor(&mut state, Rex5Cfg::default());
    executor.apply_pre_execution_changes().expect("apply pre-execution");

    let value = B256::with_last_byte(0x55);
    let tx = system_tx_with(0, Some(MEGA_CHAIN_ID), ORACLE_SLOT, value);
    let outcome =
        executor.run_transaction(&tx).expect("happy-path system tx must succeed under REX5");
    assert!(outcome.result.is_success());
    executor.commit_transaction_outcome(outcome).expect("commit happy-path tx");

    let oracle_after = oracle_storage_at(executor.evm.db_mut(), ORACLE_SLOT);
    assert_eq!(oracle_after, U256::from_be_bytes(value.0));
}

// ============================================================================
// 7. Normal user legacy tx is unaffected
// ============================================================================

/// Sanity: REX5+ guards apply only to txs whose caller is the system address. A
/// regular EOA legacy tx still goes through the canonical validate path, with normal
/// nonce + chain-id behavior. We test this via `transact_raw` because there's no
/// system-tx interaction to verify here.
#[test]
fn test_normal_user_legacy_tx_is_unaffected() {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(REGULAR_CALLER, U256::from(10u64).pow(U256::from(20u64))); // 100 ETH
    let recipient = address!("1000000000000000000000000000000000000001");
    db.set_account_balance(recipient, U256::from(1u64));

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);

    let tx = TxEnv {
        caller: REGULAR_CALLER,
        kind: TxKind::Call(recipient),
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit: 100_000,
        gas_price: 0,
        chain_id: Some(MEGA_CHAIN_ID), /* canonical chain id; not validated here because cfg
                                        * defaults */
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());

    let result: Result<_, EVMError<Infallible, MegaTransactionError>> =
        alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(result.is_ok(), "normal user legacy tx must still execute successfully under REX5");
    assert!(result.unwrap().result.is_success());
}

// ============================================================================
// 8. Actual OP deposit transaction is unaffected by the REX5 guards
// ============================================================================

/// Boundary pin: a genuine `DEPOSIT_TRANSACTION_TYPE` transaction (one that already
/// arrives stamped with a non-zero `source_hash`, not a legacy tx promoted by the Mega
/// pre-handler) must not be intercepted by the REX5 system-address chain-id / nonce
/// guards. The guards key off `sent_from_system_address` (= `caller == system_address`),
/// so a deposit tx whose caller is anyone else passes through `before_run` untouched.
/// This test pins that structural boundary so a future change extending the guards can't
/// silently break OP deposit semantics.
#[test]
fn test_actual_op_deposit_tx_is_unaffected() {
    use mega_evm::MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;

    let mut db = MemoryDatabase::default();
    db.set_account_balance(REGULAR_CALLER, U256::from(10u64).pow(U256::from(20u64))); // 100 ETH
    let recipient = address!("1000000000000000000000000000000000000001");
    db.set_account_balance(recipient, U256::from(1u64));

    let mut context = MegaContext::new(&mut db, MegaSpecId::REX5);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    let mut evm = MegaEvm::new(context);

    // Build a tx that op-revm classifies as deposit-typed via a non-zero source_hash but
    // whose caller is NOT the system address — the REX5 system-tx guards must not fire.
    let tx_inner = TxEnv {
        caller: REGULAR_CALLER,
        kind: TxKind::Call(recipient),
        data: Bytes::new(),
        value: U256::ZERO,
        gas_limit: 100_000,
        gas_price: 0,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_inner);
    tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
    tx.enveloped_tx = Some(Bytes::new());

    let result: Result<_, EVMError<Infallible, MegaTransactionError>> =
        alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(
        result.is_ok(),
        "deposit-typed tx with non-system caller must execute successfully under REX5",
    );
    assert!(result.unwrap().result.is_success());
}

// ============================================================================
// Compile-time guard: pin the canonical InvalidTransaction variant set used here.
// ============================================================================
const _: fn() = || {
    let _ = InvalidTransaction::NonceTooLow { tx: 0, state: 0 };
    let _ = InvalidTransaction::NonceTooHigh { tx: 0, state: 0 };
    let _ = InvalidTransaction::InvalidChainId;
    let _ = InvalidTransaction::MissingChainId;
};
