//! Reads that gas or a switch skips, through the harness: a cold `SLOAD` and a cold account load
//! with less gas than the cold surcharge, an oracle read the frame cannot pay, and reads a frame
//! that switched its volatile-data access off is refused. None reaches the database, none is in
//! the record, and the replay skips them the same way.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IMegaAccessControl, IOracle, ACCESS_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::BytecodeBuilder,
};
use revm::{
    bytecode::opcode::{BALANCE, BLOCKHASH, CALL, GAS, POP, PUSH0, SLOAD},
    context::result::ExecutionResult,
};

use super::harness::{call, Case};
use crate::common::{self, BLOCK_NUMBER, CONTRACT};

/// An account no transaction here funds or reads unless a cold load is made.
const STRANGER: Address = address!("0x5000000000000000000000000000000000000005");

/// The regular gas an empty call spends before its first instruction, at the byte prices in
/// effect: what a transaction to [`CONTRACT`] with empty code is billed on the regular ledger.
fn intrinsic() -> u64 {
    let replay = Case::new("intrinsic probe", common::database())
        .tx(call(0, CONTRACT, Bytes::new(), common::empty_call_gas()))
        .run();
    replay.recorded.tx(0).gas.regular
}

/// A gas limit leaving the frame `room` of regular gas after the intrinsic cost and the body.
fn gas_with_room(room: u64) -> u64 {
    intrinsic() + common::body_history(0) + room
}

/// A cold `SLOAD` with less gas than its cold surcharge is skipped before the database is asked:
/// the slot is not in the record, the transaction runs out of gas, and the replay does the same.
#[test]
fn test_a_cold_sload_short_of_gas_reads_no_slot() {
    let mut db = common::database();
    db.set_account_code(
        CONTRACT,
        BytecodeBuilder::default().push_number(7_u8).append(SLOAD).stop().build(),
    );
    let replay = Case::new("cold sload skipped", db)
        .tx(call(0, CONTRACT, Bytes::new(), gas_with_room(3 + 2_000)))
        .run();
    let run = &replay.recorded;
    assert!(matches!(run.tx(0).result, ExecutionResult::Halt { .. }), "{:?}", run.tx(0).result);
    assert!(!run.record.storage.contains_key(&(CONTRACT, U256::from(7))));
}

/// The same slot with the gas to pay for it is read and recorded.
#[test]
fn test_a_cold_sload_with_gas_reads_the_slot() {
    let mut db = common::database();
    db.set_account_code(
        CONTRACT,
        BytecodeBuilder::default().push_number(7_u8).append(SLOAD).stop().build(),
    );
    let replay = Case::new("cold sload", db)
        .tx(call(0, CONTRACT, Bytes::new(), gas_with_room(3 + 2_200)))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert_eq!(run.record.storage.get(&(CONTRACT, U256::from(7))), Some(&U256::ZERO));
}

/// A cold `BALANCE` with less gas than its cold surcharge loads no account.
#[test]
fn test_a_cold_account_load_short_of_gas_reads_no_account() {
    let mut db = common::database();
    db.set_account_code(
        CONTRACT,
        BytecodeBuilder::default().push_address(STRANGER).append(BALANCE).stop().build(),
    );
    let replay = Case::new("cold balance skipped", db)
        .tx(call(0, CONTRACT, Bytes::new(), gas_with_room(3 + 2_500)))
        .run();
    let run = &replay.recorded;
    assert!(matches!(run.tx(0).result, ExecutionResult::Halt { .. }), "{:?}", run.tx(0).result);
    assert!(!run.record.accounts.contains_key(&STRANGER));

    let mut db = common::database();
    db.set_account_code(
        CONTRACT,
        BytecodeBuilder::default().push_address(STRANGER).append(BALANCE).stop().build(),
    );
    let replay = Case::new("cold balance", db)
        .tx(call(0, CONTRACT, Bytes::new(), gas_with_room(3 + 2_700)))
        .run();
    assert_eq!(replay.recorded.record.accounts.get(&STRANGER), Some(&None));
}

/// An oracle read the frame cannot pay is neither loaded nor asked: a call into the Oracle with
/// less gas than a cold access leaves the slot out of the record and the service unasked.
#[test]
fn test_an_oracle_read_short_of_gas_asks_nothing() {
    let input: Bytes = IOracle::getSlotCall { slot: U256::from(42) }.abi_encode().into();
    let calldata = input.len() as u64;
    let case = |room: u64| {
        Case::new("oracle short of gas", common::database())
            .envs(super::harness::Envs::new().with_oracle_storage(U256::from(42), U256::from(1)))
            .tx(call(
                0,
                ORACLE_CONTRACT_ADDRESS,
                input.clone(),
                intrinsic() + common::body_history(calldata) + room,
            ))
    };
    let short = case(1_500).run();
    assert!(matches!(short.recorded.tx(0).result, ExecutionResult::Halt { .. }));
    assert!(short.recorded.record.oracle_reads.is_empty(), "the service was not asked");
    assert!(!short
        .recorded
        .record
        .storage
        .contains_key(&(ORACLE_CONTRACT_ADDRESS, U256::from(42))));

    let enough = case(50_000).run();
    assert!(enough.recorded.tx(0).result.is_success());
    assert_eq!(enough.recorded.record.oracle_reads, vec![(U256::from(42), Some(U256::from(1)))]);
}

/// Code that switches its volatile-data access off, then runs `then`.
fn disabled_then(then: BytecodeBuilder) -> Bytes {
    let disable = IMegaAccessControl::disableVolatileDataAccessCall {}.abi_encode();
    let code = BytecodeBuilder::default().mstore(0, &disable);
    let code = code
        .append_many([PUSH0, PUSH0])
        .push_number(disable.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(ACCESS_CONTROL_ADDRESS)
        .append_many([GAS, CALL, POP]);
    let mut out = code.build().to_vec();
    out.extend_from_slice(&then.build());
    out.into()
}

/// A refused `BLOCKHASH` reads no hash: the frame reverts, the record holds none and the export
/// is empty.
#[test]
fn test_a_refused_block_hash_read_reads_nothing() {
    let mut db = common::database();
    let then = BytecodeBuilder::default()
        .push_number(BLOCK_NUMBER - 1)
        .append(BLOCKHASH)
        .append(POP)
        .stop();
    db.set_account_code(CONTRACT, disabled_then(then));
    let replay = Case::new("refused blockhash", db)
        .tx(call(0, CONTRACT, Bytes::new(), 1_000_000 + common::body_history(0)))
        .run();
    let run = &replay.recorded;
    assert!(matches!(run.tx(0).result, ExecutionResult::Revert { .. }), "{:?}", run.tx(0).result);
    assert!(run.record.block_hashes.is_empty());
    assert!(run.block_hashes.is_empty());
}

/// A refused oracle read loads nothing and asks nothing.
#[test]
fn test_a_refused_oracle_read_reads_nothing() {
    let mut db = common::database();
    let read = IOracle::getSlotCall { slot: U256::from(42) }.abi_encode();
    let then = BytecodeBuilder::default().mstore(0, &read);
    let then = then
        .append_many([PUSH0, PUSH0])
        .push_number(read.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append_many([GAS, CALL, POP])
        .stop();
    db.set_account_code(CONTRACT, disabled_then(then));
    let replay = Case::new("refused oracle read", db)
        .envs(super::harness::Envs::new().with_oracle_storage(U256::from(42), U256::from(1)))
        .tx(call(0, CONTRACT, Bytes::new(), 1_000_000 + common::body_history(0)))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "the outer frame survives the Oracle's revert");
    assert!(run.record.oracle_reads.is_empty());
    assert!(!run.record.storage.contains_key(&(ORACLE_CONTRACT_ADDRESS, U256::from(42))));
}
