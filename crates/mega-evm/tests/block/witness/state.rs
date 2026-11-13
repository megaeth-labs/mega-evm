//! State-changing mechanisms through the harness: an EIP-7702 delegation and its replacement, a
//! creation, the two shapes of `SELFDESTRUCT`, a value transfer that creates its recipient, and
//! deposits — with the L1 block info a transaction is priced against and the footprint scalar it
//! is held to, which the first transaction that reads them reports, and which nothing reads
//! before the block's transactions.

use alloy_evm::{
    block::{BlockExecutionError, BlockExecutor, BlockValidationError},
    EvmFactory,
};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, TxKind, U256};
use mega_evm::{
    test_utils::{zero_fee_l1_block_info, BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    MegaBlockExecutor, MegaEvmFactory,
};
use op_revm::constants::{
    BASE_FEE_SCALAR_OFFSET, BLOB_BASE_FEE_SCALAR_OFFSET, ECOTONE_L1_BLOB_BASE_FEE_SLOT,
    ECOTONE_L1_FEE_SCALARS_SLOT, L1_BASE_FEE_SLOT, L1_BLOCK_CONTRACT, L1_FEE_RECIPIENT,
    L1_OVERHEAD_SLOT, OPERATOR_FEE_RECIPIENT, OPERATOR_FEE_SCALARS_SLOT,
};
use revm::{
    bytecode::opcode::{CODECOPY, PUSH0, RETURN},
    context_interface::cfg::GasId,
    database::State,
    state::{Account, AccountInfo, Bytecode, EvmState},
};

use super::{
    basics::{slot, slot_writer, write_gas},
    harness::{
        assert_same_run, authorization, call, call_with_value, create, deposit, eip7702, Case,
        Oracle, Run, Tx, TxSnapshot,
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
/// runtime's deposit, a kilobyte of history beyond the body, and 50 times the history of two
/// write records — a contract that creates, or a factory that forwards all but 2 per cent of its
/// gas to the creation, pays the records of the frame it starts from the fiftieth it keeps.
pub(super) fn creation_gas() -> u64 {
    1_000_000 +
        2 * common::new_account_state_gas() +
        common::slot_state_gas() +
        64 * mega_evm::satin_gas_params().get(GasId::code_deposit_state_gas()) +
        common::body_history(1_000) +
        50 * mega_evm::write_record_history_gas(2).expect("two records have a price")
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
/// are empty.
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

/// The L1 block contract's entry in `state`.
fn l1_account(state: &EvmState) -> &Account {
    state.get(&L1_BLOCK_CONTRACT).expect("the state names the L1 block contract")
}

/// Asserts `account` is a read-only entry — neither touched nor created — holding exactly
/// `slots`, each unchanged at its value.
fn assert_read_only(what: &str, account: &Account, slots: &[(U256, U256)]) {
    assert!(!account.is_touched() && !account.is_created(), "{what}: a read-only entry");
    let named: Vec<_> = account.storage.keys().copied().collect();
    assert_eq!(account.storage.len(), slots.len(), "{what}: the slots named: {named:?}");
    for (slot, value) in slots {
        let entry = account.storage.get(slot).unwrap_or_else(|| panic!("{what}: no slot {slot}"));
        assert_eq!(entry.present_value, *value, "{what}: slot {slot}");
        assert!(!entry.is_changed(), "{what}: slot {slot} is unchanged");
    }
}

/// How many of the states `run` committed name the L1 block contract's `slot`, or its account
/// when `slot` is `None`.
fn times_named(run: &Run, slot: Option<U256>) -> usize {
    run.committed
        .iter()
        .filter_map(|state| state.get(&L1_BLOCK_CONTRACT))
        .filter(|account| slot.is_none_or(|slot| account.storage.contains_key(&slot)))
        .count()
}

/// Whether any pre-block state of `run` names the L1 block contract.
fn pre_block_names_l1(run: &Run) -> bool {
    run.pre_block.iter().any(|state| state.contains_key(&L1_BLOCK_CONTRACT))
}

/// A block of deposits alone prices nothing against the L1 block info and has no footprint, so
/// nothing of the L1 block contract is read: no pre-block state and no transaction's state names
/// it, and the database is asked for neither its account nor a slot. The block replays.
#[test]
fn test_a_block_of_deposits_reads_nothing_of_the_l1_block_contract() {
    let db = chain_with_l1_info(Bytes::from(vec![0x00]));
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
    assert!(!pre_block_names_l1(run), "no pre-block read of the L1 block contract");
    assert_eq!(times_named(run, None), 0, "no state names it");
    assert!(!run.record.accounts.contains_key(&L1_BLOCK_CONTRACT), "its account is not read");
    assert!(
        !run.record.storage.keys().any(|(address, _)| *address == L1_BLOCK_CONTRACT),
        "nor is a slot of it"
    );
    if !common::state_is_free() {
        assert!(run.bucket_ids.len() >= 2, "the depositor's bucket and the slots'");
    }
}

/// Nothing reads the L1 block contract before the block's transactions, so a database that
/// cannot serve its account or one of its slots starts the block and runs a block of deposits,
/// which reads none of them.
#[test]
fn test_a_block_that_reads_no_l1_slot_runs_on_a_database_that_cannot_serve_one() {
    let refusals = [
        ("the account", Some(L1_BLOCK_CONTRACT), None),
        ("the L1 base fee", None, Some((L1_BLOCK_CONTRACT, L1_BASE_FEE_SLOT))),
        ("the overhead", None, Some((L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT))),
        ("the footprint scalar", None, Some((L1_BLOCK_CONTRACT, OPERATOR_FEE_SCALARS_SLOT))),
    ];
    for (what, account, refused) in refusals {
        let mut db = ErrorInjectingDatabase::new(chain_with_l1_info(Bytes::from(vec![0x00])));
        db.fail_on_account = account;
        db.fail_on_storage = refused;
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let evm = MegaEvmFactory::new().create_evm(&mut state, common::evm_env());
        let mut executor = MegaBlockExecutor::new(
            evm,
            common::unlimited_ctx(),
            common::chain_spec(),
            OpAlloyReceiptBuilder::default(),
        );
        executor.apply_pre_execution_changes().unwrap_or_else(|err| panic!("{what}: {err}"));
        let tx = deposit(
            DEPOSITOR,
            TxKind::Call(CONTRACT),
            1_000_000_000,
            U256::ZERO,
            slot(9),
            write_gas() + common::new_account_state_gas(),
        );
        executor.execute_transaction(&tx).unwrap_or_else(|err| panic!("{what}: {err}"));
        executor.finish().unwrap_or_else(|err| panic!("{what}: {err}"));
    }
}

/// A user transaction is priced against the L1 block info and held to the footprint by its
/// scalar, both read on the database itself and not through the transaction's journal. The first
/// transaction that reads them reports them, as read-only entries of its own state: the account
/// and the four slots its fee and footprint were computed from, with the chain's values. The
/// overhead, which the pricing does not read while the scalars are set, is not read, and no
/// pre-block state names the contract. The block replays from both witnesses.
#[test]
fn test_the_first_transaction_priced_reports_the_l1_info_it_read() {
    let db = chain_with_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("l1 info", db).tx(call(0, CONTRACT, slot(1), write_gas())).run();
    let run = &replay.recorded;
    let tx = run.tx(0);
    assert!(tx.result.is_success(), "{:?}", tx.result);
    assert!(!pre_block_names_l1(run), "no pre-block read of the L1 block contract");

    let account = l1_account(&tx.state);
    assert_read_only("the transaction's report", account, &l1_info());
    assert_eq!(account.info.code_hash, alloy_primitives::keccak256([0x00]));
    for (slot, value) in l1_info() {
        assert_eq!(
            run.record.storage.get(&(L1_BLOCK_CONTRACT, slot)),
            Some(&value),
            "{slot} was read from the database"
        );
    }
    assert_eq!(
        run.record.storage.get(&(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT)),
        None,
        "the overhead the set scalars leave unread is not read"
    );

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
/// transaction it prices, which reports it beside the other four slots, and the block replays.
#[test]
fn test_the_overhead_is_reported_when_the_scalars_are_empty() {
    let mut db = chain_with_l1_info(Bytes::from(vec![0x00]));
    db.set_account_storage(L1_BLOCK_CONTRACT, ECOTONE_L1_FEE_SCALARS_SLOT, U256::ZERO);
    let replay = Case::new("l1 overhead", db).tx(call(0, CONTRACT, slot(1), write_gas())).run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    let mut read = l1_info().to_vec();
    read[1].1 = U256::ZERO;
    read.push((L1_OVERHEAD_SLOT, OVERHEAD));
    assert_read_only("the transaction's report", l1_account(&run.tx(0).state), &read);
    assert_eq!(run.record.storage.get(&(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT)), Some(&OVERHEAD));
}

/// Two transactions are priced against the same L1 block info and read the same footprint scalar:
/// the first reports the account and the slots, and the second, whose reads they are too, reports
/// nothing again. The block replays.
#[test]
fn test_only_the_first_transaction_that_reads_the_l1_info_reports_it() {
    let db = chain_with_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("two readers", db)
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .tx(call(1, CONTRACT, slot(2), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert_read_only("the first reader", l1_account(&run.tx(0).state), &l1_info());
    assert!(!run.tx(1).state.contains_key(&L1_BLOCK_CONTRACT), "the second reports nothing");
    assert_eq!(times_named(run, None), 1, "the account is named once");
    for (slot, _) in l1_info() {
        assert_eq!(times_named(run, Some(slot)), 1, "slot {slot} is named once");
    }
}

/// A candidate the builder executes and drops reports what it read, but its state is not
/// committed: the first transaction of the block reports it in its place. The block replays.
#[test]
fn test_a_dropped_first_reader_leaves_the_report_to_the_next() {
    let db = chain_with_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("dropped first reader", db)
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .dropped(0)
        .tx(call(0, CONTRACT, slot(2), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.refusal(0).contains("dropped"), "{}", run.refusal(0));
    assert!(run.tx(1).result.is_success(), "{:?}", run.tx(1).result);
    assert_read_only("the first included reader", l1_account(&run.tx(1).state), &l1_info());
    assert_eq!(times_named(run, None), 1);
}

/// A second sender, funded on [`chain_with_free_l1_info`].
const OTHER: Address = address!("0x4000000000000000000000000000000000000007");

/// A chain holding the L1 block contract with `code` and nothing to charge: every slot the fee
/// computation and the footprint read hold zero but the overhead, which the empty scalars have
/// the fee computation read; the slot writer at [`CONTRACT`]; [`CALLER`] and [`OTHER`] funded.
///
/// A transaction at no gas price pays no fee on it, so an outcome executed on another state of
/// the block — on another executor, or before another commit — credits the fee accounts as
/// that state would: it is not stale for them, and the block it is committed into is the block a
/// validator replays.
fn chain_with_free_l1_info(code: Bytes) -> MemoryDatabase {
    let mut db = common::database();
    db.set_account_code(CONTRACT, slot_writer());
    db.set_account_code(L1_BLOCK_CONTRACT, code);
    db.set_account_storage(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT, OVERHEAD);
    db.set_account_balance(OTHER, U256::from(1_000_000_000_000_000_u64));
    db
}

/// What the fee computation reads of [`chain_with_free_l1_info`], the footprint scalar's slot
/// among it.
fn free_l1_info() -> [(U256, U256); 5] {
    [
        (L1_BASE_FEE_SLOT, U256::ZERO),
        (ECOTONE_L1_FEE_SCALARS_SLOT, U256::ZERO),
        (L1_OVERHEAD_SLOT, OVERHEAD),
        (ECOTONE_L1_BLOB_BASE_FEE_SLOT, U256::ZERO),
        (OPERATOR_FEE_SCALARS_SLOT, U256::ZERO),
    ]
}

/// A call from `sender` at `nonce` to `to` carrying `input`, at no gas price.
fn free_call(sender: Address, nonce: u64, to: Address, input: Bytes) -> Tx {
    common::legacy_from(sender, nonce, TxKind::Call(to), U256::ZERO, input, write_gas(), 0)
}

/// Asserts the block `run` committed names the L1 block contract's account and each slot of
/// `read` exactly once, all of it in the state of the transaction at `index`, read only.
fn assert_reported_once_by(run: &Run, index: usize, read: &[(U256, U256)]) {
    assert_read_only("the first transaction committed", l1_account(&run.tx(index).state), read);
    assert_eq!(times_named(run, None), 1, "the account is named once");
    for (slot, _) in read {
        assert_eq!(times_named(run, Some(*slot)), 1, "slot {slot} is named once");
    }
}

/// A builder that executes on several executors of one block commits on this executor an
/// outcome another executed after it had committed a transaction of its own, which reported the
/// L1 block contract there. This executor has reported nothing: the transaction is the first it
/// commits, so its committed state reports the account and every slot its fee and footprint
/// reads reached, and the block replays from the witness built from what this executor committed.
#[test]
fn test_an_outcome_executed_on_another_executor_is_reported_where_it_is_committed() {
    let db = chain_with_free_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("executed on another executor after it reported", db)
        .tx(free_call(OTHER, 0, CONTRACT, slot(2)))
        .executed_elsewhere(0, vec![free_call(CALLER, 0, CONTRACT, slot(1))])
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert_reported_once_by(run, 0, &free_l1_info());
}

/// The other way round: an outcome executed on another executor that had reported nothing, so
/// its returned state carries the report there, is committed on this executor after a
/// transaction of its own reported the contract. Its committed state names nothing of the
/// contract, and the block replays.
#[test]
fn test_an_outcome_executed_on_another_executor_reports_nothing_this_one_reported() {
    let db = chain_with_free_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("executed on another executor before it reported", db)
        .tx(free_call(CALLER, 0, CONTRACT, slot(1)))
        .tx(free_call(OTHER, 0, CONTRACT, slot(2)))
        .executed_elsewhere(1, Vec::new())
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert!(!run.tx(1).state.contains_key(&L1_BLOCK_CONTRACT), "the second reports nothing");
    assert_reported_once_by(run, 0, &free_l1_info());
}

/// Two candidates executed before either is committed both read the L1 block contract while no
/// committed transaction had reported it. The first committed reports it; the second, committed
/// after it, reports nothing again, and the block replays.
#[test]
fn test_two_candidates_executed_before_either_is_committed_report_once() {
    let db = chain_with_free_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("two candidates executed before either is committed", db)
        .tx(free_call(CALLER, 0, CONTRACT, slot(1)))
        .held(0)
        .tx(free_call(OTHER, 0, CONTRACT, slot(2)))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert!(!run.tx(1).state.contains_key(&L1_BLOCK_CONTRACT), "the second reports nothing");
    assert_reported_once_by(run, 0, &free_l1_info());
}

/// A Mega System Transaction pays no L1 fee, so no fee computation prices it, but the block
/// reads the footprint scalar before it as before every non-deposit transaction: as the block's
/// first reader it reports the account and the scalar's slot alone. The user transaction after
/// it, the first the fee computation prices, reports the fee's other slots, and not the scalar's
/// again. Alone in its block, the system transaction reports the same. Both blocks replay.
#[test]
fn test_a_mega_system_transaction_reports_the_footprint_scalar_it_reads() {
    let footprint = [(OPERATOR_FEE_SCALARS_SLOT, parent_scalars_word())];

    let db = chain_with_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("system transaction alone", db).tx(common::system_tx()).run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success(), "{:?}", run.tx(0).result);
    assert_eq!(run.tx(0).gas.history, 0, "the protocol's own transaction");
    assert_read_only("the system transaction", l1_account(&run.tx(0).state), &footprint);
    assert_eq!(run.tx(0).da_footprint, run.tx(0).da_size * u64::from(PARENT_FOOTPRINT_SCALAR));

    let db = chain_with_l1_info(Bytes::from(vec![0x00]));
    let replay = Case::new("system transaction first", db)
        .tx(common::system_tx())
        .tx(call(0, CONTRACT, slot(1), write_gas()))
        .run();
    let run = &replay.recorded;
    assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success());
    assert_read_only("the system transaction", l1_account(&run.tx(0).state), &footprint);
    let fee: Vec<_> =
        l1_info().into_iter().filter(|(slot, _)| *slot != OPERATOR_FEE_SCALARS_SLOT).collect();
    assert_read_only("the first transaction priced", l1_account(&run.tx(1).state), &fee);
    for (slot, _) in l1_info() {
        assert_eq!(times_named(run, Some(slot)), 1, "slot {slot} is named once");
    }
}

/// The production shape: the L1 attributes deposit comes first and writes the block's info over
/// the parent's. The deposit's own returned state holds the write, and reads nothing for a fee or
/// a footprint; the transaction after it is priced against the write and reports what it read,
/// the deposit's values, as read-only entries. Everything its fee and footprint were computed
/// from is in one of the block's states.
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

    let deposit = l1_account(&run.tx(0).state);
    assert_eq!(deposit.storage.len(), 1, "the deposit's state names its write alone");
    let written = &deposit.storage[&OPERATOR_FEE_SCALARS_SLOT];
    assert_eq!(written.original_value, parent_scalars_word());
    assert_eq!(written.present_value, word, "the deposit's state carries the write");
    assert_eq!(run.tx(0).da_footprint, 0, "a deposit has no footprint");

    let mut read = l1_info();
    read[3].1 = word;
    assert_read_only("the transaction after the deposit", l1_account(&run.tx(1).state), &read);

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
/// transaction is priced against is fetched with empty scalars and reads the overhead. The
/// deposit wrote the scalars alone, and names no overhead: the user transaction reports it, once,
/// as a read-only entry of its own state, with the other slots it read, so the channel witness
/// holds it and the block replays.
#[test]
fn test_a_deposit_emptying_the_scalars_has_the_next_transaction_report_the_overhead() {
    let case = scalars_emptied_in_the_block(OVERHEAD);
    let recorded = case.record();
    assert!(recorded.tx(0).result.is_success(), "{:?}", recorded.tx(0).result);
    assert!(recorded.tx(1).result.is_success(), "{:?}", recorded.tx(1).result);

    let written = &l1_account(&recorded.tx(0).state).storage;
    let scalars = &written[&ECOTONE_L1_FEE_SCALARS_SLOT];
    assert_eq!(scalars.original_value, fee_scalars_word(1_000_000, 1_000_000), "set before");
    assert_eq!(scalars.present_value, U256::ZERO, "the deposit emptied the scalars");
    assert!(!written.contains_key(&L1_OVERHEAD_SLOT), "the deposit's state names no overhead");

    let tx = recorded.tx(1);
    let mut read = l1_info().to_vec();
    read[1].1 = U256::ZERO;
    read.push((L1_OVERHEAD_SLOT, OVERHEAD));
    assert_read_only("the transaction after the deposit", l1_account(&tx.state), &read);
    assert_eq!(times_named(&recorded, Some(L1_OVERHEAD_SLOT)), 1, "the overhead, once");
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
    assert_eq!(
        witness.storage.get(&(L1_BLOCK_CONTRACT, L1_OVERHEAD_SLOT)),
        Some(&OVERHEAD),
        "the channel witness holds the overhead"
    );
    let channel = case.replay(&witness, &recorded.included(), Oracle::Recorded);
    assert_same_run("scalars emptied in the block (channel witness)", &recorded, &channel);

    case.run();
}

/// The L1 block contract's entry of a transaction's state, taken out, so two runs that read other
/// values there can be compared on everything else.
fn without_l1_entry(run: &Run) -> Vec<Result<TxSnapshot, String>> {
    let mut txs = run.txs.clone();
    for tx in txs.iter_mut().flatten() {
        if !tx.is_deposit {
            tx.state.remove(&L1_BLOCK_CONTRACT);
        }
    }
    txs
}

/// The overhead is a slot the block must be able to read, and nothing more: the fee of a
/// transaction priced with empty scalars does not take it in, so the same block on a chain that
/// holds another overhead produces the same transactions, receipts and state changes; only the
/// value its report carries differs.
#[test]
fn test_the_overhead_a_block_reads_decides_no_result() {
    let held = scalars_emptied_in_the_block(OVERHEAD).run().recorded;
    let zero = scalars_emptied_in_the_block(U256::ZERO).run().recorded;
    let overhead =
        |run: &Run| l1_account(&run.tx(1).state).storage[&L1_OVERHEAD_SLOT].present_value;
    assert_eq!(overhead(&held), OVERHEAD);
    assert_eq!(overhead(&zero), U256::ZERO);
    assert_eq!(without_l1_entry(&held), without_l1_entry(&zero), "the transactions");
    assert_eq!(held.receipts, zero.receipts, "the receipts");
    assert_eq!(held.bundle, zero.bundle, "the state changes");
    assert_eq!((held.gas_used, held.blob_gas_used), (zero.gas_used, zero.blob_gas_used));
}

/// A transaction whose own execution writes the L1 block contract keeps its write: code at the
/// contract that empties the scalars, called by the first transaction the fee computation prices.
/// Its state names the account as the call left it, touched, and the scalars' slot as it wrote it,
/// from the set scalars it was priced with to zero; the report adds the other slots it read as
/// read-only entries and replaces neither. The block replays.
#[test]
fn test_a_transaction_that_writes_the_l1_block_contract_keeps_its_write() {
    let db = chain_with_l1_info(scalars_emptier());
    let replay = Case::new("a reader that writes the L1 block contract", db)
        .tx(call(0, L1_BLOCK_CONTRACT, Bytes::new(), creation_gas()))
        .run();
    let run = &replay.recorded;
    let tx = run.tx(0);
    assert!(tx.result.is_success(), "{:?}", tx.result);
    let account = l1_account(&tx.state);
    assert!(account.is_touched(), "the call's own entry, not a read-only one");
    let scalars = &account.storage[&ECOTONE_L1_FEE_SCALARS_SLOT];
    assert_eq!(scalars.original_value, fee_scalars_word(1_000_000, 1_000_000), "priced on it");
    assert_eq!(scalars.present_value, U256::ZERO, "the transaction's write stands");
    for (slot, value) in l1_info() {
        if slot == ECOTONE_L1_FEE_SCALARS_SLOT {
            continue;
        }
        let entry = account.storage.get(&slot).unwrap_or_else(|| panic!("slot {slot} reported"));
        assert_eq!(entry.present_value, value, "slot {slot}");
        assert!(!entry.is_changed(), "slot {slot} is read only");
    }
    assert_eq!(account.storage.len(), l1_info().len(), "no overhead: the scalars were set");
}

/// Code at the L1 block contract that sets the Ecotone scalars, which
/// [`chain_with_free_l1_info`] holds empty: a write to a slot the fee computation reads.
fn scalars_setter() -> Bytes {
    BytecodeBuilder::default().sstore(ECOTONE_L1_FEE_SCALARS_SLOT, scalars_set()).stop().build()
}

/// The scalars [`scalars_setter`] writes.
fn scalars_set() -> U256 {
    fee_scalars_word(7, 9)
}

/// A transaction whose own execution writes a slot of the L1 block contract the fee computation
/// reads keeps its write and its account, touched, in whatever order the builder executes and
/// commits it beside another transaction, and on whichever executor it executed: committed before
/// the other, the report adds the slots it does not hold as read-only entries under its own
/// account; committed after it, the other reported every slot, and its state holds its own
/// entries alone. Every ordering's block replays.
#[test]
fn test_a_transaction_that_writes_the_l1_block_contract_keeps_its_write_in_every_ordering() {
    let writer = || free_call(CALLER, 0, L1_BLOCK_CONTRACT, Bytes::new());
    let reader = || free_call(OTHER, 0, CONTRACT, slot(2));
    let case = |name: &str, first: Tx, second: Tx| {
        Case::new(name, chain_with_free_l1_info(scalars_setter())).tx(first).tx(second)
    };
    let orderings = [
        ("writer first", case("writer first", writer(), reader()), 0),
        ("reader first", case("reader first", reader(), writer()), 1),
        ("writer held", case("writer held", writer(), reader()).held(0), 0),
        ("reader held", case("reader held", reader(), writer()).held(0), 1),
        (
            "writer elsewhere, after the reader there",
            case("writer elsewhere", writer(), reader()).executed_elsewhere(0, vec![reader()]),
            0,
        ),
        (
            "reader elsewhere, after the writer there",
            case("reader elsewhere", reader(), writer()).executed_elsewhere(0, vec![writer()]),
            1,
        ),
    ];
    for (name, case, at) in orderings {
        let run = case.run().recorded;
        let reader_at = 1 - at;
        assert!(run.tx(0).result.is_success() && run.tx(1).result.is_success(), "{name}");

        let own = l1_account(&run.tx(at).state);
        assert!(own.is_touched(), "{name}: the writer's own entry");
        let written = &own.storage[&ECOTONE_L1_FEE_SCALARS_SLOT];
        assert_eq!(written.original_value, U256::ZERO, "{name}: written over the empty scalars");
        assert_eq!(written.present_value, scalars_set(), "{name}: the write stands");

        let reported: Vec<_> = free_l1_info()
            .into_iter()
            .filter(|(slot, _)| *slot != ECOTONE_L1_FEE_SCALARS_SLOT)
            .collect();
        if at < reader_at {
            for (slot, value) in &reported {
                let entry = own.storage.get(slot).unwrap_or_else(|| panic!("{name}: slot {slot}"));
                assert_eq!(entry.present_value, *value, "{name}: slot {slot}");
                assert!(!entry.is_changed(), "{name}: slot {slot} is read only");
            }
            assert_eq!(own.storage.len(), free_l1_info().len(), "{name}: the slots named");
            assert!(!run.tx(reader_at).state.contains_key(&L1_BLOCK_CONTRACT), "{name}");
        } else {
            assert_eq!(own.storage.len(), 1, "{name}: the writer's own slot alone");
            let first = l1_account(&run.tx(reader_at).state);
            assert_read_only(name, first, &free_l1_info());
        }
        for (slot, _) in reported {
            assert_eq!(times_named(&run, Some(slot)), 1, "{name}: slot {slot} is named once");
        }
    }
}

/// The entry's other condition: a chain that does not hold the L1 block contract. A deposit that
/// creates the account in the block does not turn its slots into database reads: the slots of an
/// account the block created are the block's own, so the transaction priced after the deposit
/// reads none from the chain, and the witness, which holds the account as absent and no slot of
/// it, replays the block.
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
    assert!(!pre_block_names_l1(&recorded), "no pre-block read of the L1 block contract");
    let created = l1_account(&recorded.tx(0).state);
    assert_eq!(created.info.balance, U256::from(1_000), "the deposit created the account");
    let reported = l1_account(&recorded.tx(1).state);
    assert!(!reported.is_touched(), "the transaction after it reports the account read-only");
    assert_eq!(recorded.record.accounts.get(&L1_BLOCK_CONTRACT), Some(&None), "absent before");
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

/// A block on `db` that starts, then executes `tx`, optionally using fee information already
/// cached for this block so the transaction itself needs no fee lookup.
fn execute_one<DB>(db: DB, tx: &Tx, cached_fees: bool) -> Result<(), BlockExecutionError>
where
    DB: alloy_evm::Database + revm::DatabaseCommit,
{
    let mut evm = MegaEvmFactory::new().create_evm(db, common::evm_env());
    if cached_fees {
        evm.ctx_mut().modify_chain(|chain| {
            *chain = zero_fee_l1_block_info();
            chain.l2_block = Some(U256::from(common::BLOCK_NUMBER));
        });
    }
    let mut executor = MegaBlockExecutor::new(
        evm,
        common::unlimited_ctx(),
        common::chain_spec(),
        OpAlloyReceiptBuilder::default(),
    );
    executor.apply_pre_execution_changes().expect("nothing reads the L1 block contract yet");
    executor.execute_transaction(tx).map(drop)
}

/// A database over the L1 info that cannot serve the L1 block contract's `slot`, or its account
/// when `slot` is `None`.
fn refusing(slot: Option<U256>) -> ErrorInjectingDatabase {
    let mut db = ErrorInjectingDatabase::new(chain_with_l1_info(Bytes::from(vec![0x00])));
    match slot {
        Some(slot) => db.fail_on_storage = Some((L1_BLOCK_CONTRACT, slot)),
        None => db.fail_on_account = Some(L1_BLOCK_CONTRACT),
    }
    db
}

/// The L1 base fee is read by the fee computation alone, inside the transaction's execution: a
/// database that cannot serve it gives the transaction no outcome, and the failure is reported as
/// any failed read of a transaction's execution is, by whether the database declares it fatal —
/// an internal error when it does, an invalid block when it does not. The block starts all the
/// same: nothing reads the slot before the transaction.
#[test]
fn test_a_failed_fee_read_is_routed_as_the_transactions_own() {
    let tx = call(0, CONTRACT, slot(1), write_gas());

    let mut state = State::builder()
        .with_database(refusing(Some(L1_BASE_FEE_SLOT)))
        .with_bundle_update()
        .build();
    let err = execute_one(&mut state, &tx, false).expect_err("the L1 base fee cannot be read");
    assert!(matches!(err, BlockExecutionError::Internal(_)), "fatal: {err:?}");

    let err = execute_one(common::NotFatal(refusing(Some(L1_BASE_FEE_SLOT))), &tx, false)
        .expect_err("the L1 base fee cannot be read");
    assert!(
        matches!(err, BlockExecutionError::Validation(BlockValidationError::EVM { .. })),
        "not fatal: {err:?}"
    );
}

/// Cached fee information lets execution finish, but the outcome still needs the fee read for
/// its witness. A failure to collect that read has the transaction's fatal/nonfatal routing.
#[test]
fn test_a_failed_fee_witness_read_with_cached_fees_has_no_outcome() {
    let tx = call(0, CONTRACT, slot(1), write_gas());
    let mut state = State::builder()
        .with_database(refusing(Some(L1_BASE_FEE_SLOT)))
        .with_bundle_update()
        .build();

    let fatal = execute_one(&mut state, &tx, true).expect_err("the witness lacks the base fee");
    assert!(matches!(fatal, BlockExecutionError::Internal(_)), "fatal: {fatal:?}");
    assert!(fatal.to_string().contains(&L1_BLOCK_CONTRACT.to_string()), "{fatal}");

    let not_fatal = execute_one(common::NotFatal(refusing(Some(L1_BASE_FEE_SLOT))), &tx, true)
        .expect_err("the witness lacks the base fee");
    assert!(
        matches!(not_fatal, BlockExecutionError::Validation(BlockValidationError::EVM { .. })),
        "not fatal: {not_fatal:?}"
    );
    assert!(not_fatal.to_string().contains(&L1_BLOCK_CONTRACT.to_string()), "{not_fatal}");
}

/// The footprint scalar is read by the block before a non-deposit transaction runs, outside its
/// execution: a database that cannot serve the L1 block contract's account or the scalar's slot
/// fails the block as an internal error, whether or not it declares the error fatal, before an
/// ordinary transaction and before a Mega System Transaction alike. The block starts all the
/// same: nothing reads them before the transaction.
#[test]
fn test_a_failed_footprint_read_is_an_internal_error() {
    let txs = [
        ("a user transaction", call(0, CONTRACT, slot(1), write_gas())),
        ("a Mega System Transaction", common::system_tx()),
    ];
    for (what, tx) in &txs {
        for slot in [None, Some(OPERATOR_FEE_SCALARS_SLOT)] {
            let err = execute_one(common::NotFatal(refusing(slot)), tx, false)
                .expect_err("the footprint scalar cannot be read");
            assert!(matches!(err, BlockExecutionError::Internal(_)), "{what}, {slot:?}: {err:?}");
            assert!(
                err.to_string().contains(&L1_BLOCK_CONTRACT.to_string()),
                "{what}, {slot:?}: {err}"
            );
        }
    }
}
