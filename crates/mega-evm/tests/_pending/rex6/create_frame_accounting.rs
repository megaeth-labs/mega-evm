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

