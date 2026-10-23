//! A cost Satin inherits from Ethereum where the legacy engine departed from it: **a preload-warm
//! address is charged warm.** The precompiles, the block beneficiary and an access-list address
//! are warm from the transaction's start, and the first call to one costs what the second does.
//! The legacy engine's pricing inspected the callee's account before the opcode's own access and
//! charged that first touch cold; Satin's pricing hook inspects no account, so the warmth revm
//! preloads is what the opcode sees.
//!
//! It is read off the regular ledger, which is the gas the opcodes charge.

use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    test_utils::{op_transaction, BytecodeBuilder, MemoryDatabase},
    MegaTransaction,
};
use revm::{
    bytecode::opcode::{CALL, CALLCODE, POP, PUSH0, STATICCALL, STOP},
    context::{
        transaction::{AccessList, AccessListItem, TransactionType},
        TxEnv,
    },
    context_interface::cfg::GasId,
};

use crate::{
    common::{block, call, execute},
    salt::entry,
};

const CALLER: Address = address!("0000000000000000000000000000000000e20000");
const CONTRACT: Address = address!("0000000000000000000000000000000000e20001");
/// An account that exists and holds no code, never warmed by anything but an access list.
const EXISTING: Address = address!("0000000000000000000000000000000000e20002");
/// The identity precompile.
const IDENTITY: Address = address!("0000000000000000000000000000000000000004");

/// What the first touch of an address costs over a warm one: the schedule's cold account access
/// less its warm read.
fn cold_over_warm() -> u64 {
    entry(GasId::cold_account_additional_cost())
}

/* ---------- preload-warm addresses ---------- */

/// `units` identical calls of `opcode` to `to`, each forwarding 50,000 and dropping its status.
fn calls(opcode: u8, to: Address, units: usize) -> Bytes {
    let mut code = BytecodeBuilder::default();
    for _ in 0..units {
        code = code.append_many([PUSH0, PUSH0, PUSH0, PUSH0]);
        if opcode != STATICCALL {
            code = code.append(PUSH0);
        }
        code = code.push_address(to).push_number(50_000_u32).append(opcode).append(POP);
    }
    code.append(STOP).build()
}

/// A call from `CALLER` to `CONTRACT` carrying `access_list`, as an EIP-2930 transaction.
fn with_access_list(access_list: AccessList) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        tx_type: TransactionType::Eip2930 as u8,
        caller: CALLER,
        kind: TxKind::Call(CONTRACT),
        gas_limit: 10_000_000 + crate::common::body_history(100),
        access_list,
        ..Default::default()
    }))
}

/// How much more the first of the calls `opcode` makes to `to` costs than the second, on the
/// regular ledger: the 0-, 1- and 2-call programs are run and differenced, so what every call
/// costs alike cancels, and what is left is the first touch's price over a warm touch's.
fn first_call_over_second(opcode: u8, to: Address, tx: impl Fn() -> MegaTransaction) -> i64 {
    let [none, one, two] = [0, 1, 2].map(|units| {
        let db = MemoryDatabase::default()
            .account_balance(CALLER, U256::from(10u64.pow(18)))
            .account_balance(EXISTING, U256::from(1))
            .account_code(CONTRACT, calls(opcode, to, units));
        let outcome = execute(db, tx());
        assert!(outcome.result.is_success(), "{units} calls: {:?}", outcome.result);
        outcome.gas.regular as i64
    });
    (one - none) - (two - one)
}

fn plain() -> MegaTransaction {
    call(CALLER, CONTRACT, U256::ZERO, 10_000_000 + crate::common::body_history(0))
}

/// The control the other cases are measured against: an address nothing preloads costs the cold
/// price on its first touch, so the measurement sees a cold touch where there is one.
#[test]
fn test_the_first_call_to_an_address_nothing_warmed_is_charged_cold() {
    assert_eq!(first_call_over_second(CALL, EXISTING, plain), cold_over_warm() as i64);
}

/// A precompile is warm from the transaction's start: the first `CALL` to one costs what the
/// second does.
#[test]
fn test_the_first_call_to_a_precompile_is_charged_warm() {
    assert_eq!(first_call_over_second(CALL, IDENTITY, plain), 0);
}

/// So does the first `STATICCALL`.
#[test]
fn test_the_first_staticcall_to_a_precompile_is_charged_warm() {
    assert_eq!(first_call_over_second(STATICCALL, IDENTITY, plain), 0);
}

/// An access-list address is warm from the start whether it is listed with storage keys or
/// without: the first `CALL` to it costs what the second does, where without the list it pays the
/// cold price.
#[test]
fn test_the_first_call_to_an_access_list_address_is_charged_warm() {
    let listed = |keys: Vec<B256>| {
        move || {
            with_access_list(AccessList(vec![AccessListItem {
                address: EXISTING,
                storage_keys: keys.clone(),
            }]))
        }
    };
    assert_eq!(first_call_over_second(CALL, EXISTING, listed(vec![])), 0, "without keys");
    assert_eq!(first_call_over_second(CALL, EXISTING, listed(vec![B256::ZERO])), 0, "with a key");
}

/// The same for `CALLCODE`: its first touch of an access-list address listed without keys costs
/// what the second does.
#[test]
fn test_the_first_callcode_to_an_access_list_address_is_charged_warm() {
    let listed = || {
        with_access_list(AccessList(vec![AccessListItem {
            address: EXISTING,
            storage_keys: vec![],
        }]))
    };
    assert_eq!(first_call_over_second(CALLCODE, EXISTING, listed), 0);
}

/// The block beneficiary is warm from the start (EIP-3651): the first `CALL` to it costs what the
/// second does. The call reads the beneficiary's account, which gas detention marks, but a mark
/// charges nothing.
#[test]
fn test_the_first_call_to_the_beneficiary_is_charged_warm() {
    let beneficiary = block().beneficiary;
    assert_eq!(first_call_over_second(CALL, beneficiary, plain), 0);
}
