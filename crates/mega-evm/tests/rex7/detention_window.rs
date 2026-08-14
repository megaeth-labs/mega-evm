//! REX7 detention-mark timing: a frame that cannot afford the pre-load fees does not mark.
//!
//! revm 40 charges CALL-family static / value fees (and EXTCODECOPY's copy cost) before the
//! target account is loaded. Through REX6 that order is a frozen replay window: historical
//! revm 27 executions marked first, and `debug_check_frozen_detention_window` panics when a
//! debug replay hits the window (`tests/rex4/frozen_window_tripwire.rs`). REX7 has no history
//! and specifies the new order: the mark is produced when the target account is loaded, so
//! these shapes must stay unmarked and the tripwire — now spec-gated — must stay silent.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaSpecId, MegaTransaction, MegaTransactionNew as _,
};
use revm::{
    bytecode::opcode::{CALL, EXTCODECOPY},
    context::{tx::TxEnvBuilder, BlockEnv, TxEnv},
    handler::EvmTr,
};

const CALLER: Address = address!("0000000000000000000000000000000000320000");
const OUTER: Address = address!("0000000000000000000000000000000000320001");
const INNER: Address = address!("0000000000000000000000000000000000320002");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000320099");

fn append_call(builder: BytecodeBuilder, target: Address, gas: u64, value: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_number(value)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

fn inner_window_call(target: Address, value: u64) -> Bytes {
    append_call(BytecodeBuilder::default(), target, 0, value).build()
}

fn inner_window_extcodecopy(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0x8000_u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .push_address(target)
        .append(EXTCODECOPY)
        .build()
}

fn window_db(inner_code: Bytes, inner_budget: u64) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_u64))
        .account_code(
            OUTER,
            append_call(BytecodeBuilder::default(), INNER, inner_budget, 0).build(),
        )
        .account_code(INNER, inner_code)
}

fn transact_rex7(db: &mut MemoryDatabase, tx: TxEnv) -> bool {
    let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
    let mut context = MegaContext::new(db, MegaSpecId::REX7).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = MegaTransaction::new(tx);
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx).expect("tx must execute");
    let tracker = evm.ctx_ref().volatile_data_tracker.borrow();
    let marked = tracker.has_accessed_beneficiary_balance();
    drop(tracker);
    marked
}

fn default_tx() -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(OUTER).gas_limit(1_000_000).build_fill()
}

/// Static-charge window: INNER reaches CALL holding fewer than 100 gas, so the load never runs.
#[test]
fn test_rex7_underfunded_call_to_beneficiary_does_not_mark() {
    let mut db = window_db(inner_window_call(BENEFICIARY, 0), 80);
    let marked = transact_rex7(&mut db, default_tx());
    assert!(!marked, "REX7 specifies charge-before-load: an underfunded CALL must not mark");
}

/// Value-transfer window: the frame affords the 100 static charge but not the 9,000 transfer cost.
#[test]
fn test_rex7_underfunded_value_call_to_beneficiary_does_not_mark() {
    let mut db = window_db(inner_window_call(BENEFICIARY, 1), 800);
    let marked = transact_rex7(&mut db, default_tx());
    assert!(!marked, "REX7 specifies charge-before-load: an underfunded value CALL must not mark");
}

/// EXTCODECOPY window: the copy cost sits before the load, so a poor frame never marks.
#[test]
fn test_rex7_underfunded_extcodecopy_of_beneficiary_does_not_mark() {
    let mut db = window_db(inner_window_extcodecopy(BENEFICIARY), 100);
    let marked = transact_rex7(&mut db, default_tx());
    assert!(!marked, "REX7 specifies charge-before-load: an underfunded EXTCODECOPY must not mark");
}
