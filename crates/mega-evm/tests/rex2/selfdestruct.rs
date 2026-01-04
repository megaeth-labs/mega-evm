//! Tests for the SELFDESTRUCT opcode behavior in Rex2.

use alloy_primitives::{address, Bytes, U256};
use mega_evm::{
    revm::{
        bytecode::opcode::{PUSH0, SELFDESTRUCT},
        context::result::{ExecutionResult, ResultAndState},
    },
    test_utils::{transact, MemoryDatabase},
    *,
};

#[test]
fn test_selfdestruct_allowed_in_rex2() {
    let mut db = MemoryDatabase::default();
    let contract_address = address!("0000000000000000000000000000000000100001");
    let code = vec![PUSH0, PUSH0, SELFDESTRUCT];
    db.set_account_code(contract_address, code.into());

    let caller = address!("0000000000000000000000000000000000100000");
    let callee = Some(contract_address);
    let result = transact(MegaSpecId::REX2, &mut db, caller, callee, Bytes::default(), U256::ZERO);

    assert!(matches!(result, Ok(ResultAndState { result: ExecutionResult::Success { .. }, .. })));
}
