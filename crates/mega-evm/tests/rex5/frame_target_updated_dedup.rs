//! Tests for Rex5's caller-account deduplication within a parent frame.
//!
//! Pre-Rex5, `DataSizeTracker::before_frame_init` and `KVUpdateTracker::before_frame_init`
//! checked the parent frame's `target_updated` flag to decide whether to charge the parent's
//! caller-account update for a value-transferring CALL / CALLCODE or CREATE, but never set
//! the flag to `true` after charging. As a result, every subsequent value-transferring call
//! or create from the same parent frame re-charged the caller account, overcounting data
//! size and KV updates.
//!
//! Rex5 marks the parent's flag after charging, so subsequent operations from the same
//! parent frame no longer double-charge the caller account. Per-target charges for distinct
//! callee / created addresses are unchanged. Pre-Rex5 specs keep their existing (frozen)
//! overcounting behavior for backward compatibility.

use std::convert::Infallible;

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
    MegaTransactionError, ACCOUNT_INFO_WRITE_SIZE, BASE_TX_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, GAS, INVALID, POP, PUSH0, PUSH1, STOP},
    context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
        TxEnv,
    },
    handler::EvmTr,
};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const CONTRACT: Address = address!("0000000000000000000000000000000000100002");
const CONTRACT2: Address = address!("0000000000000000000000000000000000100003");

/// Runs the given `tx` on `spec` with unlimited data/KV budgets and returns the
/// recorded data size and KV update counts.
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<(ResultAndState<MegaHaltReason>, u64, u64), EVMError<Infallible, MegaTransactionError>>
{
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(
        EvmTxRuntimeLimits::no_limits()
            .with_tx_data_size_limit(u64::MAX)
            .with_tx_kv_updates_limit(u64::MAX),
    );
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    let r = alloy_evm::Evm::transact_raw(&mut evm, tx)?;

    let ctx = evm.ctx_ref();
    let usage = ctx.additional_limit.borrow().get_usage();
    Ok((r, usage.data_size, usage.kv_updates))
}

fn default_tx(to: Address) -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(to).gas_limit(100_000_000).build_fill()
}

/// Appends `CALL(gas=GAS, target, value=1, args=[], ret=[])`, leaving the CALL
/// success flag on the stack.
fn append_value_call(builder: BytecodeBuilder, target: Address) -> BytecodeBuilder {
    builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0]) // retLen, retOff, argsLen, argsOff
        .append(PUSH1)
        .append(1u8) // value
        .push_address(target)
        .append(GAS)
        .append(CALL)
}

/// Appends a CREATE with value=1 and an empty init code (immediately returns).
fn append_value_create(builder: BytecodeBuilder) -> BytecodeBuilder {
    builder
        .append_many([PUSH0, PUSH0]) // initLen, initOff (empty init code)
        .append(PUSH1)
        .append(1u8) // value
        .append(CREATE)
}

/// Base intrinsic data size for a TX with no calldata / access-list / 7702 entries:
/// `BASE_TX_SIZE` + caller account write.
fn intrinsic_data_size() -> u64 {
    BASE_TX_SIZE + ACCOUNT_INFO_WRITE_SIZE
}

// ============================================================================
// Two consecutive value-transferring CALLs
// ============================================================================

/// CALLEE does two value-transferring CALLs to two distinct targets.
///
/// Expected charges for the caller (= CALLEE parent frame) update:
/// - Rex5: charged once (deduplicated)
/// - Pre-Rex5 (Rex4): charged twice (overcounting bug)
///
/// The two distinct target accounts are always charged separately.
fn two_value_calls_code() -> Bytes {
    let mut builder = BytecodeBuilder::default();
    builder = append_value_call(builder, CONTRACT).append(POP);
    builder = append_value_call(builder, CONTRACT2).append(POP);
    builder.append(STOP).build()
}

#[test]
fn test_rex5_two_value_calls_dedup_parent_kv() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, two_value_calls_code());

    let (res, data_size, kv_updates) =
        transact(MegaSpecId::REX5, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success());

    // KV updates: 1 (caller nonce) + 1 (CALLEE, counted once) + 1 (CONTRACT) + 1 (CONTRACT2) = 4.
    assert_eq!(kv_updates, 4, "Rex5 must deduplicate the CALLEE (parent) update across both calls");
    // Data size: intrinsic + 3 discardable account writes (CALLEE once, CONTRACT, CONTRACT2).
    assert_eq!(data_size, intrinsic_data_size() + 3 * ACCOUNT_INFO_WRITE_SIZE);
}

#[test]
fn test_rex4_two_value_calls_overcount_preserved() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, two_value_calls_code());

    let (res, data_size, kv_updates) =
        transact(MegaSpecId::REX4, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success());

    // Pre-Rex5 behavior: the CALLEE parent update is charged on BOTH calls.
    // KV updates: 1 (caller nonce) + 2 (CALLEE, double-charged) + 1 (CONTRACT) + 1 (CONTRACT2) = 5.
    assert_eq!(kv_updates, 5, "Rex4 must preserve the pre-Rex5 overcounting behavior");
    assert_eq!(data_size, intrinsic_data_size() + 4 * ACCOUNT_INFO_WRITE_SIZE);
}

// ============================================================================
// CREATE followed by value-transferring CALL
// ============================================================================

/// CALLEE does a CREATE followed by a value-transferring CALL, both from the same
/// parent frame. The parent update should only be charged once under Rex5.
fn create_then_call_code() -> Bytes {
    let mut builder = BytecodeBuilder::default();
    builder = append_value_create(builder).append(POP);
    builder = append_value_call(builder, CONTRACT).append(POP);
    builder.append(STOP).build()
}

#[test]
fn test_rex5_create_then_call_dedup_parent_kv() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, create_then_call_code());

    let (res, _data_size, kv_updates) =
        transact(MegaSpecId::REX5, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success());

    // KV updates: 1 (caller nonce) + 1 (CALLEE parent, counted once) + 1 (created account)
    //           + 1 (CONTRACT) = 4.
    assert_eq!(kv_updates, 4, "Rex5 must deduplicate CALLEE update across CREATE and CALL");
}

#[test]
fn test_rex4_create_then_call_overcount_preserved() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, create_then_call_code());

    let (res, _data_size, kv_updates) =
        transact(MegaSpecId::REX4, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success());

    // Pre-Rex5 behavior: CALLEE parent charged both by CREATE and by the subsequent CALL.
    // KV updates: 1 (caller nonce) + 2 (CALLEE, double-charged) + 1 (created) + 1 (CONTRACT) = 5.
    assert_eq!(kv_updates, 5, "Rex4 must preserve the pre-Rex5 overcounting behavior");
}

// ============================================================================
// Two consecutive CREATEs from the same parent
// ============================================================================

/// CALLEE does two CREATEs from the same parent frame. The parent update should
/// only be charged once under Rex5 (created accounts still counted per-CREATE).
fn two_creates_code() -> Bytes {
    let mut builder = BytecodeBuilder::default();
    builder = append_value_create(builder).append(POP);
    builder = append_value_create(builder).append(POP);
    builder.append(STOP).build()
}

#[test]
fn test_rex5_two_creates_dedup_parent_kv() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, two_creates_code());

    let (res, _data_size, kv_updates) =
        transact(MegaSpecId::REX5, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success());

    // KV updates: 1 (caller nonce) + 1 (CALLEE, once) + 2 (two distinct created accounts) = 4.
    assert_eq!(kv_updates, 4, "Rex5 must deduplicate CALLEE across both CREATEs");
}

#[test]
fn test_rex4_two_creates_overcount_preserved() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, two_creates_code());

    let (res, _data_size, kv_updates) =
        transact(MegaSpecId::REX4, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success());

    // Pre-Rex5: CALLEE parent charged on BOTH CREATEs.
    // KV updates: 1 (caller nonce) + 2 (CALLEE, double) + 2 (created accounts) = 5.
    assert_eq!(kv_updates, 5, "Rex4 must preserve the pre-Rex5 overcounting behavior");
}

// ============================================================================
// Reverted child still discards its accumulated charges
// ============================================================================

/// CALLEE CALLs CONTRACT with value. CONTRACT is an INVALID instruction that
/// halts, so the child frame's discardable usage must be dropped on revert,
/// regardless of spec.
fn call_that_reverts_code() -> Bytes {
    let mut builder = BytecodeBuilder::default();
    builder = append_value_call(builder, CONTRACT).append(POP);
    builder.append(STOP).build()
}

#[test]
fn test_rex5_reverted_child_drops_discardable_charges() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, call_that_reverts_code())
        .account_code(CONTRACT, Bytes::from_static(&[INVALID]));

    let (res, data_size, kv_updates) =
        transact(MegaSpecId::REX5, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success(), "outer TX succeeds even though child reverts");

    // The child's discardable charges (CALLEE parent update + CONTRACT target update) are
    // dropped on revert. Only the persistent caller nonce survives.
    assert_eq!(kv_updates, 1, "Reverted child's discardable KV updates must be dropped");
    assert_eq!(data_size, intrinsic_data_size(), "Reverted child's data size must be dropped");
}

#[test]
fn test_rex4_reverted_child_drops_discardable_charges() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, call_that_reverts_code())
        .account_code(CONTRACT, Bytes::from_static(&[INVALID]));

    let (res, data_size, kv_updates) =
        transact(MegaSpecId::REX4, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success(), "outer TX succeeds even though child reverts");

    // Existing revert behavior must stay unchanged pre-Rex5.
    assert_eq!(kv_updates, 1);
    assert_eq!(data_size, intrinsic_data_size());
}

// ============================================================================
// Reverted first child followed by a successful second child (revert-then-retry)
// ============================================================================

/// CALLEE first CALLs CONTRACT with value (reverts), then CALLs CONTRACT2 with value (succeeds).
/// Under Rex5, the parent (CALLEE) account update should be charged exactly once — from the
/// second (successful) call, not zero times. Without the flag-reset-on-revert fix, Rex5 would
/// set `target_updated` on the first call, see it still set on the second, and charge 0 times.
fn revert_then_succeed_code() -> Bytes {
    let mut builder = BytecodeBuilder::default();
    builder = append_value_call(builder, CONTRACT).append(POP); // CONTRACT reverts
    builder = append_value_call(builder, CONTRACT2).append(POP); // CONTRACT2 succeeds
    builder.append(STOP).build()
}

#[test]
fn test_rex5_reverted_first_child_flag_reset_allows_second_charge() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, revert_then_succeed_code())
        .account_code(CONTRACT, Bytes::from_static(&[INVALID])); // CONTRACT2 has no code → EOA

    let (res, data_size, kv_updates) =
        transact(MegaSpecId::REX5, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success());

    // KV updates: 1 (caller nonce) + 1 (CALLEE parent, charged on successful 2nd call)
    //           + 1 (CONTRACT2) = 3.
    // The first call's charges (CALLEE + CONTRACT) are dropped on revert, and the
    // target_updated flag is reset, so the second call correctly re-charges CALLEE.
    assert_eq!(
        kv_updates, 3,
        "Rex5 must charge CALLEE once after revert-then-retry (not undercount to 0)"
    );
    assert_eq!(data_size, intrinsic_data_size() + 2 * ACCOUNT_INFO_WRITE_SIZE);
}

#[test]
fn test_rex4_reverted_first_child_flag_not_set() {
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(10_000_000))
        .account_balance(CALLEE, U256::from(10_000_000))
        .account_code(CALLEE, revert_then_succeed_code())
        .account_code(CONTRACT, Bytes::from_static(&[INVALID]));

    let (res, _data_size, kv_updates) =
        transact(MegaSpecId::REX4, &mut db, default_tx(CALLEE)).unwrap();
    assert!(res.result.is_success());

    // Pre-Rex5: flag is never set, so both calls charge the parent independently.
    // KV updates: 1 (caller nonce) + 1 (CALLEE from 1st call, dropped on revert)
    //           + 1 (CALLEE from 2nd call) + 1 (CONTRACT2) = 3.
    // Note: the first call's CALLEE charge is dropped on revert, so Rex4 also gives 3 here.
    // The overcounting in Rex4 manifests when BOTH calls succeed (tested separately above).
    assert_eq!(kv_updates, 3);
}
