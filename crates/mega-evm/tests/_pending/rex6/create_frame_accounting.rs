//! REX6 CREATE-frame resource-accounting tests.
//!
//! Under REX6, `StateGrowthTracker` records +1 state growth for a `CREATE` only when the
//! deployment address is a net-new account; a pre-funded balance-only target already exists
//! and is not counted. Pre-REX6 records +1 unconditionally.
//!
//! This file also pins that a child-CREATE's creator nonce-bump account-info write is charged
//! to the parent frame's discardable lane under REX6, so the charge survives the child's revert
//! and is correctly attributed to the frame that owns the on-chain effect.

use alloy_primitives::{Address, Bytes, U256};
use mega_evm::{
    test_utils::{ErrorInjectingDatabase, MemoryDatabase},
    EmptyExternalEnv, EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
    MegaTransaction, ACCOUNT_INFO_WRITE_SIZE,
};
use revm::{
    context::{
        result::{ExecutionResult, ResultAndState},
        BlockEnv, ContextSetters, TxEnv,
    },
    handler::EvmTr,
    primitives::TxKind,
};

const CALLER: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xC0, 0xDE, 0x00, 0x01,
]);
const CALLER_BALANCE: u128 = 1_000_000_000_000_000_000; // 1 ETH
const TX_GAS_LIMIT: u64 = 5_000_000; // generous headroom for a trivial CREATE; not a bound under test

type TestEvm = MegaEvm<MemoryDatabase, revm::inspector::NoOpInspector, EmptyExternalEnv>;
type TestResult = ResultAndState<MegaHaltReason>;

/// Builds a configured `MegaEvm` for `spec` with the given `limits` and operator fees zeroed
/// (so only the accounting under test moves). Single source of truth for block/chain setup.
fn make_evm_with_limits(
    spec: MegaSpecId,
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
) -> TestEvm {
    let mut context = MegaContext::new(db, spec).with_tx_runtime_limits(limits);
    context.set_block(BlockEnv { gas_limit: 1_000_000_000, ..Default::default() });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    MegaEvm::new(context)
}

/// Builds a configured `MegaEvm` for `spec` with no tx runtime limits and operator fees zeroed.
/// Shared by every test in this file that doesn't need custom limits.
fn make_evm(spec: MegaSpecId, db: MemoryDatabase) -> TestEvm {
    make_evm_with_limits(spec, db, EvmTxRuntimeLimits::no_limits())
}

/// Init code that immediately STOPs (deploys empty runtime) — a trivial successful CREATE.
fn stop_init_code() -> Bytes {
    Bytes::from_static(&[0x00]) // STOP
}

fn run(spec: MegaSpecId, db: MemoryDatabase, init_code: Bytes) -> (TestResult, TestEvm) {
    let mut evm = make_evm(spec, db);

    let tx_env = TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        gas_limit: TX_GAS_LIMIT,
        gas_price: 0,
        data: init_code,
        value: U256::ZERO,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());

    let r = alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError");
    (r, evm)
}

/// The address a top-level CREATE from CALLER (nonce 0) deploys to.
fn first_create_address() -> Address {
    CALLER.create(0)
}

const OUTER_CREATOR: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xC0, 0xDE, 0x00, 0xFF,
]);

/// Bytecode: PUSH5 0x60006000fd; PUSH1 0; MSTORE; PUSH1 5; PUSH1 27; PUSH1 0; CREATE; POP; STOP.
/// The inner CREATE deploys init code [60 00 60 00 FD] = PUSH1 0; PUSH1 0; REVERT, which reverts,
/// so the nested CREATE frame reverts while this outer contract returns success.
fn outer_creator_code() -> Bytes {
    Bytes::from_static(&[
        0x64, 0x60, 0x00, 0x60, 0x00, 0xFD, // PUSH5 <reverting init code>
        0x60, 0x00, // PUSH1 0 (mem offset)
        0x52, // MSTORE
        0x60, 0x05, // PUSH1 5 (create length)
        0x60, 0x1b, // PUSH1 27 (create offset)
        0x60, 0x00, // PUSH1 0 (create value)
        0xf0, // CREATE
        0x50, // POP
        0x00, // STOP
    ])
}

#[test]
fn test_rex6_nested_create_revert_charges_creator_nonce_bump_to_parent() {
    let build_db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(CALLER_BALANCE))
            .account_code(OUTER_CREATOR, outer_creator_code())
    };
    let make_call = || {
        let mut tx = MegaTransaction::new(TxEnv {
            caller: CALLER,
            kind: TxKind::Call(OUTER_CREATOR),
            gas_limit: TX_GAS_LIMIT,
            gas_price: 0,
            ..Default::default()
        });
        tx.enveloped_tx = Some(Bytes::new());
        tx
    };
    let usage = |spec: MegaSpecId| {
        let mut evm = make_evm(spec, build_db());
        let r = alloy_evm::Evm::transact_raw(&mut evm, make_call());
        assert!(r.expect("ok").result.is_success(), "outer call must succeed (spec {spec:?})");
        let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
        usage
    };
    let rex5 = usage(MegaSpecId::REX5);
    let rex6 = usage(MegaSpecId::REX6);
    assert_eq!(
        rex6.data_size.saturating_sub(rex5.data_size),
        ACCOUNT_INFO_WRITE_SIZE,
        "REX6 must keep the creator nonce-bump account-info write (+{ACCOUNT_INFO_WRITE_SIZE}) on \
         the surviving parent (rex5={}, rex6={})",
        rex5.data_size,
        rex6.data_size,
    );
    assert_eq!(
        rex6.kv_updates.saturating_sub(rex5.kv_updates),
        1,
        "REX6 must keep the creator nonce-bump KV write (+1) on the surviving parent \
         (rex5={}, rex6={})",
        rex5.kv_updates,
        rex6.kv_updates,
    );
}

/// Init code that returns `code_len` bytes of zeros from `memory[0..code_len]`.
///
/// Layout: PUSH3 `code_len`; PUSH1 0; RETURN.
fn return_zeros_initcode(code_len: u32) -> Bytes {
    let bytes = code_len.to_be_bytes();
    let mut code = Vec::with_capacity(7);
    code.push(0x62); // PUSH3
    code.extend_from_slice(&bytes[1..]); // 3-byte big-endian length
    code.push(0x60); // PUSH1
    code.push(0x00);
    code.push(0xf3); // RETURN
    Bytes::from(code)
}

/// Runs a top-level CREATE under `spec` with `data_size_limit` as the tx data-size budget and a
/// constructor that RETURNs `code_len` runtime bytes, with `tx_gas_limit` available to the frame.
fn run_create_with_limits(
    spec: MegaSpecId,
    data_size_limit: u64,
    code_len: u32,
    tx_gas_limit: u64,
) -> TestResult {
    let db = MemoryDatabase::default().account_balance(CALLER, U256::from(CALLER_BALANCE));
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(data_size_limit);
    let mut evm = make_evm_with_limits(spec, db, limits);

    let tx_env = TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        gas_limit: tx_gas_limit,
        gas_price: 0,
        data: return_zeros_initcode(code_len),
        value: U256::ZERO,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx should not surface EVMError")
}

/// REX6: a DB failure while inspecting the CREATE target for the net-new state-growth check must
/// surface as an error, not be silently swallowed. A normal `MemoryDatabase` is infallible, so the
/// `inspect_account(created_address, ..)?` error branch is only exercised by injecting a DB read
/// failure here. The created-address read happens after the caller-nonce read, so failing on
/// `first_create_address()` lets the caller read succeed and trips exactly the created-address
/// inspect.
#[test]
fn test_rex6_create_net_new_inspect_db_error_surfaces() {
    let inner = MemoryDatabase::default().account_balance(CALLER, U256::from(CALLER_BALANCE));
    let mut db = ErrorInjectingDatabase::new(inner);
    db.fail_on_account = Some(first_create_address());

    let mut context = MegaContext::new(db, MegaSpecId::REX6);
    context.set_block(BlockEnv { gas_limit: 1_000_000_000, ..Default::default() });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);

    let tx_env = TxEnv {
        caller: CALLER,
        kind: TxKind::Create,
        gas_limit: TX_GAS_LIMIT,
        gas_price: 0,
        data: stop_init_code(),
        value: U256::ZERO,
        ..Default::default()
    };
    let mut tx = MegaTransaction::new(tx_env);
    tx.enveloped_tx = Some(Bytes::new());

    let res = alloy_evm::Evm::transact_raw(&mut evm, tx);
    assert!(res.is_err(), "DB error during REX6 CREATE net-new inspect must surface as Err");
}

/// Bytecode like [`outer_creator_code`] but with the reverting CREATE performed TWICE
/// (revert-then-retry). The creator's account-info write must be charged once, not once per
/// attempt: the first CREATE's parent-lane charge survives the child's revert (the nonce bump
/// does too), so the unwind must not re-arm the dedup flag.
fn outer_double_creator_code() -> Bytes {
    Bytes::from_static(&[
        0x64, 0x60, 0x00, 0x60, 0x00, 0xFD, // PUSH5 <reverting init code>
        0x60, 0x00, // PUSH1 0 (mem offset)
        0x52, // MSTORE
        0x60, 0x05, // PUSH1 5 (create length)
        0x60, 0x1b, // PUSH1 27 (create offset)
        0x60, 0x00, // PUSH1 0 (create value)
        0xf0, // CREATE (#1 — reverts)
        0x50, // POP
        0x60, 0x05, // PUSH1 5
        0x60, 0x1b, // PUSH1 27
        0x60, 0x00, // PUSH1 0
        0xf0, // CREATE (#2 — reverts again)
        0x50, // POP
        0x00, // STOP
    ])
}

/// REX6: a reverted-then-retried nested CREATE charges the creator's account-info write exactly
/// once. Before the unwind gate, `pop_frame_unwind_parent` reset the parent's dedup flag while
/// the parent-lane charge survived — the retry charged the same creator update again.
#[test]
fn test_rex6_nested_create_revert_then_retry_charges_creator_once() {
    let build_db = || {
        MemoryDatabase::default()
            .account_balance(CALLER, U256::from(CALLER_BALANCE))
            .account_code(OUTER_CREATOR, outer_double_creator_code())
    };
    let make_call = || {
        let mut tx = MegaTransaction::new(TxEnv {
            caller: CALLER,
            kind: TxKind::Call(OUTER_CREATOR),
            gas_limit: TX_GAS_LIMIT,
            gas_price: 0,
            ..Default::default()
        });
        tx.enveloped_tx = Some(Bytes::new());
        tx
    };
    let usage = |spec: MegaSpecId| {
        let mut evm = make_evm(spec, build_db());
        let r = alloy_evm::Evm::transact_raw(&mut evm, make_call());
        assert!(r.expect("ok").result.is_success(), "outer call must succeed (spec {spec:?})");
        let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
        usage
    };
    let rex5 = usage(MegaSpecId::REX5);
    let rex6 = usage(MegaSpecId::REX6);
    assert_eq!(
        rex6.data_size.saturating_sub(rex5.data_size),
        ACCOUNT_INFO_WRITE_SIZE,
        "double reverted CREATE must charge the creator write ONCE under REX6 \
         (rex5={}, rex6={})",
        rex5.data_size,
        rex6.data_size,
    );
    assert_eq!(
        rex6.kv_updates.saturating_sub(rex5.kv_updates),
        1,
        "double reverted CREATE must charge the creator KV update ONCE under REX6 \
         (rex5={}, rex6={})",
        rex5.kv_updates,
        rex6.kv_updates,
    );
}
