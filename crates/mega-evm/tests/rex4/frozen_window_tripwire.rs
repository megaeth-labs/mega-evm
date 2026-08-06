//! Coverage for the frozen-detention-window tripwire (debug builds only).
//!
//! revm 27 loaded a CALL-family (and EXTCODECOPY) target before charging the opcode's own
//! costs, so a frame that went out of gas on those charges had already marked beneficiary
//! access; revm 40 charges first, so the same frame halts unmarked. Release builds accept
//! the revm 40 order and the full-history replay gate proves no historical transaction sits
//! in that window — `debug_check_frozen_detention_window` is the tripwire such a replay runs
//! with. These tests construct one transaction per window shape and pin that the tripwire
//! fires there (`#[should_panic]`) and stays silent on the neighbouring shapes.
//!
//! Every scenario uses two contracts: `OUTER` (ample gas) gives `INNER` an exact gas budget
//! via CALL, and `INNER` executes the guarded opcode with that budget — putting the frame in
//! or out of the window deterministically without depending on intrinsic-gas arithmetic.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    alloy_op_evm::{OpTx, OpTxError},
    op_revm::OpTransaction,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    MegaContext, MegaEvm, MegaHaltReason, MegaSpecId,
};
use revm::{
    bytecode::opcode::*,
    context::{
        result::{EVMError, ResultAndState},
        tx::TxEnvBuilder,
        BlockEnv, TxEnv,
    },
    database::AccountState,
    state::Bytecode,
};
use std::convert::Infallible;

const CALLER: Address = address!("0000000000000000000000000000000000410000");
const OUTER: Address = address!("0000000000000000000000000000000000410001");
const INNER: Address = address!("0000000000000000000000000000000000410002");
/// An EIP-7702 delegator whose code points at the beneficiary.
const DELEGATOR: Address = address!("0000000000000000000000000000000000410003");
/// A plain address that is not the beneficiary.
const UNRELATED: Address = address!("0000000000000000000000000000000000410004");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000410099");

/// Appends a CALL to `target` forwarding `gas` with the given `value`.
fn append_call(builder: BytecodeBuilder, target: Address, gas: u64, value: u64) -> BytecodeBuilder {
    builder
        .push_number(0_u64) // retSize
        .push_number(0_u64) // retOffset
        .push_number(0_u64) // argsSize
        .push_number(0_u64) // argsOffset
        .push_number(value)
        .push_address(target)
        .push_number(gas)
        .append(CALL)
}

/// `INNER` bytecode: a CALL to `target` with `value`. With the budgets used below the frame
/// reaches the CALL with well under its pre-load charges, so the CALL halts out-of-gas
/// before revm loads (and the host marks) the target.
fn inner_window_call(target: Address, value: u64) -> Bytes {
    append_call(BytecodeBuilder::default(), target, 0, value).build()
}

/// `INNER` bytecode: EXTCODECOPY of `target` whose copy cost (1024 words) exceeds any budget
/// used below, so the copy-cost charge — which revm 40 places before the load — halts the
/// frame out of gas.
fn inner_window_extcodecopy(target: Address) -> Bytes {
    BytecodeBuilder::default()
        .push_number(0x8000_u64) // len: 32768 bytes = 1024 words = 3072 gas of copy cost
        .push_number(0_u64) // codeOffset
        .push_number(0_u64) // memOffset
        .push_address(target)
        .append(EXTCODECOPY)
        .build()
}

/// Executes a transaction from `CALLER` to `OUTER` under `spec` with `BENEFICIARY` as the
/// block beneficiary.
fn transact(
    spec: MegaSpecId,
    db: &mut MemoryDatabase,
    tx: TxEnv,
) -> Result<ResultAndState<MegaHaltReason>, EVMError<Infallible, OpTxError>> {
    let block = BlockEnv { beneficiary: BENEFICIARY, ..Default::default() };
    let mut context = MegaContext::new(db, spec).with_block(block);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(0));
        chain.operator_fee_constant = Some(U256::from(0));
    });
    let mut evm = MegaEvm::new(context);
    let mut tx = OpTx(OpTransaction::new(tx));
    tx.enveloped_tx = Some(Bytes::new());
    alloy_evm::Evm::transact_raw(&mut evm, tx)
}

fn default_tx() -> TxEnv {
    TxEnvBuilder::default().caller(CALLER).call(OUTER).gas_limit(1_000_000).build_fill()
}

/// A database where `OUTER` hands `inner_budget` gas to `INNER`, whose code is `inner_code`.
fn window_db(inner_code: Bytes, inner_budget: u64) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_u64))
        .account_code(
            OUTER,
            append_call(BytecodeBuilder::default(), INNER, inner_budget, 0).build(),
        )
        .account_code(INNER, inner_code)
}

/// The static-charge window: `INNER` reaches its CALL with fewer than the 100 gas the
/// wrapper charges before revm's body runs, so the frame halts before the load. revm 27
/// charged that cost after loading (and marking) the target, so the tripwire must fire.
#[test]
#[should_panic(expected = "frozen detention window hit")]
fn test_tripwire_fires_on_static_charge_window_call_to_beneficiary() {
    let mut db = window_db(inner_window_call(BENEFICIARY, 0), 80);
    let _ = transact(MegaSpecId::REX4, &mut db, default_tx());
}

/// The value-transfer window: the frame affords the 100 static charge but not the 9,000
/// value-transfer cost, which revm 40 charges before the load and revm 27 after it.
#[test]
#[should_panic(expected = "frozen detention window hit")]
fn test_tripwire_fires_on_value_transfer_window_call_to_beneficiary() {
    let mut db = window_db(inner_window_call(BENEFICIARY, 1), 800);
    let _ = transact(MegaSpecId::REX4, &mut db, default_tx());
}

/// The EXTCODECOPY window: the copy cost sits before the load in revm 40's body and after it
/// in revm 27's.
#[test]
#[should_panic(expected = "frozen detention window hit")]
fn test_tripwire_fires_on_extcodecopy_window_of_beneficiary() {
    let mut db = window_db(inner_window_extcodecopy(BENEFICIARY), 100);
    let _ = transact(MegaSpecId::REX4, &mut db, default_tx());
}

/// Installs `0xef0100 || delegate_to` at `address`, mirroring what revm's
/// `apply_eip7702_auth_list` does for Type 4 transactions.
fn set_eip7702_delegation(db: &mut MemoryDatabase, address: Address, delegate_to: Address) {
    let bytecode = Bytecode::new_eip7702(delegate_to);
    let code_hash = bytecode.hash_slow();
    let account = db.load_account(address).unwrap();
    account.info.code = Some(bytecode);
    account.info.code_hash = code_hash;
    account.account_state = AccountState::None;
}

/// From REX6 a CALL's one-hop EIP-7702 delegate marks like the raw target, so the tripwire
/// resolves the delegate: a window CALL to a delegator pointing at the beneficiary fires.
#[test]
#[should_panic(expected = "frozen detention window hit")]
fn test_tripwire_fires_on_window_call_to_delegator_of_beneficiary_from_rex6() {
    let mut db = window_db(inner_window_call(DELEGATOR, 0), 80);
    set_eip7702_delegation(&mut db, DELEGATOR, BENEFICIARY);
    let _ = transact(MegaSpecId::REX6, &mut db, default_tx());
}

/// Control: the same delegator shape stays silent below REX6, where the delegate hop never
/// marked — the raw operand is not the beneficiary, so there is nothing to lose.
#[test]
fn test_tripwire_silent_on_window_call_to_delegator_of_beneficiary_below_rex6() {
    let mut db = window_db(inner_window_call(DELEGATOR, 0), 80);
    set_eip7702_delegation(&mut db, DELEGATOR, BENEFICIARY);
    let result = transact(MegaSpecId::REX5, &mut db, default_tx()).expect("tx must execute");
    assert!(result.result.is_success(), "outer frame absorbs the inner failure: {result:?}");
}

/// Control: the identical window shape against an address that is not the beneficiary is not
/// a divergence — revm 27 marked nothing there either.
#[test]
fn test_tripwire_silent_on_window_call_to_non_beneficiary() {
    let mut db = window_db(inner_window_call(UNRELATED, 0), 80);
    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).expect("tx must execute");
    assert!(result.result.is_success(), "outer frame absorbs the inner failure: {result:?}");
}

/// Control: once the transaction has already marked the beneficiary, a later window CALL
/// cannot diverge — the mark is idempotent — so the tripwire stays silent.
#[test]
fn test_tripwire_silent_when_beneficiary_already_marked() {
    // OUTER reads BALANCE(BENEFICIARY) first (marking it), then hands INNER its window budget.
    let outer_code = append_call(
        BytecodeBuilder::default().push_address(BENEFICIARY).append(BALANCE).append(POP),
        INNER,
        80,
        0,
    )
    .build();
    let mut db = MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000_000_u64))
        .account_code(OUTER, outer_code)
        .account_code(INNER, inner_window_call(BENEFICIARY, 0));
    let result = transact(MegaSpecId::REX4, &mut db, default_tx()).expect("tx must execute");
    assert!(result.result.is_success(), "outer frame absorbs the inner failure: {result:?}");
}
