//! Tests for the disabled SELFDESTRUCT opcode after Mini-Rex hardfork.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{test_utils::{MemoryDatabase, transact}, *};
use revm::{
    bytecode::opcode::{PUSH0, SELFDESTRUCT},
    context::result::{ExecutionResult, ResultAndState},
};

/// Test that verifies the SELFDESTRUCT opcode works normally before the Mini-Rex hardfork.
/// This test ensures that contracts using the SELFDESTRUCT opcode execute successfully
/// and consume the expected amount of gas when using the `EQUIVALENCE` spec.
#[test]
fn test_selfdestruct_allowed_before_mini_rex() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
    db.set_account_code(contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(MegaSpecId::EQUIVALENCE, &mut db, caller, callee, Bytes::default(), U256::ZERO);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().result.gas_used(), 26004);
}

/// Test that verifies the SELFDESTRUCT opcode is disabled and returns `InvalidFEOpcode` after
/// the Mini-Rex hardfork. This test ensures that contracts attempting to use the SELFDESTRUCT
/// opcode are properly rejected with the correct halt reason when using the `MINI_REX` spec.
#[test]
fn test_selfdestruct_disallowed_after_mini_rex() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
    db.set_account_code(contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(MegaSpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO);
    assert!(matches!(
        result,
        Ok(ResultAndState {
            result: ExecutionResult::Halt {
                reason: MegaHaltReason::Base(OpHaltReason::Base(EthHaltReason::InvalidFEOpcode)),
                ..
            },
            ..
        })
    ));
}
