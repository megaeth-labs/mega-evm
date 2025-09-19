//! Simplified tests for beneficiary balance access tracking functionality
//! Any action which cause `ResultAndState` to contain the beneficiary should mark beneficiary
//! access.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction, NoOpOracle};
use revm::{
    bytecode::opcode::{BALANCE, EXTCODESIZE, POP, PUSH20, STOP},
    context::{result::ResultAndState, BlockEnv, ContextSetters, ContextTr, TxEnv},
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    inspector::NoOpInspector,
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
};

const BENEFICIARY: Address = address!("0000000000000000000000000000000000BEEF01");
const CALLER_ADDR: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT_ADDR: Address = address!("0000000000000000000000000000000000100001");

fn create_evm() -> MegaEvm<CacheDB<EmptyDB>, NoOpInspector, NoOpOracle> {
    let db = CacheDB::<EmptyDB>::default();
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX, NoOpOracle::default());

    let block_env =
        BlockEnv { beneficiary: BENEFICIARY, number: U256::from(10), ..Default::default() };
    context.set_block(block_env);

    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));

    MegaEvm::new(context)
}

fn set_account_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = AccountInfo { code: Some(bytecode), code_hash, ..Default::default() };
    db.insert_account_info(address, account_info);
}

fn execute_tx(
    evm: &mut MegaEvm<CacheDB<EmptyDB>, NoOpInspector, NoOpOracle>,
    caller: Address,
    to: Option<Address>,
    value: U256,
    disable_beneficiary: bool,
) -> ResultAndState<MegaHaltReason> {
    if disable_beneficiary {
        evm.disable_beneficiary();
    }

    let tx = MegaTransaction {
        base: TxEnv {
            caller,
            kind: match to {
                Some(addr) => TxKind::Call(addr),
                None => TxKind::Create,
            },
            data: Bytes::default(),
            value,
            gas_limit: 10000000,
            ..Default::default()
        },
        ..Default::default()
    };

    alloy_evm::Evm::transact_raw(evm, tx).unwrap()
}

fn assert_beneficiary_detection(
    evm: &MegaEvm<CacheDB<EmptyDB>, NoOpInspector, NoOpOracle>,
    result_and_state: &ResultAndState<MegaHaltReason>,
) {
    // Transaction should succeed
    assert!(result_and_state.result.is_success());

    // If state contains beneficiary, should have detection
    if result_and_state.state.contains_key(&BENEFICIARY) {
        assert!(evm.ctx_ref().has_accessed_beneficiary_balance());
    }
}

/// Test that verifies beneficiary balance access detection when the beneficiary is the transaction
/// caller. This test ensures that when the beneficiary address is used as the caller in a
/// transaction, the system correctly detects and tracks beneficiary balance access.
#[test]
fn test_beneficiary_caller() {
    let mut evm = create_evm();
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, vec![STOP].into());

    let result_and_state =
        execute_tx(&mut evm, BENEFICIARY, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);
}

/// Test that verifies beneficiary balance access detection when the beneficiary is the transaction
/// recipient. This test ensures that when a transaction sends value to the beneficiary address,
/// the system correctly detects and tracks beneficiary balance access.
#[test]
fn test_beneficiary_recipient() {
    let mut evm = create_evm();

    // Give caller some balance
    evm.ctx().db_mut().insert_account_info(
        CALLER_ADDR,
        AccountInfo { balance: U256::from(1_000_000_000_000_000_000u64), ..Default::default() },
    );

    let result_and_state = execute_tx(
        &mut evm,
        CALLER_ADDR,
        Some(BENEFICIARY),
        U256::from(500_000_000_000_000_000u64),
        false,
    );
    assert_beneficiary_detection(&evm, &result_and_state);
}

/// Test that verifies beneficiary balance access detection when a contract uses the BALANCE opcode
/// on the beneficiary address. This test ensures that when a contract reads the balance of the
/// beneficiary address using the BALANCE opcode, the system correctly detects and tracks
/// beneficiary balance access.
#[test]
fn test_balance_opcode() {
    let mut evm = create_evm();

    // Contract that reads beneficiary balance
    let mut code = vec![];
    code.push(PUSH20);
    code.extend_from_slice(BENEFICIARY.as_slice());
    code.push(BALANCE);
    code.push(POP);
    code.push(STOP);

    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code.into());

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);
}

/// Test that verifies beneficiary balance access detection when a contract uses the EXTCODESIZE
/// opcode on the beneficiary address. This test ensures that when a contract checks the code size
/// of the beneficiary address using the EXTCODESIZE opcode, the system correctly detects and tracks
/// beneficiary balance access.
#[test]
fn test_extcodesize_opcode() {
    let mut evm = create_evm();

    // Give beneficiary some code
    set_account_code(evm.ctx().db_mut(), BENEFICIARY, vec![STOP].into());

    // Contract that checks beneficiary code size
    let mut code = vec![];
    code.push(PUSH20);
    code.extend_from_slice(BENEFICIARY.as_slice());
    code.push(EXTCODESIZE);
    code.push(POP);
    code.push(STOP);

    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code.into());

    let result_and_state =
        execute_tx(&mut evm, CALLER_ADDR, Some(CONTRACT_ADDR), U256::ZERO, false);
    assert_beneficiary_detection(&evm, &result_and_state);
}
