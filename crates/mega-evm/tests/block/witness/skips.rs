//! Reads that gas or a switch skips, through the harness: a cold `SLOAD` and a cold account load
//! with less gas than the cold surcharge, and reads a frame that switched its volatile-data access
//! off is refused. None reaches the database, none is in the record, and the replay skips them the
//! same way. An Oracle read is not one of them: it is made before its charge, so a frame that
//! cannot pay it reads the slot and asks the service all the same, and the replay does too.

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{IMegaAccessControl, IOracle, ACCESS_CONTROL_ADDRESS, ORACLE_CONTRACT_ADDRESS},
    test_utils::BytecodeBuilder,
    volatile_data_access_disabled_revert_data, OracleRead, VolatileDataAccess,
};
use revm::{
    bytecode::opcode::{
        BALANCE, BLOCKHASH, CALL, CALLCODE, GAS, MSTORE, POP, PUSH0, RETURN, SLOAD,
    },
    context::result::ExecutionResult,
    context_interface::cfg::GasId,
    state::{AccountInfo, Bytecode},
};

use super::{
    harness::{call, Case},
    oracle::db as chain_with_oracle,
};
use crate::common::{self, BLOCK_NUMBER, CONTRACT};

/// An account no transaction here funds or reads unless a cold load is made.
const STRANGER: Address = address!("0x5000000000000000000000000000000000000005");

/// An EIP-7702 delegator of the block beneficiary, the harness's block environment's default.
const DELEGATOR: Address = address!("0x5000000000000000000000000000000000000006");

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

/// Code that reads slot 7 of its own storage and stops.
fn sload_seven() -> Bytes {
    BytecodeBuilder::default().push_number(7_u8).append(SLOAD).stop().build()
}

/// A cold `SLOAD` of another contract than the Oracle charges its static 100 and then its cold
/// access, each on its own, and is skipped before the database is asked when the gas left after
/// the 100 is short of the cold access: with 50 or 2,099 left at the `SLOAD` the slot is not read
/// and the transaction runs out of gas; with 2,100 or 4,099 it is read and the transaction
/// completes. The replay does the same.
#[test]
fn test_a_cold_sload_reads_its_slot_only_with_the_gas_for_its_cold_access() {
    for (left, reads) in [(50, false), (2_099, false), (2_100, true), (4_099, true)] {
        let mut db = common::database();
        db.set_account_code(CONTRACT, sload_seven());
        let replay = Case::new("cold sload by gas", db)
            .tx(call(0, CONTRACT, Bytes::new(), gas_with_room(3 + left)))
            .run();
        let run = &replay.recorded;
        let slot = run.record.storage.get(&(CONTRACT, U256::from(7)));
        if reads {
            assert!(run.tx(0).result.is_success(), "{left}: {:?}", run.tx(0).result);
            assert_eq!(slot, Some(&U256::ZERO), "{left}: the slot is read");
        } else {
            assert!(
                matches!(run.tx(0).result, ExecutionResult::Halt { .. }),
                "{left}: {:?}",
                run.tx(0).result
            );
            assert_eq!(slot, None, "{left}: the slot is not read");
        }
    }
}

/// A cold `BALANCE` with less gas than its cold surcharge loads no account.
#[test]
fn test_a_cold_account_load_short_of_gas_reads_no_account() {
    // The push, and `BALANCE`'s static 100 leaves the cold surcharge short by that 100.
    let cold = mega_evm::satin_gas_params().get(GasId::cold_account_additional_cost());
    let mut db = common::database();
    db.set_account_code(
        CONTRACT,
        BytecodeBuilder::default().push_address(STRANGER).append(BALANCE).stop().build(),
    );
    let replay = Case::new("cold balance skipped", db)
        .tx(call(0, CONTRACT, Bytes::new(), gas_with_room(3 + cold)))
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
        .tx(call(0, CONTRACT, Bytes::new(), gas_with_room(3 + 100 + cold + 100)))
        .run();
    assert_eq!(replay.recorded.record.accounts.get(&STRANGER), Some(&None));
}

/// Code that calls the Oracle forwarding `gas` and returns whether the call succeeded, as a word.
fn calls_the_oracle_forwarding(gas: u64) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .push_number(gas)
        .append_many([CALL, PUSH0, MSTORE])
        .push_number(32_u8)
        .append_many([PUSH0, RETURN])
        .build()
}

/// An Oracle read is made before any of its 2,100 is charged: whatever its frame holds at the
/// `SLOAD` — 50, 2,099, 2,100 or 4,099 — the slot is read from the database and the service
/// asked, on a chain that holds the Oracle, and the frame then runs out of gas on the first two
/// and completes the other two. The replay reads and asks the same, and replays the recorded
/// answer at each.
#[test]
fn test_an_oracle_read_is_read_and_asked_whatever_its_frame_holds() {
    let slot = (ORACLE_CONTRACT_ADDRESS, U256::from(42));
    for (left, completes) in [(50, false), (2_099, false), (2_100, true), (4_099, true)] {
        let mut db = chain_with_oracle();
        db.set_account_code(
            ORACLE_CONTRACT_ADDRESS,
            BytecodeBuilder::default().push_number(42_u8).append(SLOAD).stop().build(),
        );
        db.set_account_code(CONTRACT, calls_the_oracle_forwarding(3 + left));
        let replay = Case::new("oracle read by gas", db)
            .envs(super::harness::Envs::new().with_oracle_storage(U256::from(42), U256::from(1)))
            .tx(call(0, CONTRACT, Bytes::new(), 1_000_000 + common::body_history(0)))
            .run();
        let run = &replay.recorded;
        assert!(run.tx(0).result.is_success(), "{left}: {:?}", run.tx(0).result);
        let succeeded = U256::from_be_slice(run.tx(0).result.output().expect("the flag"));
        assert_eq!(succeeded, U256::from(u8::from(completes)), "{left}: the Oracle's frame");
        let read = OracleRead { slot: U256::from(42), answer: Some(U256::from(1)) };
        assert_eq!(run.record.oracle_reads, vec![read], "{left}: the service was asked");
        assert_eq!(run.tx(0).oracle_reads, vec![read], "{left}: and the read recorded");
        assert!(run.record.storage.contains_key(&slot), "{left}: the slot is read");
    }
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

/// Code that reads slot 42 through the Oracle and stops.
fn read_slot_then_stop() -> BytecodeBuilder {
    let read = IOracle::getSlotCall { slot: U256::from(42) }.abi_encode();
    let code = BytecodeBuilder::default().mstore(0, &read);
    code.append_many([PUSH0, PUSH0])
        .push_number(read.len() as u64)
        .append_many([PUSH0, PUSH0])
        .push_address(ORACLE_CONTRACT_ADDRESS)
        .append_many([GAS, CALL, POP])
        .stop()
}

/// A refused oracle read loads nothing and asks nothing, on a chain that holds the Oracle: the
/// same read with the frame's access on is loaded from the database and asked.
#[test]
fn test_a_refused_oracle_read_reads_nothing() {
    let slot = (ORACLE_CONTRACT_ADDRESS, U256::from(42));
    let case = |name: &str, code: Bytes| {
        let mut db = chain_with_oracle();
        db.set_account_code(CONTRACT, code);
        Case::new(name, db)
            .envs(super::harness::Envs::new().with_oracle_storage(U256::from(42), U256::from(1)))
            .tx(call(0, CONTRACT, Bytes::new(), 1_000_000 + common::body_history(0)))
            .run()
    };

    let refused = case("refused oracle read", disabled_then(read_slot_then_stop()));
    let run = &refused.recorded;
    assert!(run.tx(0).result.is_success(), "the outer frame survives the Oracle's revert");
    assert!(run.record.oracle_reads.is_empty());
    assert!(!run.record.storage.contains_key(&slot));

    let allowed = case("allowed oracle read", read_slot_then_stop().build());
    let run = &allowed.recorded;
    assert!(run.tx(0).result.is_success());
    assert_eq!(run.record.oracle_reads.len(), 1, "with access on, the service is asked");
    assert!(run.record.storage.contains_key(&slot), "and the slot is read from the database");
}

/// With access off, a call opcode reads its target's account and code to find the EIP-7702
/// delegate it would follow, before it charges or checks anything: a `CALLCODE` of a delegator
/// of the block beneficiary, with an input of 1 MiB its 1,000 gas cannot pay, is refused, and the
/// delegator's account and code are in the record and the witness, which the replay reads alike.
#[test]
fn test_a_refused_call_reads_its_targets_account_and_code() {
    let delegation = Bytecode::new_eip7702(Address::ZERO);
    let mut db = common::database();
    db.insert_account_info(
        DELEGATOR,
        AccountInfo {
            code_hash: delegation.hash_slow(),
            code: Some(delegation.clone()),
            ..Default::default()
        },
    );
    let then = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0])
        .push_number(0x10_0000_u32)
        .append_many([PUSH0, PUSH0])
        .push_address(DELEGATOR)
        .push_number(1_000_u16)
        .append(CALLCODE)
        .stop();
    db.set_account_code(CONTRACT, disabled_then(then));
    let replay = Case::new("refused call of a delegator", db)
        .tx(call(0, CONTRACT, Bytes::new(), 1_000_000 + common::body_history(0)))
        .run();
    let run = &replay.recorded;
    match &run.tx(0).result {
        ExecutionResult::Revert { output, .. } => assert_eq!(
            output,
            &volatile_data_access_disabled_revert_data(VolatileDataAccess::BENEFICIARY_BALANCE)
        ),
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert!(run.record.accounts.contains_key(&DELEGATOR), "the target's account is read");
    assert!(run.record.codes.contains_key(&delegation.hash_slow()), "and its code");
}
