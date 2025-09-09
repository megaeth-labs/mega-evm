//! Tests for the disabled SELFDESTRUCT opcode after Mini-Rex hardfork.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{test_utils::*, *};
use revm::{
    bytecode::opcode::{PUSH0, SELFDESTRUCT},
    context::result::{ExecutionResult, ResultAndState},
    database::{CacheDB, EmptyDB},
};

/// Test that SELFDESTRUCT opcode works normally before Mini-Rex
#[test]
fn test_selfdestruct_allowed_before_mini_rex() {
    let mut db = CacheDB::<EmptyDB>::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
    set_account_code(&mut db, contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result =
        transact(SpecId::EQUIVALENCE, &mut db, caller, callee, Bytes::default(), U256::ZERO);
    assert!(result.is_ok());
    assert_eq!(result.unwrap().result.gas_used(), 26004);
}

/// Test that SELFDESTRUCT opcode is disabled and returns `InvalidFEOpcode` after Mini-Rex
/// hardfork
#[test]
fn test_selfdestruct_disallowed_after_mini_rex() {
    let mut db = CacheDB::<EmptyDB>::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
    set_account_code(&mut db, contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result = transact(SpecId::MINI_REX, &mut db, caller, callee, Bytes::default(), U256::ZERO);
    assert!(matches!(
        result,
        Ok(ResultAndState {
            result: ExecutionResult::Halt {
                reason: HaltReason::Base(revm::context::result::HaltReason::InvalidFEOpcode),
                ..
            },
            ..
        })
    ));
}
