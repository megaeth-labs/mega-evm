//! What a Satin transaction leaves in its returned state.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::test_utils::{BytecodeBuilder, MemoryDatabase};
use revm::{
    bytecode::opcode::{CALL, GAS, INVALID, MSTORE, POP, PUSH0, RETURN, SLOAD},
    database::{AccountState, StateBuilder},
    state::Bytecode,
};

use crate::common::{call, run};

const CALLER: Address = address!("0000000000000000000000000000000000100000");
const CONTRACT: Address = address!("0000000000000000000000000000000000100001");
const GAS_LIMIT: u64 = 1_000_000;

/// A read of a slot that was never written still puts the slot in the returned state.
#[test]
fn test_evm_state_zero_storage() {
    let db = MemoryDatabase::default().account_code(CONTRACT, Bytes::from_static(&[PUSH0, SLOAD]));
    let (result, _) = run(db, call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT));
    let contract = &result.state[&CONTRACT];
    assert_eq!(contract.storage.len(), 1);
    assert!(contract.storage.contains_key(&U256::ZERO));
}

/// A failed transaction's reads are in the returned state too.
#[test]
fn test_evm_state_include_failed_tx_reads() {
    let db = MemoryDatabase::default()
        .account_code(CONTRACT, Bytes::from_static(&[PUSH0, SLOAD, INVALID]));
    let (result, _) = run(db, call(CALLER, CONTRACT, U256::ZERO, GAS_LIMIT));
    assert!(!result.result.is_success());
    assert!(result.state[&CONTRACT].storage.contains_key(&U256::ZERO));
}

/// A call to an existing empty account touches it, and EIP-161 clears it when the state is
/// applied.
#[test]
fn test_state_clear_with_noop_call() {
    let empty_account = CONTRACT;
    let mut db = MemoryDatabase::default().account_nonce(empty_account, 0);
    let mut state = StateBuilder::<MemoryDatabase>::default()
        .with_database(&mut db)
        .with_bundle_update()
        .build();
    let (result, _) = run(&mut state, call(CALLER, empty_account, U256::ZERO, GAS_LIMIT));
    assert!(result.state[&empty_account].is_touched());

    let transitions = state.cache.apply_evm_state(result.state, |_, _| {});
    let (_, transition) =
        transitions.iter().find(|(address, _)| *address == empty_account).unwrap();
    assert!(transition.storage_was_destroyed);
}

const PARENT: Address = address!("0000000000000000000000000000000000500001");
const DELEGATOR: Address = address!("0000000000000000000000000000000000500005");
const DELEGATE: Address = address!("0000000000000000000000000000000000500006");

/// A contract that calls `first`, then `second`, and returns the 32 bytes `second` returned.
fn two_calls_returning_the_second(first: Address, second: Address) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(first)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .push_number(32u8)
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(second)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .push_number(32u8)
        .append(PUSH0)
        .append(RETURN)
        .build()
}

/// A write through an EIP-7702 delegator goes to the delegator's storage: the delegate's own slot
/// keeps its original value when the delegate later runs in its own context.
#[test]
fn test_inspect_storage_does_not_pollute_delegate_journal_rex4() {
    let delegate_code = BytecodeBuilder::default()
        .sstore(U256::from(1), U256::from(0x77))
        .push_number(1u8)
        .append(SLOAD)
        .append(PUSH0)
        .append(MSTORE)
        .push_number(32u8)
        .append(PUSH0)
        .append(RETURN)
        .build();
    let mut db = MemoryDatabase::default()
        .account_code(DELEGATE, delegate_code)
        .account_storage(DELEGATE, U256::from(1), U256::from(99))
        .account_storage(DELEGATOR, U256::from(1), U256::from(42))
        .account_code(PARENT, two_calls_returning_the_second(DELEGATOR, DELEGATE));
    let delegation = Bytecode::new_eip7702(DELEGATE);
    let account = db.load_account(DELEGATOR).unwrap();
    account.info.code_hash = delegation.hash_slow();
    account.info.code = Some(delegation);
    account.account_state = AccountState::None;

    let (result, _) = run(db, call(CALLER, PARENT, U256::ZERO, 100_000_000));
    assert!(result.result.is_success(), "{:?}", result.result);
    let second = U256::from_be_slice(result.result.output().unwrap());
    assert_eq!(second, U256::from(0x77), "the delegate reads its own write back");
    let slot = &result.state[&DELEGATE].storage[&U256::from(1)];
    assert_eq!(slot.original_value(), U256::from(99), "the delegate's own original value");
    let slot = &result.state[&DELEGATOR].storage[&U256::from(1)];
    assert_eq!(slot.original_value(), U256::from(42), "the delegator's own original value");
}
