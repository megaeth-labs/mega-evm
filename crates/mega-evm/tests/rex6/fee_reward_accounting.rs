//! REX6 post-execution fee-reward accounting tests.
//!
//! `op_revm`'s `reward_beneficiary` runs AFTER the `MegaETH` `AdditionalLimit`
//! trackers are finalised at the end of each transaction. It credits up to four
//! accounts:
//!
//! - block beneficiary (coinbase)  — priority fee
//! - `BASE_FEE_RECIPIENT`          — basefee × `gas_used`
//! - `L1_FEE_RECIPIENT`            — L1 data fee
//! - `OPERATOR_FEE_RECIPIENT`      — operator fee
//!
//! Each credit that changes a recipient's balance is an account-info write, and a
//! credit to a not-yet-existing recipient materialises the account. Because these
//! writes happen after the per-frame limit accounting is finalised, REX6 records
//! them directly on the transaction-level lane: `data_size` gains one account-info
//! write and `kv_updates` gains 1 for every recipient whose balance changes, plus
//! `state_growth` gains 1 for every recipient that is newly materialised. Pre-REX6
//! specs record none of this (frozen for replay parity).
//!
//! These tests pin each half of that behaviour and the pre-REX6 freeze.

use std::convert::Infallible;

use alloy_primitives::{address, keccak256, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    EmptyExternalEnv, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError, ACCOUNT_INFO_WRITE_SIZE, MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
};
use op_revm::constants::BASE_FEE_RECIPIENT;
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ResultAndState},
        BlockEnv, ContextSetters, TxEnv,
    },
    handler::EvmTr,
    primitives::TxKind,
};

// ============================================================================
// TEST ADDRESSES
// ============================================================================

/// A well-funded EOA sender for the normal (non-deposit) transaction.
const CALLER: Address = address!("00000000000000000000000000000000C0DE0001");

/// A simple callee contract that just RETURNs immediately.
const TARGET_CONTRACT: Address = address!("00000000000000000000000000000000C0DE0002");

/// Coinbase / block beneficiary address.
const COINBASE: Address = address!("00000000000000000000000000000000C0FFEE01");

// ============================================================================
// CONSTANTS
// ============================================================================

/// Block base-fee (in wei).  Setting `gas_price == BASEFEE` means the priority
/// fee to the coinbase is zero, so the only fee-vault credit on this path is
/// `BASE_FEE_RECIPIENT ← basefee × gas_used`.
const BASEFEE: u64 = 1_000_000;

/// Gas limit for the test transaction — large enough to comfortably cover the
/// `MegaETH` REX intrinsic (60 k) plus the trivial RETURN contract execution.
const TX_GAS_LIMIT: u64 = 1_000_000;

/// Caller starting balance (1 ETH) — enough to cover `gas_limit × gas_price`.
const CALLER_BALANCE: u128 = 1_000_000_000_000_000_000; // 1 ETH

// ============================================================================
// HARNESS HELPERS
// ============================================================================

type TestEvm = MegaEvm<MemoryDatabase, revm::inspector::NoOpInspector, EmptyExternalEnv>;
type TestEvmResult =
    Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, MegaTransactionError>>;

/// Deploys a trivial `RETURN` bytecode at `TARGET_CONTRACT`.
fn simple_return_contract() -> Bytes {
    BytecodeBuilder::default().push_number(0u64).push_number(0u64).append(RETURN).build()
}

/// Builds a `MegaEvm` for the given `spec` with the given database and `basefee`.
///
/// - Block: `gas_limit = 1_000_000_000`, `basefee = basefee`, `beneficiary = COINBASE`.
/// - Chain: operator-fee scalar and constant zeroed so `OPERATOR_FEE_RECIPIENT` receives no credit
///   and cannot accidentally materialise.
fn build_evm(spec: MegaSpecId, db: MemoryDatabase, basefee: u64) -> TestEvm {
    let mut context = MegaContext::new(db, spec);
    context.set_block(BlockEnv {
        gas_limit: 1_000_000_000,
        basefee,
        beneficiary: COINBASE,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    MegaEvm::new(context)
}

/// Constructs the normal (non-deposit) `Call` transaction used in both runs.
///
/// `gas_price == BASEFEE`: the priority-fee tip to coinbase is zero, but
/// `BASE_FEE_RECIPIENT` is credited `basefee × gas_used > 0` — which
/// materialises it when the account is absent from the DB.
fn make_call_tx() -> MegaTransaction {
    let mut tx = MegaTransaction {
        base: TxEnv {
            caller: CALLER,
            kind: TxKind::Call(TARGET_CONTRACT),
            gas_limit: TX_GAS_LIMIT,
            gas_price: BASEFEE as u128,
            ..Default::default()
        },
        ..Default::default()
    };
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Executes `tx` against `db`, pinned to REX6 + BASEFEE. Delegates to `transact_with_spec`.
fn transact_with(db: MemoryDatabase, tx: MegaTransaction) -> (TestEvmResult, TestEvm) {
    transact_with_spec(MegaSpecId::REX6, db, BASEFEE, tx)
}

/// Generic variant: executes `tx` against `db` under the given `spec` and `basefee`.
fn transact_with_spec(
    spec: MegaSpecId,
    db: MemoryDatabase,
    basefee: u64,
    tx: MegaTransaction,
) -> (TestEvmResult, TestEvm) {
    let mut evm = build_evm(spec, db, basefee);
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx);
    (r, evm)
}

/// A transaction with a 1-wei priority fee so the block beneficiary receives a non-zero
/// credit: `effective_gas_price = BASEFEE + 1` → `coinbase_gas_price = 1` → `1 × gas_used`.
fn make_tip_tx() -> MegaTransaction {
    let mut tx = MegaTransaction {
        base: TxEnv {
            caller: CALLER,
            kind: TxKind::Call(TARGET_CONTRACT),
            gas_limit: TX_GAS_LIMIT,
            gas_price: BASEFEE as u128 + 1,
            gas_priority_fee: Some(1),
            ..Default::default()
        },
        ..Default::default()
    };
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// An OP deposit-style transaction. A non-zero `source_hash` flips `tx_type` to deposit, and
/// op-revm's `reward_beneficiary` early-returns for deposits — so no fee recipient is credited.
fn make_deposit_tx() -> MegaTransaction {
    let mut tx = MegaTransaction {
        base: TxEnv {
            caller: CALLER,
            kind: TxKind::Call(TARGET_CONTRACT),
            gas_limit: TX_GAS_LIMIT,
            gas_price: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
    tx.deposit.mint = Some(0);
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// Builds a REX6 EVM with a custom block `beneficiary` (the standard `build_evm` hardcodes
/// `COINBASE`), runs `tx`, asserts success, and returns the EVM for usage inspection. Used to
/// exercise the recipient dedup when the beneficiary coincides with a fee vault.
fn transact_rex6_with_beneficiary(
    db: MemoryDatabase,
    beneficiary: Address,
    tx: MegaTransaction,
) -> TestEvm {
    let mut context = MegaContext::new(db, MegaSpecId::REX6);
    context.set_block(BlockEnv {
        gas_limit: 1_000_000_000,
        basefee: BASEFEE,
        beneficiary,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let res = alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(
        res.expect("tx must not produce a validation error").result.is_success(),
        "tx must succeed",
    );
    evm
}

// ============================================================================
// TESTS
// ============================================================================

/// REX6: materialising `BASE_FEE_RECIPIENT` inside `reward_beneficiary` records
/// exactly +1 `state_growth` compared with a run where the account already exists.
/// The credit runs after the `AdditionalLimit` snapshot, so the new-account event
/// is recorded on the transaction-level lane rather than via a frame.
#[test]
fn test_rex6_fee_recipient_materialization_records_state_growth() {
    // --- Run A: BASE_FEE_RECIPIENT absent from the DB ---------------------------
    // The first basefee credit will materialise an empty account → should record
    // +1 state_growth under REX6.
    let db_empty = MemoryDatabase::default()
        // Fund CALLER enough to pay gas: gas_limit × gas_price
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        // Install the trivial RETURN contract
        .account_code(TARGET_CONTRACT, simple_return_contract());

    let tx_empty = make_call_tx();
    let (res_empty, evm_empty) = transact_with(db_empty, tx_empty);
    assert!(
        res_empty.expect("run A must not produce a validation error").result.is_success(),
        "run A (empty BASE_FEE_RECIPIENT): tx must succeed",
    );
    let growth_empty = evm_empty.ctx_ref().additional_limit.borrow().get_usage().state_growth;

    // --- Run B: BASE_FEE_RECIPIENT pre-funded (already exists) ------------------
    // The basefee credit lands on an existing account → no new account created →
    // state_growth should be one less than run A (once the fix is in).
    let db_funded = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_code(TARGET_CONTRACT, simple_return_contract())
        // Pre-seed BASE_FEE_RECIPIENT so it already exists before the tx runs.
        .account_balance(BASE_FEE_RECIPIENT, U256::from(1u64));

    let tx_funded = make_call_tx();
    let (res_funded, evm_funded) = transact_with(db_funded, tx_funded);
    assert!(
        res_funded.expect("run B must not produce a validation error").result.is_success(),
        "run B (pre-funded BASE_FEE_RECIPIENT): tx must succeed",
    );
    let growth_funded = evm_funded.ctx_ref().additional_limit.borrow().get_usage().state_growth;

    // The empty run must record exactly +1 more state_growth than the funded run.
    // Before the REX6 fix this FAILS with: left=0, right=1
    assert_eq!(
        growth_empty.saturating_sub(growth_funded),
        1,
        "materialising BASE_FEE_RECIPIENT in reward_beneficiary must record exactly \
         +1 state_growth vs the pre-funded baseline \
         (empty={growth_empty}, funded={growth_funded})",
    );
}

// ============================================================================
// DataSize / KV account-info-write isolation
// ============================================================================

/// REX6: every time a fee recipient's balance changes, `reward_beneficiary`
/// must record an account-info write of `+40` `data_size` and `+1` `kv_updates`.
///
/// Strategy: hold `state_growth` constant by pre-funding `BASE_FEE_RECIPIENT` in
/// both runs, then vary only whether the recipient *receives* a non-zero credit:
///
/// - **Credited run**: `basefee = BASEFEE`, `gas_price = BASEFEE` → base-fee credit is `> 0`, so
///   `BASE_FEE_RECIPIENT`'s balance changes → account-info write is charged.
/// - **Baseline run**: `basefee = 0`, `gas_price = 0` → base-fee credit is `0`, balance unchanged →
///   no account-info write.
///
/// Expected delta: `credited.data_size - baseline.data_size == 40` and
/// `credited.kv_updates - baseline.kv_updates == 1`.
#[test]
fn test_rex6_fee_recipient_records_data_size_and_kv() {
    // Common DB builder that pre-funds BASE_FEE_RECIPIENT so state_growth is equal
    // in both runs (no new-account materialisation either way).
    let funded_db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(CALLER_BALANCE))
            .account_code(TARGET_CONTRACT, simple_return_contract())
            .account_balance(BASE_FEE_RECIPIENT, U256::from(1u64))
    };

    // --- Credited run: basefee = BASEFEE, gas_price = BASEFEE → base-fee credit > 0 ---
    let (res_credited, evm_credited) =
        transact_with_spec(MegaSpecId::REX6, funded_db(), BASEFEE, make_call_tx());
    assert!(
        res_credited.expect("credited run must not produce a validation error").result.is_success(),
        "credited run: tx must succeed",
    );
    let credited = evm_credited.ctx_ref().additional_limit.borrow().get_usage();

    // --- Baseline run: basefee = 0, gas_price = 0 → base-fee credit is 0, no write ---
    let mut tx_zero_price = make_call_tx();
    tx_zero_price.base.gas_price = 0;
    let (res_baseline, evm_baseline) =
        transact_with_spec(MegaSpecId::REX6, funded_db(), 0, tx_zero_price);
    assert!(
        res_baseline.expect("baseline run must not produce a validation error").result.is_success(),
        "baseline run: tx must succeed",
    );
    let baseline = evm_baseline.ctx_ref().additional_limit.borrow().get_usage();

    assert_eq!(
        credited.data_size.saturating_sub(baseline.data_size),
        ACCOUNT_INFO_WRITE_SIZE,
        "crediting BASE_FEE_RECIPIENT must add exactly {ACCOUNT_INFO_WRITE_SIZE} data_size \
         (credited={}, baseline={})",
        credited.data_size,
        baseline.data_size,
    );
    assert_eq!(
        credited.kv_updates.saturating_sub(baseline.kv_updates),
        1,
        "crediting BASE_FEE_RECIPIENT must add exactly 1 kv_update \
         (credited={}, baseline={})",
        credited.kv_updates,
        baseline.kv_updates,
    );
}

// ============================================================================
// Beneficiary (priority-fee) materialisation
// ============================================================================

/// REX6: when the block beneficiary (coinbase) receives a non-zero
/// priority-fee tip and its account did not previously exist, `reward_beneficiary`
/// must record `+1 state_growth` (new-account materialisation) while
/// `data_size` stays equal between the empty and pre-funded cases (both runs
/// credit the coinbase, so both incur the `+40` account-info write; only the
/// first-time materialisation differs).
///
/// Tx config: EIP-1559 style with `gas_price = BASEFEE + 1` and
/// `gas_priority_fee = Some(1)` → `effective_gas_price = BASEFEE + 1` →
/// `coinbase_gas_price = 1` → coinbase receives `1 × gas_used > 0`.
///
/// `BASE_FEE_RECIPIENT` is pre-funded in both runs so it does not contribute
/// differing `state_growth`.
#[test]
fn test_rex6_beneficiary_tip_materialization_records_accounting() {
    // Pre-fund BASE_FEE_RECIPIENT in both runs so it never contributes differing
    // state_growth.  The only variable is whether COINBASE is empty or pre-funded.
    let base_db_funded_bfr = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(CALLER_BALANCE))
            .account_code(TARGET_CONTRACT, simple_return_contract())
            .account_balance(BASE_FEE_RECIPIENT, U256::from(1u64))
    };

    // --- Empty COINBASE run: COINBASE does not yet exist → materialised on credit ---
    let (res_empty, evm_empty) =
        transact_with_spec(MegaSpecId::REX6, base_db_funded_bfr(), BASEFEE, make_tip_tx());
    assert!(
        res_empty
            .expect("empty-coinbase run must not produce a validation error")
            .result
            .is_success(),
        "empty-coinbase run: tx must succeed",
    );
    let empty = evm_empty.ctx_ref().additional_limit.borrow().get_usage();

    // --- Pre-funded COINBASE run: COINBASE already exists → no materialisation ---
    let db_funded_coinbase = base_db_funded_bfr().account_balance(COINBASE, U256::from(1u64));
    let (res_funded, evm_funded) =
        transact_with_spec(MegaSpecId::REX6, db_funded_coinbase, BASEFEE, make_tip_tx());
    assert!(
        res_funded
            .expect("funded-coinbase run must not produce a validation error")
            .result
            .is_success(),
        "funded-coinbase run: tx must succeed",
    );
    let funded = evm_funded.ctx_ref().additional_limit.borrow().get_usage();

    // Materialising COINBASE adds exactly +1 state_growth.
    assert_eq!(
        empty.state_growth.saturating_sub(funded.state_growth),
        1,
        "materialising COINBASE must add exactly +1 state_growth \
         (empty={}, funded={})",
        empty.state_growth,
        funded.state_growth,
    );

    // Both runs credit COINBASE a non-zero tip, so both charge the +40 account-info
    // write; data_size must be identical (the materialisation delta is captured by
    // state_growth, not an extra data_size charge).
    assert_eq!(
        empty.data_size, funded.data_size,
        "data_size must be equal when only COINBASE materialisation differs \
         (empty={}, funded={})",
        empty.data_size, funded.data_size,
    );
    assert_eq!(
        empty.kv_updates, funded.kv_updates,
        "both runs credit the coinbase a non-zero tip → both charge +1 kv_update; only state_growth differs",
    );
}

// ============================================================================
// REX5 freeze guard
// ============================================================================

/// REX5 freeze guard: under `MegaSpecId::REX5` the fee-reward path must record
/// **no** fee-recipient accounting.  An empty vs pre-funded `BASE_FEE_RECIPIENT`
/// must produce byte-identical `state_growth`, `data_size`, and `kv_updates`.
#[test]
fn test_rex5_freeze_no_fee_recipient_accounting() {
    // --- Run A: BASE_FEE_RECIPIENT absent (would materialise under REX6) ----------
    let db_empty = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_code(TARGET_CONTRACT, simple_return_contract());

    let (res_empty, evm_empty) =
        transact_with_spec(MegaSpecId::REX5, db_empty, BASEFEE, make_call_tx());
    assert!(
        res_empty.expect("REX5 empty run must not produce a validation error").result.is_success(),
        "REX5 empty run: tx must succeed",
    );
    let empty = evm_empty.ctx_ref().additional_limit.borrow().get_usage();

    // --- Run B: BASE_FEE_RECIPIENT pre-funded ------------------------------------
    let db_funded = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_code(TARGET_CONTRACT, simple_return_contract())
        .account_balance(BASE_FEE_RECIPIENT, U256::from(1u64));

    let (res_funded, evm_funded) =
        transact_with_spec(MegaSpecId::REX5, db_funded, BASEFEE, make_call_tx());
    assert!(
        res_funded
            .expect("REX5 funded run must not produce a validation error")
            .result
            .is_success(),
        "REX5 funded run: tx must succeed",
    );
    let funded = evm_funded.ctx_ref().additional_limit.borrow().get_usage();

    // Under REX5 the fee-reward path is frozen: no accounting is recorded for any
    // fee-recipient balance change, so all usage fields must be identical.
    assert_eq!(
        empty.state_growth, funded.state_growth,
        "REX5: state_growth must be equal regardless of BASE_FEE_RECIPIENT presence \
         (empty={}, funded={})",
        empty.state_growth, funded.state_growth,
    );
    assert_eq!(
        empty.data_size, funded.data_size,
        "REX5: data_size must be equal regardless of BASE_FEE_RECIPIENT presence \
         (empty={}, funded={})",
        empty.data_size, funded.data_size,
    );
    assert_eq!(
        empty.kv_updates, funded.kv_updates,
        "REX5: kv_updates must be equal regardless of BASE_FEE_RECIPIENT presence \
         (empty={}, funded={})",
        empty.kv_updates, funded.kv_updates,
    );
}

// ============================================================================
// Recipient dedup (beneficiary coincides with a fee vault)
// ============================================================================

/// REX6: when the block beneficiary coincides with a fee vault, op-revm issues two
/// `balance_incr`s to the SAME on-chain account (the tip as beneficiary + the base fee as
/// vault) — but that is still a single account write. The recipient dedup must collapse it to
/// one charge, not double-count.
///
/// Strategy: compare a collision run (beneficiary == `BASE_FEE_RECIPIENT`) against a distinct
/// run (beneficiary == `COINBASE`). Both use a tip tx under `basefee > 0` with both accounts
/// empty, so the distinct run materialises TWO accounts while the collision run materialises
/// ONE. The distinct run must therefore record exactly one more account write. Without dedup,
/// the collision run would also count two and these deltas would be zero.
#[test]
fn test_rex6_beneficiary_equal_to_fee_vault_is_counted_once() {
    let db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(CALLER_BALANCE))
            .account_code(TARGET_CONTRACT, simple_return_contract())
    };

    // Collision: beneficiary == BASE_FEE_RECIPIENT → both credits hit one empty account.
    let collide = transact_rex6_with_beneficiary(db(), BASE_FEE_RECIPIENT, make_tip_tx())
        .ctx_ref()
        .additional_limit
        .borrow()
        .get_usage();

    // Distinct: beneficiary == COINBASE (≠ BASE_FEE_RECIPIENT) → two empty accounts credited.
    let distinct = transact_rex6_with_beneficiary(db(), COINBASE, make_tip_tx())
        .ctx_ref()
        .additional_limit
        .borrow()
        .get_usage();

    assert_eq!(
        distinct.state_growth.saturating_sub(collide.state_growth),
        1,
        "beneficiary == fee vault must materialise one account, not two \
         (collide={}, distinct={})",
        collide.state_growth,
        distinct.state_growth,
    );
    assert_eq!(
        distinct.data_size.saturating_sub(collide.data_size),
        ACCOUNT_INFO_WRITE_SIZE,
        "collision must record one account-info write, not two (collide={}, distinct={})",
        collide.data_size,
        distinct.data_size,
    );
    assert_eq!(
        distinct.kv_updates.saturating_sub(collide.kv_updates),
        1,
        "collision must record one kv_update, not two (collide={}, distinct={})",
        collide.kv_updates,
        distinct.kv_updates,
    );
}

// ============================================================================
// Deposit transactions record no fee-recipient accounting
// ============================================================================

/// REX6: deposit transactions credit no fee recipient — op-revm's `reward_beneficiary`
/// early-returns for deposits — so the fee-reward accounting records nothing. An empty vs
/// pre-funded `BASE_FEE_RECIPIENT` must produce identical usage even with `basefee > 0`. This
/// pins the "no special-casing needed" property against op-revm behavior drift.
#[test]
fn test_rex6_deposit_tx_records_no_fee_recipient_accounting() {
    // CALLER is pre-funded (non-empty) in both runs so the deposit-caller path contributes no
    // differing state growth; the only variable is BASE_FEE_RECIPIENT presence.
    let db_empty = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_code(TARGET_CONTRACT, simple_return_contract());
    let (res_empty, evm_empty) =
        transact_with_spec(MegaSpecId::REX6, db_empty, BASEFEE, make_deposit_tx());
    assert!(
        res_empty
            .expect("deposit empty run must not produce a validation error")
            .result
            .is_success(),
        "deposit empty run: tx must succeed",
    );
    let empty = evm_empty.ctx_ref().additional_limit.borrow().get_usage();

    let db_funded = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_code(TARGET_CONTRACT, simple_return_contract())
        .account_balance(BASE_FEE_RECIPIENT, U256::from(1u64));
    let (res_funded, evm_funded) =
        transact_with_spec(MegaSpecId::REX6, db_funded, BASEFEE, make_deposit_tx());
    assert!(
        res_funded
            .expect("deposit funded run must not produce a validation error")
            .result
            .is_success(),
        "deposit funded run: tx must succeed",
    );
    let funded = evm_funded.ctx_ref().additional_limit.borrow().get_usage();

    assert_eq!(
        empty.state_growth, funded.state_growth,
        "deposit tx must record no fee-recipient state_growth (empty={}, funded={})",
        empty.state_growth, funded.state_growth,
    );
    assert_eq!(
        empty.data_size, funded.data_size,
        "deposit tx must record no fee-recipient data_size (empty={}, funded={})",
        empty.data_size, funded.data_size,
    );
    assert_eq!(
        empty.kv_updates, funded.kv_updates,
        "deposit tx must record no fee-recipient kv_updates (empty={}, funded={})",
        empty.kv_updates, funded.kv_updates,
    );
}

// ============================================================================
// DB-error path on a fee-recipient read
// ============================================================================

/// REX6: a database failure while reading a fee recipient in `reward_beneficiary` must
/// surface as `EVMError::Custom` (the `inspect_account` `map_err` wrap), not be silently
/// dropped. The pre-reward snapshot reads `BASE_FEE_RECIPIENT`; injecting a DB failure there
/// exercises the error path.
#[test]
fn test_rex6_fee_recipient_db_error_surfaces_as_custom() {
    let inner = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_code(TARGET_CONTRACT, simple_return_contract());
    let mut db = ErrorInjectingDatabase::new(inner);
    // The reward snapshot's `inspect_account(BASE_FEE_RECIPIENT)` read fails.
    db.fail_on_account = Some(BASE_FEE_RECIPIENT);

    let mut context = MegaContext::new(db, MegaSpecId::REX6);
    context.set_block(BlockEnv {
        gas_limit: 1_000_000_000,
        basefee: BASEFEE,
        beneficiary: COINBASE,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });

    let mut evm = MegaEvm::new(context);
    let res = alloy_evm::Evm::transact_raw(&mut evm, make_call_tx());

    match res {
        Err(EVMError::Custom(msg)) => assert!(
            msg.contains("Failed to inspect fee recipient"),
            "expected the fee-recipient inspect_account DB error to be wrapped, got: {msg}",
        ),
        other => panic!(
            "expected EVMError::Custom for fee-recipient inspect_account DB error, got {other:?}",
        ),
    }
}

/// A REX6 deposit-style tx must not read the fee vaults at all: op-revm's reward path credits
/// nothing for deposits, so the snapshot / diff pass is skipped and a stateless witness that
/// carries no fee-vault entries stays sufficient. Pinned by injecting a DB error on the
/// `BASE_FEE_RECIPIENT` read — before the deposit guard, `snapshot_fee_recipients` tripped it.
#[test]
fn test_rex6_deposit_skips_fee_recipient_snapshots() {
    let inner = MemoryDatabase::default().account_balance(CALLER, U256::from(CALLER_BALANCE));
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_account = Some(BASE_FEE_RECIPIENT);

    let mut context = MegaContext::new(db, MegaSpecId::REX6);
    context.set_block(BlockEnv {
        gas_limit: 1_000_000_000,
        basefee: BASEFEE,
        beneficiary: COINBASE,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let r = alloy_evm::Evm::transact_raw(&mut evm, make_deposit_tx())
        .expect("a deposit must not read any fee vault");
    assert!(r.result.is_success(), "deposit must succeed: {:?}", r.result);
}

/// Fee-reward snapshots must not hydrate a contract recipient's bytecode: only balance and
/// emptiness are read, so a stateless witness needs the recipient's account proof, not its code.
/// Pinned by giving the beneficiary lazy code and injecting a DB error on its `code_by_hash` —
/// before the non-hydrating read, the post-reward re-read hit `inspect_account`'s
/// occupied-branch hydration.
#[test]
fn test_rex6_fee_recipient_snapshot_does_not_hydrate_code() {
    let code_hash = keccak256([0x5b]); // JUMPDEST — never executed, only referenced by hash
    let inner = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_lazy_code(COINBASE, code_hash);
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_code_by_hash = Some(code_hash);

    let mut context = MegaContext::new(db, MegaSpecId::REX6);
    context.set_block(BlockEnv {
        gas_limit: 1_000_000_000,
        basefee: BASEFEE,
        beneficiary: COINBASE,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let r = alloy_evm::Evm::transact_raw(&mut evm, make_tip_tx())
        .expect("fee-reward accounting must not load the recipient's bytecode");
    assert!(r.result.is_success(), "tip tx must succeed: {:?}", r.result);
}
