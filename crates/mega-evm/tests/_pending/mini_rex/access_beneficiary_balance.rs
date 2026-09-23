//! Simplified tests for beneficiary balance access tracking functionality.
//!
//! When the block beneficiary is accessed, gas is immediately detained (limited to 10,000).
//! Any action which causes `ResultAndState` to contain the beneficiary should mark beneficiary
//! access and trigger gas detention.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::mini_rex::BLOCK_ENV_ACCESS_COMPUTE_GAS,
    test_utils::{BytecodeBuilder, GasInspector, MsgCallMeta},
    EmptyExternalEnv, MegaContext, MegaEvm, MegaHaltReason, MegaSpecId, MegaTransaction,
};
use revm::{
    bytecode::opcode::{BALANCE, EXTCODECOPY, EXTCODEHASH, EXTCODESIZE, POP, PUSH0, STOP},
    context::{result::ResultAndState, BlockEnv, ContextSetters, ContextTr, TxEnv},
    database::{CacheDB, EmptyDB},
    handler::EvmTr,
    primitives::TxKind,
    state::{AccountInfo, Bytecode},
};

const BENEFICIARY: Address = address!("0000000000000000000000000000000000BEEF01");
const CALLER_ADDR: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT_ADDR: Address = address!("0000000000000000000000000000000000100001");
const NESTED_CONTRACT: Address = address!("0000000000000000000000000000000000100002");

fn create_evm() -> MegaEvm<CacheDB<EmptyDB>, GasInspector, EmptyExternalEnv> {
    let db = CacheDB::<EmptyDB>::default();
    let mut context = MegaContext::new(db, MegaSpecId::MINI_REX);

    let block_env =
        BlockEnv { beneficiary: BENEFICIARY, number: U256::from(10), ..Default::default() };
    context.set_block(block_env);

    context.chain_mut().operator_fee_scalar = Some(U256::from(0));
    context.chain_mut().operator_fee_constant = Some(U256::from(0));

    MegaEvm::new(context).with_inspector(GasInspector::new())
}

fn set_account_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
    let bytecode = Bytecode::new_legacy(code);
    let code_hash = bytecode.hash_slow();
    let account_info = AccountInfo { code: Some(bytecode), code_hash, ..Default::default() };
    db.insert_account_info(address, account_info);
}

fn execute_tx(
    evm: &mut MegaEvm<CacheDB<EmptyDB>, GasInspector, EmptyExternalEnv>,
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
    evm: &MegaEvm<CacheDB<EmptyDB>, GasInspector, EmptyExternalEnv>,
    result_and_state: &ResultAndState<MegaHaltReason>,
) {
    // Transaction should succeed
    assert!(result_and_state.result.is_success());

    // If state contains beneficiary, should have detection
    if result_and_state.state.contains_key(&BENEFICIARY) {
        assert!(evm.ctx_ref().volatile_data_tracker.borrow().has_accessed_beneficiary_balance());
    }
}

/// Test that verifies detained gas is restored (refunded) at the end of the transaction.
/// This ensures that users are not charged for the gas that was temporarily detained during
/// beneficiary access.
///
/// The transaction should start with high gas, detain most of it when beneficiary is accessed,
/// but the detained gas should be refunded so the final `gas_used` is reasonable.
#[test]
fn test_detained_gas_is_restored() {
    let mut evm = create_evm();

    // Simple contract that accesses beneficiary balance
    let code = BytecodeBuilder::default()
        .push_address(BENEFICIARY)
        .append(BALANCE)
        .append(POP)
        .stop()
        .build();
    set_account_code(evm.ctx().db_mut(), CONTRACT_ADDR, code);

    // Execute with a large gas limit
    let gas_limit = 1_000_000u64;
    let tx = MegaTransaction {
        base: TxEnv {
            caller: CALLER_ADDR,
            kind: TxKind::Call(CONTRACT_ADDR),
            data: Bytes::default(),
            value: U256::ZERO,
            gas_limit,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = alloy_evm::Evm::transact_raw(&mut evm, tx).unwrap();
    assert!(result.result.is_success());
    assert!(evm.ctx_ref().volatile_data_tracker.borrow().has_accessed_beneficiary_balance());

    // The gas_used should be much less than the gas_limit because detained gas is refunded.
    // We expect gas_used to be only a few thousand (for the actual work done), not close to 1M.
    let gas_used = result.result.gas_used();
    assert!(
        gas_used < 50_000,
        "Gas used should be low after detained gas restoration, got {}",
        gas_used
    );

    // Verify that gas was actually limited during execution
    let gas_inspector = &evm.inspector;
    let mut saw_limited_gas = false;
    gas_inspector.trace.as_ref().unwrap().iterate_with(
        |_node_location, _node, _item_location, item| {
            let opcode_info = item.borrow();
            if opcode_info.gas_after <= BLOCK_ENV_ACCESS_COMPUTE_GAS {
                saw_limited_gas = true;
            }
        },
    );
    assert!(saw_limited_gas, "Should have seen gas limited to 10k during execution");
}
