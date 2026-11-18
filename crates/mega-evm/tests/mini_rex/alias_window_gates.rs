//! The `MegaHardfork::MiniRex1` window is a behavior rollback on mainnet:
//! `MegaSpecId::MINI_REX_1::behavior()` is `EQUIVALENCE`, so the rung executes revm's stock
//! mainnet table with the `MINI_REX` gates *off* (`MegaSpecId::is_enabled` projects both sides
//! through `behavior()`). Blocks in that window are replayed, so the observation must be pinned
//! here rather than only at `EQUIVALENCE`.
//!
//! Everything the `MINI_REX` bookkeeping touches is asserted to be inert in this window: a
//! transaction that writes storage, emits a log, spans two frames and deploys a contract leaves
//! every metered dimension at zero, exposes no volatile-data info, and consumes exactly the gas
//! the same transaction consumes at `EQUIVALENCE`.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EmptyExternalEnv, MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::*,
    context::{BlockEnv, TxEnv},
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::TxKind,
};

const EOA_CALLER: Address = address!("0000000000000000000000000000000000100000");
const CALLEE: Address = address!("0000000000000000000000000000000000100001");
const INNER: Address = address!("0000000000000000000000000000000000100002");
const BENEFICIARY: Address = address!("00000000000000000000000000000000000BEEF1");

const GAS_LIMIT: u64 = 10_000_000;
const INITIAL_BALANCE: u64 = 1_000_000_000;

fn build_evm<DB: revm::Database + core::fmt::Debug>(
    db: DB,
    spec: MegaSpecId,
) -> MegaEvm<DB, NoOpInspector, EmptyExternalEnv> {
    let mut context = MegaContext::new(db, spec).with_block(BlockEnv {
        beneficiary: BENEFICIARY,
        number: U256::from(10),
        basefee: 0,
        ..Default::default()
    });
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    MegaEvm::new(context)
}

/// CALLEE writes a slot, calls INNER (which writes a slot and emits LOG0), then CREATEs a
/// one-byte contract. Covers the frame-entry, frame-return, storage-gas, code-deposit and
/// volatile-access paths in a single transaction.
fn scenario_db() -> MemoryDatabase {
    // INNER writes a slot, emits LOG0 and returns the 32-byte word 0x2a.
    let inner_code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(2))
        .push_number(0u64)
        .push_number(0u64)
        .append(LOG0)
        .push_number(0x2au64)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(32u64)
        .push_number(0u64)
        .append(RETURN)
        .build();
    // CALLEE writes a slot, calls INNER and stores the call's success flag at word 0, then
    // CREATEs a contract and stores the returned address at word 32, returning both words. The
    // success flag and the address therefore reach the caller instead of being discarded.
    let callee_code = BytecodeBuilder::default()
        .sstore(U256::from(3), U256::from(4))
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_number(0u64)
        .push_address(INNER)
        .push_number(100_000u64)
        .append(CALL)
        .push_number(0u64)
        .append(MSTORE)
        .push_number(1u64)
        .push_number(0u64)
        .push_number(0u64)
        .append(CREATE)
        .push_number(32u64)
        .append(MSTORE)
        .push_number(64u64)
        .push_number(0u64)
        .append(RETURN)
        .build();

    MemoryDatabase::default()
        .account_balance(EOA_CALLER, U256::from(INITIAL_BALANCE))
        .account_balance(CALLEE, U256::from(INITIAL_BALANCE))
        .account_code(CALLEE, callee_code)
        .account_code(INNER, inner_code)
}

fn scenario_tx() -> MegaTransaction {
    let mut tx = MegaTransaction::new(TxEnv {
        caller: EOA_CALLER,
        kind: TxKind::Call(CALLEE),
        gas_limit: GAS_LIMIT,
        gas_price: 0,
        data: Bytes::from(vec![0xAAu8; 64]),
        ..Default::default()
    });
    tx.enveloped_tx = Some(Bytes::new());
    tx
}

/// `(success, gas used, output length)` for the scenario transaction at `spec`.
fn observe(spec: MegaSpecId) -> (bool, u64, Vec<u8>) {
    let mut evm = build_evm(scenario_db(), spec);
    let result = alloy_evm::Evm::transact_raw(&mut evm, scenario_tx()).expect("tx must execute");
    (
        result.result.is_success(),
        result.result.tx_gas_used(),
        result.result.output().map(|out| out.to_vec()).unwrap_or_default(),
    )
}

/// The `MiniRex1` window must consume exactly the gas `EQUIVALENCE` consumes for the same
/// transaction. Any `MINI_REX` gate that the mutant turns on inside this window shows up here as
/// a gas or outcome difference.
#[test]
fn test_mini_rex_1_window_matches_equivalence_observation() {
    let at_equivalence = observe(MegaSpecId::EQUIVALENCE);
    let at_window = observe(MegaSpecId::MINI_REX_1);
    assert_eq!(
        at_window, at_equivalence,
        "the MiniRex1 window must observe exactly what EQUIVALENCE observes"
    );
    assert!(at_window.0, "scenario transaction must succeed in the window");
    assert_eq!(
        at_window.2.len(),
        64,
        "the callee must return the call flag and the created address"
    );
    assert_eq!(
        U256::from_be_slice(&at_window.2[..32]),
        U256::from(1u64),
        "the nested call must report success to the caller"
    );
    assert_ne!(
        U256::from_be_slice(&at_window.2[32..]),
        U256::ZERO,
        "the CREATE must report a deployed address to the caller"
    );
}

/// No metered dimension may move in the window: the stock table installs no wrapper that feeds
/// the trackers, so running the bookkeeping there cannot record anything.
#[test]
fn test_mini_rex_1_window_leaves_the_trackers_at_zero() {
    let mut evm = build_evm(scenario_db(), MegaSpecId::MINI_REX_1);
    let result = alloy_evm::Evm::transact_raw(&mut evm, scenario_tx()).expect("tx must execute");
    assert!(result.result.is_success(), "got {:?}", result.result);

    let usage = evm.ctx_ref().additional_limit.borrow().get_usage();
    assert_eq!(usage.compute_gas, 0, "the window must not meter compute gas");
    assert_eq!(usage.data_size, 0, "the window must not meter data size");
    assert_eq!(usage.kv_updates, 0, "the window must not meter KV updates");
    assert_eq!(usage.state_growth, 0, "the window must not meter state growth");
    assert!(
        evm.ctx_ref().volatile_data_tracker.borrow().get_volatile_data_info().is_none(),
        "the window must not report volatile-data info"
    );
}
