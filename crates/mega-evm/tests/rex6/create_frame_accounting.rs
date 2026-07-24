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

/// REX6: a CREATE deploying to an address that ALREADY exists (pre-funded balance-only) must
/// NOT record a state-growth account-creation event; a fresh-address CREATE still records +1.
#[test]
fn test_rex6_create_to_prefunded_address_records_no_state_growth() {
    // --- fresh address: no pre-existing account ---
    let db_fresh = MemoryDatabase::default().account_balance(CALLER, U256::from(CALLER_BALANCE));
    let (res_fresh, evm_fresh) = run(MegaSpecId::REX6, db_fresh, stop_init_code());
    assert!(res_fresh.result.is_success(), "fresh CREATE must succeed: {:?}", res_fresh.result);
    let growth_fresh = evm_fresh.ctx_ref().additional_limit.borrow().get_usage().state_growth;

    // --- prefunded address: target already has a balance-only account ---
    let db_prefunded = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_balance(first_create_address(), U256::from(1u64));
    let (res_pre, evm_pre) = run(MegaSpecId::REX6, db_prefunded, stop_init_code());
    assert!(res_pre.result.is_success(), "prefunded CREATE must succeed: {:?}", res_pre.result);
    let growth_pre = evm_pre.ctx_ref().additional_limit.borrow().get_usage().state_growth;

    assert_eq!(
        growth_fresh.saturating_sub(growth_pre),
        1,
        "REX6 CREATE must record +1 state_growth only for a net-new address \
         (fresh={growth_fresh}, prefunded={growth_pre})",
    );
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

/// A CREATE whose Mega code-deposit-storage charge runs out of gas while the frame's data-size
/// budget is simultaneously exceeded is absorbed into the frame-local `Revert` and its unspent
/// child gas is returned to the caller — identically on REX6 and pre-REX6. The frame-local absorb
/// treats an already-failed CREATE the same as a successful-but-over-limit one, so REX5 and REX6
/// produce the same result class and the same `gas_used` (no spec divergence on this path).
///
/// Setup: the constructor RETURNs an 8_000-byte runtime blob. The Mega code-deposit-storage charge
/// (`CODEDEPOSIT_STORAGE_GAS` per byte) far exceeds the 400_000-gas frame budget, so the frame
/// returns `OutOfGas`. At the same time the runtime blob overshoots the tiny `tx_data_size_limit`
/// (2000 bytes), so the frame-local data-size limit is also exceeded — the exact edge where the
/// absorb fires.
#[test]
fn test_rex6_create_oog_over_limit_matches_rex5_frozen() {
    // Runtime blob large enough that the code-deposit-storage charge cannot fit in `GAS`, and
    // large enough to overshoot DATA_SIZE_LIMIT so the frame-local data-size limit also trips.
    const CODE_LEN: u32 = 8_000;
    const DATA_SIZE_LIMIT: u64 = 2_000;
    // Frame gas budget far below the code-deposit-storage charge (CODE_LEN * 10_000), so the
    // charge runs OutOfGas; large enough for the constructor + intrinsic to run first.
    const GAS: u64 = 400_000;

    let r5 = run_create_with_limits(MegaSpecId::REX5, DATA_SIZE_LIMIT, CODE_LEN, GAS);
    let r6 = run_create_with_limits(MegaSpecId::REX6, DATA_SIZE_LIMIT, CODE_LEN, GAS);

    // Both specs absorb the failed CREATE into the frame-local Revert.
    assert!(
        matches!(r5.result, ExecutionResult::Revert { .. }),
        "REX5 absorbs the OutOfGas CREATE into a frame-local Revert: {:?}",
        r5.result,
    );
    assert!(
        matches!(r6.result, ExecutionResult::Revert { .. }),
        "REX6 absorbs the OutOfGas CREATE into a frame-local Revert identically to REX5: {:?}",
        r6.result,
    );

    // Same result class → same returned gas; no REX6 burn-vs-refund divergence on this path.
    assert_eq!(
        r6.result.gas_used(),
        r5.result.gas_used(),
        "REX6 must return the absorbed CREATE's gas identically to REX5 \
         (rex5_gas_used={}, rex6_gas_used={})",
        r5.result.gas_used(),
        r6.result.gas_used(),
    );
}

/// Pre-REX6 freeze: under REX5 a CREATE records state growth unconditionally, so deploying to a
/// pre-funded (already-existing) address records the same state growth as deploying to a fresh
/// one — the net-new distinction is REX6-only.
#[test]
fn test_rex5_freeze_create_state_growth_unconditional() {
    let db_fresh = MemoryDatabase::default().account_balance(CALLER, U256::from(CALLER_BALANCE));
    let (rf, ef) = run(MegaSpecId::REX5, db_fresh, stop_init_code());
    assert!(rf.result.is_success(), "REX5 fresh CREATE must succeed");
    let fresh = ef.ctx_ref().additional_limit.borrow().get_usage().state_growth;

    let db_pre = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(CALLER_BALANCE))
        .account_balance(first_create_address(), U256::from(1u64));
    let (rp, ep) = run(MegaSpecId::REX5, db_pre, stop_init_code());
    assert!(rp.result.is_success(), "REX5 prefunded CREATE must succeed");
    let pre = ep.ctx_ref().additional_limit.borrow().get_usage().state_growth;

    assert_eq!(
        fresh, pre,
        "REX5 must record CREATE state_growth unconditionally (fresh={fresh}, prefunded={pre})",
    );
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
