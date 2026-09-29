//! State-changing mechanisms through the harness: an EIP-7702 delegation, a creation, the two
//! shapes of `SELFDESTRUCT`, a value transfer that creates its recipient, and deposits — with the
//! L1 block info a user transaction reads and a deposit does not.

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::test_utils::BytecodeBuilder;
use op_revm::constants::{
    DA_FOOTPRINT_GAS_SCALAR_SLOT, ECOTONE_L1_BLOB_BASE_FEE_SLOT, ECOTONE_L1_FEE_SCALARS_SLOT,
    L1_BASE_FEE_SLOT, L1_BLOCK_CONTRACT, OPERATOR_FEE_SCALARS_SLOT,
};
use revm::bytecode::opcode::{CODECOPY, PUSH0, RETURN, SELFDESTRUCT};

use super::{
    basics::{slot, slot_writer, write_gas},
    harness::{authorization, call, call_with_value, create, deposit, eip7702, Case},
};
use crate::common::{self, CALLER, CONTRACT};

/// An account that holds nothing.
const EMPTY: Address = address!("0x4000000000000000000000000000000000000004");

/// A contract that destroys itself to [`EMPTY`].
const DESTROYER: Address = address!("0x4000000000000000000000000000000000000005");

/// A fresh depositor.
const DEPOSITOR: Address = address!("0x4000000000000000000000000000000000000006");

/// Init code that deploys `runtime`.
fn deploying(runtime: &[u8]) -> Bytes {
    let len = u8::try_from(runtime.len()).expect("a short runtime");
    let tail = 11_u16.to_be_bytes();
    let mut code =
        vec![0x60, len, 0x61, tail[0], tail[1], PUSH0, CODECOPY, 0x60, len, PUSH0, RETURN];
    code.extend_from_slice(runtime);
    code.into()
}

/// A gas limit with room for a creation and a new account at any byte price.
const CREATION_GAS: u64 = 5_000_000;

/// An EIP-7702 call to an authority delegating to the slot writer: the authority's absent account
/// is in the record, it is created with its delegation, and its slot is written.
#[test]
fn test_a_delegation_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let (signed, authority) = authorization(CONTRACT, 0);
    let replay = Case::new("delegation", db)
        .tx(eip7702(0, authority, slot(5), vec![signed], CREATION_GAS))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert_eq!(run.record.accounts.get(&authority), Some(&None), "the authority was absent");
    let account = &run.tx(0).state[&authority];
    assert_eq!(account.info.nonce, 1, "the authorization bumped it");
    assert!(account.info.code.as_ref().is_some_and(|code| code.is_eip7702()));
    assert!(account.storage.get(&U256::from(5)).is_some_and(|slot| slot.is_changed()));
    if !common::state_is_free() {
        assert!(!run.bucket_ids.is_empty(), "the authority's account and the slot were priced");
    }
}

/// A creation transaction, then a call to what it created.
#[test]
fn test_a_creation_replays() {
    let created = CALLER.create(0);
    let replay = Case::new("creation", common::database())
        .tx(create(0, deploying(&[0x00; 5]), CREATION_GAS))
        .tx(call(1, created, Bytes::new(), common::empty_call_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert!(run.tx(1).result.is_success(), "{:?}", run.tx(1).result);
    assert_eq!(run.record.accounts.get(&created), Some(&None), "the created address was read");
    assert!(
        !run.record.codes.values().any(|code| code.original_bytes() == vec![0x00; 5]),
        "the created code came from the block, not the witness"
    );
}

/// A `SELFDESTRUCT` of a pre-existing contract moves its balance to an empty account, which is
/// created and priced; and a contract created and destroyed in one transaction leaves nothing.
#[test]
fn test_selfdestructs_replay() {
    let mut db = common::database();
    db.set_account_code(DESTROYER, BytecodeBuilder::default().selfdestruct(EMPTY).build());
    db.set_account_balance(DESTROYER, U256::from(1_000));
    let init = BytecodeBuilder::default().selfdestruct(EMPTY).build();
    db.set_account_code(
        CONTRACT,
        BytecodeBuilder::default().create(U256::ZERO, init).stop().build(),
    );
    db.set_account_nonce(CONTRACT, 1);
    let replay = Case::new("selfdestruct", db)
        .tx(call(0, DESTROYER, Bytes::new(), CREATION_GAS))
        .tx(call(1, CONTRACT, Bytes::new(), CREATION_GAS))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert_eq!(run.record.accounts.get(&EMPTY), Some(&None));
    assert_eq!(run.tx(0).state[&EMPTY].info.balance, U256::from(1_000));
    assert_eq!(run.tx(0).result.logs().len(), 1, "the transfer log");
    let created = CONTRACT.create(1);
    assert!(run.tx(1).state[&created].is_selfdestructed());
    let _ = SELFDESTRUCT;
}

/// A value transfer that creates its recipient: the recipient is absent in the record, created
/// in the state, priced through SALT, and its transfer log is in the receipt.
#[test]
fn test_a_transfer_creating_its_recipient_replays() {
    let replay = Case::new("transfer", common::database())
        .tx(call_with_value(0, EMPTY, U256::from(7), Bytes::new(), CREATION_GAS))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert_eq!(run.record.accounts.get(&EMPTY), Some(&None));
    assert_eq!(run.receipts[0].logs().len(), 1, "the transfer log");
    if !common::state_is_free() {
        assert_eq!(run.bucket_ids.len(), 1, "the recipient's bucket");
    }
}

/// The L1 block info slots a user transaction is priced against.
fn l1_slots() -> [U256; 5] {
    [
        L1_BASE_FEE_SLOT,
        ECOTONE_L1_BLOB_BASE_FEE_SLOT,
        ECOTONE_L1_FEE_SCALARS_SLOT,
        OPERATOR_FEE_SCALARS_SLOT,
        DA_FOOTPRINT_GAS_SCALAR_SLOT,
    ]
}

/// A deposit minting to a fresh depositor: the depositor's absent account is read (for the
/// receipt's nonce and as the caller), created and priced, and the deposit writes its slot. A
/// block of deposits alone reads no L1 block info.
#[test]
fn test_a_deposit_only_block_replays_and_reads_no_l1_info() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let replay = Case::new("deposits", db)
        .tx(deposit(
            DEPOSITOR,
            TxKind::Call(CONTRACT),
            1_000_000_000,
            U256::ZERO,
            slot(9),
            write_gas(),
        ))
        .tx(deposit(DEPOSITOR, TxKind::Call(CONTRACT), 0, U256::ZERO, slot(10), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert_eq!(run.tx(0).depositor_nonce, Some(0));
    assert_eq!(run.tx(1).depositor_nonce, Some(1), "the nonce the first deposit left");
    assert_eq!(run.record.accounts.get(&DEPOSITOR), Some(&None));
    assert!(
        !run.record.accounts.contains_key(&L1_BLOCK_CONTRACT),
        "a deposit is priced against no L1 info and has no footprint"
    );
    if !common::state_is_free() {
        assert!(run.bucket_ids.len() >= 2, "the depositor's bucket and the slots'");
    }
}

/// A user transaction is priced against the L1 block info, read straight from the database and
/// not through the journal: the L1 block contract's account and its slots are in the record —
/// what a witness built from the transaction's state alone would miss.
#[test]
fn test_a_user_transaction_reads_the_l1_block_info_outside_the_journal() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    // The L1 block contract exists, as the predeploy does on a chain: an absent account's slots
    // are known to be zero without a read.
    db.set_account_code(L1_BLOCK_CONTRACT, Bytes::from(vec![0x00]));
    let replay = Case::new("l1 info", db).tx(call(0, CONTRACT, slot(1), write_gas())).run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success());
    assert!(run.record.accounts.get(&L1_BLOCK_CONTRACT).is_some_and(Option::is_some), "read");
    for slot in l1_slots() {
        assert_eq!(run.record.storage.get(&(L1_BLOCK_CONTRACT, slot)), Some(&U256::ZERO), "{slot}");
    }
    assert!(
        !run.tx(0).state.contains_key(&L1_BLOCK_CONTRACT),
        "the L1 block contract is in no transaction's state"
    );
    assert!(
        !run.pre_block.iter().any(|(_, state)| state.contains_key(&L1_BLOCK_CONTRACT)),
        "nor in any pre-block state"
    );
}
