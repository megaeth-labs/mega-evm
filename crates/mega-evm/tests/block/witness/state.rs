//! State-changing mechanisms through the harness: an EIP-7702 delegation and its replacement, a
//! creation, the two shapes of `SELFDESTRUCT`, a value transfer that creates its recipient, and
//! deposits — with the L1 block info a user transaction is priced against, which the pre-block
//! phase carries.

use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    PreBlockStateSource,
};
use op_revm::constants::{
    BASE_FEE_SCALAR_OFFSET, BLOB_BASE_FEE_SCALAR_OFFSET, ECOTONE_L1_BLOB_BASE_FEE_SLOT,
    ECOTONE_L1_FEE_SCALARS_SLOT, L1_BASE_FEE_SLOT, L1_BLOCK_CONTRACT, L1_FEE_RECIPIENT,
    L1_OVERHEAD_SLOT, OPERATOR_FEE_RECIPIENT, OPERATOR_FEE_SCALARS_SLOT,
};
use revm::{
    bytecode::opcode::{CODECOPY, PUSH0, RETURN},
    context_interface::cfg::GasId,
    state::{Account, AccountInfo, Bytecode},
};

use super::{
    basics::{slot, slot_writer, write_gas},
    harness::{
        assert_same_run, authorization, call, call_with_value, create, deposit, eip7702, Case,
        Oracle, Run,
    },
};
use crate::{
    common::{self, CALLER, CONTRACT},
    rules::{l1_block_setter, scalars_word},
};

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

/// A gas limit with room for a creation and two new accounts at the byte prices in effect:
/// 1,000,000 of regular gas on top of the state of two new accounts, a fresh slot and a short
/// runtime's deposit, a kilobyte of history beyond the body, and 64 times the history of two
/// write records — a contract that creates, or a factory that forwards all but a 64th of its gas
/// to the creation, pays the records of the frame it starts from the 64th it keeps.
pub(super) fn creation_gas() -> u64 {
    1_000_000 +
        2 * common::new_account_state_gas() +
        common::slot_state_gas() +
        64 * mega_evm::satin_gas_params().get(GasId::code_deposit_state_gas()) +
        common::body_history(1_000) +
        64 * mega_evm::write_record_history_gas(2).expect("two records have a price")
}

/// An EIP-7702 call to an authority delegating to the slot writer: the authority's absent account
/// is in the record, it is created with its delegation, and its slot is written.
#[test]
fn test_a_delegation_replays() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let (signed, authority) = authorization(CONTRACT, 0);
    let replay = Case::new("delegation", db)
        .tx(eip7702(0, authority, slot(5), vec![signed], creation_gas()))
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

/// An authority the chain holds delegated to one contract is delegated anew: the engine loads
/// the delegation the chain holds to admit the authorization, and the returned state carries the
/// one the transaction wrote over it. The witness holds the authority's code as the chain held
/// it, which a witness built from the code the returned states carry would lack.
#[test]
fn test_a_replaced_delegation_replays_from_the_code_the_chain_held() {
    let (signed, authority) = authorization(CONTRACT, 1);
    let held = Bytecode::new_eip7702(EMPTY);
    let written = Bytecode::new_eip7702(CONTRACT);
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    db.insert_account_info(
        authority,
        AccountInfo {
            nonce: 1,
            code_hash: held.hash_slow(),
            code: Some(held.clone()),
            ..Default::default()
        },
    );
    let case = Case::new("replaced delegation", db).tx(eip7702(
        0,
        authority,
        slot(5),
        vec![signed],
        creation_gas(),
    ));

    let recorded = case.record();
    assert!(recorded.tx(0).result.is_success(), "{:?}", recorded.tx(0).result);
    let account = &recorded.tx(0).state[&authority];
    assert_eq!(account.info.nonce, 2, "the authorization was applied");
    assert_eq!(account.info.code.as_ref(), Some(&written), "the state carries the new delegation");
    assert!(account.storage.get(&U256::from(5)).is_some_and(|slot| slot.is_changed()));
    assert_eq!(
        recorded.record.codes.get(&held.hash_slow()),
        Some(&held),
        "the engine loaded the delegation the chain held"
    );
    let witness = case.channel_witness(&recorded);
    assert_eq!(witness.codes.get(&held.hash_slow()), Some(&held), "which the witness holds");
    assert!(!witness.codes.contains_key(&written.hash_slow()), "and not the one the block wrote");

    case.run();
}

/// A creation transaction, then a call to what it created.
#[test]
fn test_a_creation_replays() {
    let created = CALLER.create(0);
    let replay = Case::new("creation", common::database())
        .tx(create(0, deploying(&[0x00; 5]), creation_gas()))
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
        .tx(call(0, DESTROYER, Bytes::new(), creation_gas()))
        .tx(call(1, CONTRACT, Bytes::new(), creation_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert_eq!(run.record.accounts.get(&EMPTY), Some(&None));
    assert_eq!(run.tx(0).state[&EMPTY].info.balance, U256::from(1_000));
    assert_eq!(run.tx(0).result.logs().len(), 1, "the transfer log");
    let created = CONTRACT.create(1);
    assert!(run.tx(1).state[&created].is_selfdestructed());
}

/// A value transfer that creates its recipient: the recipient is absent in the record, created
/// in the state, priced through SALT, and its transfer log is in the receipt.
#[test]
fn test_a_transfer_creating_its_recipient_replays() {
    let replay = Case::new("transfer", common::database())
        .tx(call_with_value(0, EMPTY, U256::from(7), Bytes::new(), creation_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert_eq!(run.record.accounts.get(&EMPTY), Some(&None));
    assert_eq!(run.receipts[0].logs().len(), 1, "the transfer log");
    if !common::state_is_free() {
        assert_eq!(run.bucket_ids.len(), 1, "the recipient's bucket");
    }
}

/// The L1 attributes depositor, whose deposit sets the L1 block info every block.
const L1_ATTRIBUTES_DEPOSITOR: Address = address!("0xDeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001");

/// The DA footprint gas scalar the chain holds before the block, and the one the block's L1
/// attributes deposit writes.
const PARENT_FOOTPRINT_SCALAR: u16 = 400;
const BLOCK_FOOTPRINT_SCALAR: u16 = 800;

/// The word the L1 block contract holds at its Ecotone fee-scalars slot: the base fee scalar and
/// the blob base fee scalar at the offsets the fork reads them from.
fn fee_scalars_word(base_fee_scalar: u32, blob_base_fee_scalar: u32) -> U256 {
    let mut word = [0_u8; 32];
    word[BASE_FEE_SCALAR_OFFSET..BASE_FEE_SCALAR_OFFSET + 4]
        .copy_from_slice(&base_fee_scalar.to_be_bytes());
    word[BLOB_BASE_FEE_SCALAR_OFFSET..BLOB_BASE_FEE_SCALAR_OFFSET + 4]
        .copy_from_slice(&blob_base_fee_scalar.to_be_bytes());
    U256::from_be_bytes(word)
}

/// The operator fee scalars word the chain holds before the block.
fn parent_scalars_word() -> U256 {
    scalars_word(PARENT_FOOTPRINT_SCALAR, 5, 7)
}

/// The L1 block info the chain holds before the block: a distinct non-zero value in every slot
/// the transactions are priced against, so a slot a witness answered with zero would show in the
/// L1 fee, the operator fee or the footprint. The Ecotone scalars are set, so the pricing does not
/// read the overhead, which the chain holds beside them ([`OVERHEAD`]).
fn l1_info() -> [(U256, U256); 4] {
    [
        (L1_BASE_FEE_SLOT, U256::from(1_000_000_000_u64)),
        (ECOTONE_L1_FEE_SCALARS_SLOT, fee_scalars_word(1_000_000, 1_000_000)),
        (ECOTONE_L1_BLOB_BASE_FEE_SLOT, U256::from(1_000_000_000_u64)),
        (OPERATOR_FEE_SCALARS_SLOT, parent_scalars_word()),
    ]
}

/// The L1 fee overhead the chain holds, which the pricing reads only when the scalars it finds
/// are empty, and the pre-block entry carries whatever they hold.
const OVERHEAD: U256 = U256::from_limbs([188, 0, 0, 0]);

/// A chain holding the L1 block contract with `code`, the info of [`l1_info`] and the overhead,
/// and the slot writer at [`CONTRACT`].
fn chain_with_l1_info(code: Bytes) -> MemoryDatabase {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    db.set_account_code(L1_BLOCK_CONTRACT, code);
    for (slot, value) in l1_info() {
        db.set_account_storage(L1_BLOCK_CONTRACT, slot, value);
    }
    db.set_account_storage(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT, OVERHEAD);
    db
}

/// The L1 block info entry of a run's pre-block states: the last state, and the one entry in it.
fn l1_entry(run: &Run) -> &Account {
    let (source, state) = run.pre_block.last().expect("a pre-block state");
    assert_eq!(*source, PreBlockStateSource::L1BlockInfo, "the L1 block info is the last state");
    assert_eq!(state.len(), 1, "the L1 block contract alone");
    &state[&L1_BLOCK_CONTRACT]
}

/// A deposit minting to a fresh depositor: the depositor's absent account is read (for the
/// receipt's nonce and as the caller), created and priced, and the deposit writes its slot. A
/// block of deposits alone prices no transaction against the L1 block info; the pre-block phase
/// reads it all the same, here from a chain that does not hold the contract, which the entry
/// records as not existing without a slot read.
#[test]
fn test_a_deposit_only_block_replays_and_reads_no_l1_slot() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    // The first deposit also pays for the account the engine creates for its caller.
    let replay = Case::new("deposits", db)
        .tx(deposit(
            DEPOSITOR,
            TxKind::Call(CONTRACT),
            1_000_000_000,
            U256::ZERO,
            slot(9),
            write_gas() + common::new_account_state_gas(),
        ))
        .tx(deposit(DEPOSITOR, TxKind::Call(CONTRACT), 0, U256::ZERO, slot(10), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert_eq!(run.tx(0).depositor_nonce, Some(0));
    assert_eq!(run.tx(1).depositor_nonce, Some(1), "the nonce the first deposit left");
    assert_eq!(run.record.accounts.get(&DEPOSITOR), Some(&None));
    assert!(l1_entry(run).is_loaded_as_not_existing(), "the chain holds no L1 block contract");
    assert_eq!(run.record.accounts.get(&L1_BLOCK_CONTRACT), Some(&None), "read once, pre-block");
    assert!(
        !run.record.storage.keys().any(|(address, _)| *address == L1_BLOCK_CONTRACT),
        "no slot of an absent contract is read"
    );
    if !common::state_is_free() {
        assert!(run.bucket_ids.len() >= 2, "the depositor's bucket and the slots'");
    }
}

/// A user transaction is priced against the L1 block info, which is read on the database itself
/// and not through the transaction's journal: the L1 block contract is in no transaction's state,
/// and the pre-block phase carries its account and slots as read-only entries with the chain's
/// values, so a witness built from the states holds what the transaction's L1 fee, operator fee
/// and footprint were computed from. The entry holds the overhead beside them, which the pricing
/// does not read while the scalars are set: the pre-block read is the one read of it.
#[test]
fn test_a_user_transaction_is_priced_against_the_l1_info_the_pre_block_state_carries() {
    let db = chain_with_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("l1 info", db).tx(call(0, CONTRACT, slot(1), write_gas())).run();
    let run = &replay.recorded;
    let tx = run.tx(0);
    assert!(tx.result.is_success(), "{:?}", tx.result);
    assert!(!tx.state.contains_key(&L1_BLOCK_CONTRACT), "in no transaction's state");

    let account = l1_entry(run);
    assert!(!account.is_touched() && !account.is_created(), "a read-only entry");
    assert_eq!(account.info.code_hash, alloy_primitives::keccak256([0x00]));
    let held = l1_info().into_iter().chain([(L1_OVERHEAD_SLOT, OVERHEAD)]);
    assert_eq!(account.storage.len(), held.clone().count(), "the slots the pricing can read");
    for (slot, value) in held {
        let entry = account.storage.get(&slot).expect("every slot of the set");
        assert_eq!(entry.present_value, value, "{slot}");
        assert!(!entry.is_changed(), "{slot} is unchanged");
        assert_eq!(
            run.record.storage.get(&(L1_BLOCK_CONTRACT, slot)),
            Some(&value),
            "{slot} was read from the database, before the transactions"
        );
    }

    let footprint_scalar = u64::from(PARENT_FOOTPRINT_SCALAR);
    assert_eq!(
        tx.da_footprint,
        tx.da_size * footprint_scalar,
        "the footprint the chain's scalar gives"
    );
    let credited = |vault: Address| tx.state.get(&vault).map_or(U256::ZERO, |a| a.info.balance);
    assert!(!credited(L1_FEE_RECIPIENT).is_zero(), "an L1 fee was paid from the info");
    assert!(!credited(OPERATOR_FEE_RECIPIENT).is_zero(), "an operator fee was paid from the info");
}

/// With the Ecotone scalars empty before the block the pricing reads the overhead at the first
/// transaction it prices; the pre-block entry holds the same five slots as with set scalars, the
/// overhead among them, and the block replays.
#[test]
fn test_the_overhead_is_in_the_entry_when_the_scalars_are_empty() {
    let mut db = chain_with_l1_info(Bytes::from(vec![0x00]));
    db.set_account_storage(L1_BLOCK_CONTRACT, ECOTONE_L1_FEE_SCALARS_SLOT, U256::ZERO);
    let replay = Case::new("l1 overhead", db).tx(call(0, CONTRACT, slot(1), write_gas())).run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    let account = l1_entry(run);
    assert_eq!(account.storage.len(), l1_info().len() + 1, "the same set as with set scalars");
    let overhead = account.storage.get(&L1_OVERHEAD_SLOT).expect("in the entry");
    assert_eq!(overhead.present_value, OVERHEAD);
    assert!(!overhead.is_changed());
    assert_eq!(run.record.storage.get(&(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT)), Some(&OVERHEAD));
}

/// The production shape: the L1 attributes deposit comes first and writes the block's info over
/// the parent's. The pre-block entry holds the parent's values, the deposit's own returned state
/// holds the write, and the transaction after the deposit is priced against the write; so
/// everything its footprint was computed from is in one of the block's states.
#[test]
fn test_the_l1_attributes_deposit_first_prices_the_transactions_after_it() {
    let db = chain_with_l1_info(l1_block_setter());
    let word = scalars_word(BLOCK_FOOTPRINT_SCALAR, 9, 11);
    let replay = Case::new("l1 attributes deposit first", db)
        .tx(deposit(
            L1_ATTRIBUTES_DEPOSITOR,
            TxKind::Call(L1_BLOCK_CONTRACT),
            0,
            U256::ZERO,
            Bytes::from(word.to_be_bytes::<32>()),
            200_000 + common::new_account_state_gas(),
        ))
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert!(run.tx(1).result.is_success(), "{:?}", run.tx(1).result);

    let before = l1_entry(run).storage.get(&OPERATOR_FEE_SCALARS_SLOT).expect("in the entry");
    assert_eq!(before.present_value, parent_scalars_word(), "the parent's info, pre-block");
    let written = &run.tx(0).state[&L1_BLOCK_CONTRACT].storage[&OPERATOR_FEE_SCALARS_SLOT];
    assert_eq!(written.original_value, parent_scalars_word());
    assert_eq!(written.present_value, word, "the deposit's state carries the write");
    assert_eq!(run.tx(0).da_footprint, 0, "a deposit has no footprint");

    let tx = run.tx(1);
    assert_eq!(
        tx.da_footprint,
        tx.da_size * u64::from(BLOCK_FOOTPRINT_SCALAR),
        "priced against the deposit's write"
    );
    assert_ne!(tx.da_footprint, tx.da_size * u64::from(PARENT_FOOTPRINT_SCALAR));
}

/// Code that empties the Ecotone fee scalars: a write to the L1 block contract, which the engine
/// accepts from a deposit like any other.
fn scalars_emptier() -> Bytes {
    BytecodeBuilder::default().sstore(ECOTONE_L1_FEE_SCALARS_SLOT, U256::ZERO).stop().build()
}

/// A block whose first transaction is a deposit that empties the Ecotone scalars, on a chain
/// that holds them set and holds `overhead`, followed by a user transaction.
fn scalars_emptied_in_the_block(overhead: U256) -> Case {
    let mut db = chain_with_l1_info(scalars_emptier());
    db.set_account_storage(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT, overhead);
    Case::new("scalars emptied in the block", db)
        .tx(deposit(
            L1_ATTRIBUTES_DEPOSITOR,
            TxKind::Call(L1_BLOCK_CONTRACT),
            0,
            U256::ZERO,
            Bytes::new(),
            200_000 + common::new_account_state_gas(),
        ))
        .tx(call(0, CONTRACT, slot(1), write_gas()))
}

/// The scalars are set before the block and a deposit empties them, so the L1 info the next
/// transaction is priced against is fetched with empty scalars and reads the overhead. No
/// transaction's state names that slot: the deposit wrote the scalars alone, and the fetch is not
/// in the user transaction's journal. The pre-block entry carries the overhead whatever the
/// scalars hold before the block, so the channel witness holds it and the block replays.
#[test]
fn test_a_deposit_emptying_the_scalars_leaves_the_overhead_in_the_witness() {
    let case = scalars_emptied_in_the_block(OVERHEAD);
    let recorded = case.record();
    assert!(recorded.tx(0).result.is_success(), "{:?}", recorded.tx(0).result);
    assert!(recorded.tx(1).result.is_success(), "{:?}", recorded.tx(1).result);

    let written = &recorded.tx(0).state[&L1_BLOCK_CONTRACT].storage;
    let scalars = &written[&ECOTONE_L1_FEE_SCALARS_SLOT];
    assert_eq!(scalars.original_value, fee_scalars_word(1_000_000, 1_000_000), "set before");
    assert_eq!(scalars.present_value, U256::ZERO, "the deposit emptied the scalars");
    assert!(!written.contains_key(&L1_OVERHEAD_SLOT), "the deposit's state names no overhead");
    let tx = recorded.tx(1);
    assert!(!tx.state.contains_key(&L1_BLOCK_CONTRACT), "nor does the transaction after it");
    assert!(
        tx.state.get(&L1_FEE_RECIPIENT).is_none_or(|vault| vault.info.balance.is_zero()),
        "priced against the scalars the deposit left: no L1 fee"
    );
    assert_eq!(
        recorded.record.storage.get(&(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT)),
        Some(&OVERHEAD),
        "the block read the overhead from the database"
    );

    let witness = case.channel_witness(&recorded);
    let channel = case.replay(&witness, &recorded.included(), Oracle::Recorded);
    assert!(
        channel.txs[1].is_ok(),
        "the transaction after the deposit replays from the channel witness: {:?}",
        channel.txs[1].as_ref().err()
    );
    assert_eq!(
        witness.storage.get(&(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT)),
        Some(&OVERHEAD),
        "which holds the overhead"
    );
    let entry = l1_entry(&recorded).storage.get(&L1_OVERHEAD_SLOT).expect("in the entry");
    assert_eq!(entry.present_value, OVERHEAD, "named by the pre-block entry, with set scalars");

    case.run();
}

/// The overhead is a slot the block must be able to read, and nothing more: the fee of a
/// transaction priced with empty scalars does not take it in, so the same block on a chain that
/// holds another overhead produces the same transactions, receipts and state changes.
#[test]
fn test_the_overhead_a_block_reads_decides_no_result() {
    let held = scalars_emptied_in_the_block(OVERHEAD).run().recorded;
    let zero = scalars_emptied_in_the_block(U256::ZERO).run().recorded;
    assert_eq!(l1_entry(&held).storage[&L1_OVERHEAD_SLOT].present_value, OVERHEAD);
    assert_eq!(l1_entry(&zero).storage[&L1_OVERHEAD_SLOT].present_value, U256::ZERO);
    assert_eq!(held.txs, zero.txs, "the transactions");
    assert_eq!(held.receipts, zero.receipts, "the receipts");
    assert_eq!(held.bundle, zero.bundle, "the state changes");
    assert_eq!((held.gas_used, held.blob_gas_used), (zero.gas_used, zero.blob_gas_used));
}

/// The entry's other condition: a chain that does not hold the L1 block contract has it recorded
/// as not existing, with no slot. A deposit that creates the account in the block does not turn
/// its slots into database reads: the slots of an account the block created are the block's own,
/// so the transaction priced after the deposit reads none from the chain, and the witness, which
/// holds the account as absent and no slot of it, replays the block.
#[test]
fn test_a_deposit_creating_the_l1_block_contract_adds_no_slot_read() {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    let case = Case::new("L1 block contract created in the block", db)
        .tx(deposit(
            DEPOSITOR,
            TxKind::Call(L1_BLOCK_CONTRACT),
            1_000,
            U256::from(1_000),
            Bytes::new(),
            200_000 + 2 * common::new_account_state_gas(),
        ))
        .tx(call(0, CONTRACT, slot(1), write_gas()));
    let recorded = case.record();
    assert!(recorded.tx(0).result.is_success(), "{:?}", recorded.tx(0).result);
    assert!(recorded.tx(1).result.is_success(), "{:?}", recorded.tx(1).result);
    assert!(l1_entry(&recorded).is_loaded_as_not_existing(), "absent before the block");
    let created = &recorded.tx(0).state[&L1_BLOCK_CONTRACT];
    assert_eq!(created.info.balance, U256::from(1_000), "the deposit created the account");
    assert!(
        !recorded.record.storage.keys().any(|(address, _)| *address == L1_BLOCK_CONTRACT),
        "no slot of it is read from the database, before the deposit or after"
    );

    let witness = case.channel_witness(&recorded);
    assert_eq!(witness.accounts.get(&L1_BLOCK_CONTRACT), Some(&None), "held as absent");
    assert!(!witness.storage.keys().any(|(address, _)| *address == L1_BLOCK_CONTRACT));
    let channel = case.replay(&witness, &recorded.included(), Oracle::Recorded);
    assert_same_run("L1 block contract created in the block", &recorded, &channel);

    case.run();
}
