//! The system-address transaction: what it may do, what it pays, and what still validates it.

use alloy_evm::Evm;
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        is_deposit_like_transaction, is_mega_system_transaction_with, IOracle, ISequencerRegistry,
        MEGA_SYSTEM_ADDRESS, MEGA_SYSTEM_TRANSACTION_SOURCE_HASH, ORACLE_CONTRACT_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
    },
    test_utils::{op_transaction, MemoryDatabase},
    MegaContext, MegaEvm, MegaTransaction, MegaTransactionOutcome,
};
use mega_system_contracts::sequencer_registry::storage_slots::CURRENT_SYSTEM_ADDRESS;
use revm::{
    context::{result::ExecutionResult, CfgEnv, TxEnv},
    database::State,
    DatabaseCommit,
};

use crate::common::{block, context, system_db, CALLER, GAS_LIMIT};

/// The chain the tests run on.
const CHAIN_ID: u64 = 4326;

/// A contract that is not on the whitelist.
const OFF_WHITELIST: Address = address!("0x00000000000000000000000000000000000dead1");

/// The Oracle's slot the system transactions write.
const ORACLE_SLOT: U256 = U256::ZERO;

/// A database with the system contracts in place and the registry naming
/// [`MEGA_SYSTEM_ADDRESS`] the current system address, which is what lets an Oracle write from
/// it through.
fn chain_db() -> MemoryDatabase {
    system_db().account_storage(
        SEQUENCER_REGISTRY_ADDRESS,
        CURRENT_SYSTEM_ADDRESS,
        U256::from_be_slice(MEGA_SYSTEM_ADDRESS.as_slice()),
    )
}

/// A legacy transaction from `caller`, as the sequencer builds one.
fn legacy_tx(caller: Address, kind: TxKind, data: Bytes, nonce: u64) -> MegaTransaction {
    legacy_tx_with_chain_id(caller, kind, data, nonce, Some(CHAIN_ID))
}

fn legacy_tx_with_chain_id(
    caller: Address,
    kind: TxKind,
    data: Bytes,
    nonce: u64,
    chain_id: Option<u64>,
) -> MegaTransaction {
    OpTx(op_transaction(TxEnv {
        caller,
        kind,
        data,
        nonce,
        chain_id,
        gas_limit: GAS_LIMIT,
        gas_price: 1_000,
        ..Default::default()
    }))
}

/// A deposit transaction from `caller`, which pays no fee, minting `mint` to its caller.
fn deposit_tx(caller: Address, kind: TxKind, value: U256, mint: u128) -> MegaTransaction {
    let mut tx = legacy_tx(caller, kind, Bytes::new(), 0);
    tx.0.base.gas_price = 0;
    tx.0.base.value = value;
    tx.0.deposit.source_hash = B256::repeat_byte(0x22);
    tx.0.deposit.mint = Some(mint);
    tx
}

/// The calldata of `setSlots([slot], [value])`.
fn set_slot(slot: U256, value: B256) -> Bytes {
    IOracle::setSlotsCall { slots: vec![slot], values: vec![value] }.abi_encode().into()
}

/// A system transaction writing `value` to the Oracle's slot.
fn system_tx(nonce: u64, value: B256) -> MegaTransaction {
    legacy_tx(
        MEGA_SYSTEM_ADDRESS,
        TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        set_slot(ORACLE_SLOT, value),
        nonce,
    )
}

/// A Satin context over `db` with the chain id the tests use.
fn chain_context<DB: revm::Database>(db: DB) -> MegaContext<DB> {
    let mut cfg = CfgEnv::new_with_spec(mega_evm::MegaSpecId::SATIN);
    cfg.chain_id = CHAIN_ID;
    context(db).with_cfg(cfg)
}

/// Runs `tx` and returns everything the engine reports about it.
fn run_outcome(db: MemoryDatabase, tx: MegaTransaction) -> MegaTransactionOutcome {
    MegaEvm::new(chain_context(db)).execute_transaction(tx).expect("the transaction is valid")
}

/// Runs `tx` and returns the error it was rejected with, as a string.
fn rejection(db: MemoryDatabase, tx: MegaTransaction) -> String {
    let error = MegaEvm::new(chain_context(db))
        .execute_transaction(tx)
        .expect_err("the transaction must be rejected");
    format!("{error:?}")
}

/// The Oracle's slot as the database holds it.
fn oracle_slot<DB: revm::Database>(db: &mut DB, slot: U256) -> U256
where
    DB::Error: core::fmt::Debug,
{
    db.storage(ORACLE_CONTRACT_ADDRESS, slot).expect("the read succeeds")
}

/// The nonce of `address` as the database holds it.
fn nonce_of<DB: revm::Database>(db: &mut DB, address: Address) -> u64
where
    DB::Error: core::fmt::Debug,
{
    db.basic(address).expect("the read succeeds").map_or(0, |info| info.nonce)
}

/// A system transaction executes with the system address as its caller and writes what it came
/// to write.
#[test]
fn test_a_system_transaction_writes_through_the_oracle() {
    let value = B256::with_last_byte(0x55);
    let outcome = run_outcome(chain_db(), system_tx(0, value));

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        outcome.state[&ORACLE_CONTRACT_ADDRESS].storage[&ORACLE_SLOT].present_value,
        U256::from_be_bytes(value.0),
    );
}

/// The transaction the engine executed is a deposit: the promotion stamped the source hash on
/// it, which is what op-revm's deposit path keys on.
#[test]
fn test_a_system_transaction_is_promoted_to_a_deposit() {
    let mut evm = MegaEvm::new(chain_context(chain_db()));
    let tx = system_tx(0, B256::with_last_byte(0x11));
    assert!(is_mega_system_transaction_with(&tx, MEGA_SYSTEM_ADDRESS));
    assert!(is_deposit_like_transaction(&tx, MEGA_SYSTEM_ADDRESS));

    assert!(evm.transact_raw(tx).expect("the transaction is valid").result.is_success());
    assert_eq!(
        revm::context::ContextTr::tx(evm.ctx()).deposit.source_hash,
        MEGA_SYSTEM_TRANSACTION_SOURCE_HASH,
    );
}

/// A system transaction pays no fee: neither the sequencer's balance nor the block's
/// beneficiary changes, whatever the L1 and operator fees are.
#[test]
fn test_a_system_transaction_pays_no_fee() {
    let balance = U256::from(1_000_000);
    let db = chain_db().account_balance(MEGA_SYSTEM_ADDRESS, balance);
    let mut context = chain_context(db);
    context.modify_chain(|chain| {
        chain.operator_fee_scalar = Some(U256::from(1_000_000));
        chain.operator_fee_constant = Some(U256::from(10_000));
    });
    let mut evm = MegaEvm::new(context);

    let result = evm
        .transact_raw(system_tx(0, B256::with_last_byte(0x22)))
        .expect("the transaction is valid");
    assert!(result.result.is_success(), "{:?}", result.result);
    assert_eq!(
        result.state[&MEGA_SYSTEM_ADDRESS].info.balance, balance,
        "no fee, no L1 fee and no operator fee comes out of the system address",
    );
    let beneficiary = block().beneficiary;
    assert!(
        result.state.get(&beneficiary).is_none_or(|account| account.info.balance.is_zero()),
        "the beneficiary is paid nothing",
    );
}

/// A system transaction needs no balance at all: nothing is deducted from its caller.
#[test]
fn test_a_system_transaction_needs_no_balance() {
    let outcome = run_outcome(chain_db(), system_tx(0, B256::with_last_byte(0x33)));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
}

/// A system transaction that carries value moves exactly that value, and no fee with it.
#[test]
fn test_a_system_transaction_moves_only_its_value() {
    let balance = U256::from(1_000_000);
    let transferred = U256::from(50_000);
    // The Oracle's methods are not payable, so the value goes to a whitelisted address running
    // code that takes it.
    let db = chain_db()
        .account_balance(MEGA_SYSTEM_ADDRESS, balance)
        .account_code(ORACLE_CONTRACT_ADDRESS, Bytes::from_static(&[revm::bytecode::opcode::STOP]));

    let mut tx =
        legacy_tx(MEGA_SYSTEM_ADDRESS, TxKind::Call(ORACLE_CONTRACT_ADDRESS), Bytes::new(), 0);
    tx.0.base.value = transferred;
    let outcome = run_outcome(db, tx);

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.state[&MEGA_SYSTEM_ADDRESS].info.balance, balance - transferred);
    assert_eq!(outcome.state[&ORACLE_CONTRACT_ADDRESS].info.balance, transferred);
}

/// A transaction from the system address to a contract that is not on the whitelist is
/// rejected, and so is one that creates a contract.
#[test]
fn test_the_whitelist_is_what_the_system_address_may_call() {
    let off_whitelist =
        legacy_tx(MEGA_SYSTEM_ADDRESS, TxKind::Call(OFF_WHITELIST), Bytes::new(), 0);
    let refused = rejection(chain_db(), off_whitelist);
    assert!(refused.contains("whitelist"), "{refused}");

    let creation = legacy_tx(
        MEGA_SYSTEM_ADDRESS,
        TxKind::Create,
        Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]),
        0,
    );
    let refused = rejection(chain_db(), creation);
    assert!(refused.contains("whitelist"), "{refused}");
}

/// The whitelist holds for the system address alone: anyone else may call anything, and pays
/// for it.
#[test]
fn test_a_user_transaction_is_not_held_to_the_whitelist() {
    let db = chain_db()
        .account_balance(CALLER, U256::from(1_000_000_000_000_u64))
        .account_code(OFF_WHITELIST, Bytes::from_static(&[revm::bytecode::opcode::STOP]));
    let tx = legacy_tx(CALLER, TxKind::Call(OFF_WHITELIST), Bytes::new(), 0);
    let outcome = run_outcome(db, tx);

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert!(
        outcome.state[&CALLER].info.balance < U256::from(1_000_000_000_000_u64),
        "a user transaction pays its fee",
    );
}

/// A user's deposit transaction is not a system transaction: it is executed as the deposit it
/// is, and the system-address checks do not touch it.
#[test]
fn test_a_user_deposit_is_not_a_system_transaction() {
    let db = chain_db().account_balance(CALLER, U256::from(1_000_000));
    let mut tx = legacy_tx(CALLER, TxKind::Call(OFF_WHITELIST), Bytes::new(), 0);
    tx.0.deposit.source_hash = B256::repeat_byte(0x11);

    assert!(is_deposit_like_transaction(&tx, MEGA_SYSTEM_ADDRESS));
    assert!(!is_mega_system_transaction_with(&tx, MEGA_SYSTEM_ADDRESS));
    let outcome = run_outcome(db, tx);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
}

/// A deposit transaction whose caller is not the system address goes through untouched, even
/// when it carries the system transaction's own source hash.
#[test]
fn test_a_deposit_from_another_caller_is_untouched() {
    let db = chain_db().account_balance(CALLER, U256::from(1_000_000));
    let mut tx = legacy_tx(CALLER, TxKind::Call(OFF_WHITELIST), Bytes::new(), 0);
    tx.0.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
    assert!(run_outcome(db, tx).result.is_success());
}

/// The nonce of a system transaction is checked against the system address's own, so a
/// transaction cannot be replayed: a stale one is refused and changes nothing.
#[test]
fn test_a_replayed_system_transaction_is_refused() {
    let mut state = State::builder().with_database(chain_db()).build();
    let mut evm = MegaEvm::new(chain_context(&mut state));

    let first = B256::with_last_byte(0x11);
    let second = B256::with_last_byte(0x22);
    for (nonce, value) in [(0, first), (1, second)] {
        let result = evm.transact_commit(system_tx(nonce, value)).expect("the transaction runs");
        assert!(result.is_success(), "nonce {nonce}: {result:?}");
    }

    let replay = rejection_in(&mut evm, system_tx(0, first));
    assert!(replay.contains("NonceTooLow"), "{replay}");
    let ahead = rejection_in(&mut evm, system_tx(5, first));
    assert!(ahead.contains("NonceTooHigh"), "{ahead}");

    drop(evm);
    assert_eq!(
        oracle_slot(&mut state, ORACLE_SLOT),
        U256::from_be_bytes(second.0),
        "the refused replay did not write the old value back",
    );
    assert_eq!(
        nonce_of(&mut state, MEGA_SYSTEM_ADDRESS),
        2,
        "a refused transaction does not bump the nonce",
    );
}

/// Two system transactions in the same block both go through: the second sees the nonce the
/// first bumped.
#[test]
fn test_two_system_transactions_in_a_block_both_go_through() {
    let mut state = State::builder().with_database(chain_db()).build();
    let mut evm = MegaEvm::new(chain_context(&mut state));
    for (nonce, value) in [(0, B256::with_last_byte(0xaa)), (1, B256::with_last_byte(0xbb))] {
        assert!(evm.transact_commit(system_tx(nonce, value)).expect("it runs").is_success());
    }
    drop(evm);
    assert_eq!(
        oracle_slot(&mut state, ORACLE_SLOT),
        U256::from_be_bytes(B256::with_last_byte(0xbb).0),
    );
    assert_eq!(nonce_of(&mut state, MEGA_SYSTEM_ADDRESS), 2);
}

/// A system transaction carries the chain's id, so it cannot be replayed on another chain; one
/// without an id at all is refused too.
#[test]
fn test_a_system_transaction_is_bound_to_the_chain() {
    let foreign = legacy_tx_with_chain_id(
        MEGA_SYSTEM_ADDRESS,
        TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        set_slot(ORACLE_SLOT, B256::with_last_byte(0xcc)),
        0,
        Some(31_337),
    );
    let rejected = rejection(chain_db(), foreign);
    assert!(rejected.contains("InvalidChainId"), "{rejected}");

    let no_chain_id = legacy_tx_with_chain_id(
        MEGA_SYSTEM_ADDRESS,
        TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        set_slot(ORACLE_SLOT, B256::with_last_byte(0xdd)),
        0,
        None,
    );
    let rejected = rejection(chain_db(), no_chain_id);
    assert!(rejected.contains("MissingChainId"), "{rejected}");
}

/// The system address may not carry code (EIP-3607), as no sender may.
#[test]
fn test_a_system_address_with_code_is_refused() {
    let db = chain_db().account_code(MEGA_SYSTEM_ADDRESS, Bytes::from_static(&[0x00]));
    let rejected = rejection(db, system_tx(0, B256::with_last_byte(0xce)));
    assert!(rejected.contains("RejectCallerWithCode"), "{rejected}");
}

/// The checks a caller switches off for user transactions are switched off here too: one shape
/// of validation, not two.
#[test]
fn test_the_configuration_switches_hold_for_system_transactions() {
    let run_with = |mutate: fn(&mut CfgEnv<mega_evm::MegaSpecId>), db: MemoryDatabase, tx| {
        let mut cfg = CfgEnv::new_with_spec(mega_evm::MegaSpecId::SATIN);
        cfg.chain_id = CHAIN_ID;
        mutate(&mut cfg);
        MegaEvm::new(context(db).with_cfg(cfg)).execute_transaction(tx)
    };

    // A foreign chain id goes through when the chain-id check is off.
    let foreign = legacy_tx_with_chain_id(
        MEGA_SYSTEM_ADDRESS,
        TxKind::Call(ORACLE_CONTRACT_ADDRESS),
        set_slot(ORACLE_SLOT, B256::with_last_byte(0xee)),
        0,
        Some(31_337),
    );
    let outcome = run_with(|cfg| cfg.tx_chain_id_check = false, chain_db(), foreign)
        .expect("the chain-id check is off");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);

    // A stale nonce goes through when the nonce check is off.
    let db = chain_db().account_nonce(MEGA_SYSTEM_ADDRESS, 7);
    let outcome = run_with(|cfg| cfg.disable_nonce_check = true, db, system_tx(0, B256::ZERO))
        .expect("the nonce check is off");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);

    // A system address with code goes through when EIP-3607 is off.
    let db = chain_db().account_code(MEGA_SYSTEM_ADDRESS, Bytes::from_static(&[0x00]));
    let outcome =
        run_with(|cfg| cfg.disable_eip3607 = true, db, system_tx(0, B256::with_last_byte(0xcf)))
            .expect("EIP-3607 is off");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
}

/// Validating a system transaction does not warm the system address: the transaction pays the
/// cold access its first touch of any account would.
#[test]
fn test_validating_a_system_transaction_does_not_warm_the_system_address() {
    // `getSlot` reads one slot and writes nothing, so what the two transactions differ by is
    // the access to the caller's own account.
    let read = Bytes::from(IOracle::getSlotCall { slot: ORACLE_SLOT }.abi_encode());
    let system = run_outcome(
        chain_db(),
        legacy_tx(MEGA_SYSTEM_ADDRESS, TxKind::Call(ORACLE_CONTRACT_ADDRESS), read.clone(), 0),
    );
    let user = run_outcome(
        chain_db().account_balance(CALLER, U256::from(1_000_000_000_000_u64)),
        legacy_tx(CALLER, TxKind::Call(ORACLE_CONTRACT_ADDRESS), read, 0),
    );
    assert!(system.result.is_success() && user.result.is_success());
    assert_eq!(
        system.gas.regular, user.gas.regular,
        "the system transaction pays what any other transaction pays",
    );
}

/// A system transaction spends nothing on the history ledger: the protocol's own maintenance is
/// exempt from history gas.
#[test]
fn test_a_system_transaction_spends_no_history_gas() {
    let outcome = run_outcome(chain_db(), system_tx(0, B256::with_last_byte(0x66)));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.history, 0, "the protocol's own transaction pays no history gas");
}

/// A deposit-like transaction whose caller does not exist yet creates that account, and pays
/// the account-creation charge for it exactly once: the same charge a value transfer to a
/// fresh account pays.
#[test]
fn test_a_deposit_creates_its_caller_and_pays_for_it_once() {
    // The reference charge: a user transaction that creates one account by sending value to it.
    let fresh = address!("0x00000000000000000000000000000000000f0001");
    let reference =
        run_outcome(chain_db().account_balance(CALLER, U256::from(1_000_000_000_000_u64)), {
            let mut tx = legacy_tx(CALLER, TxKind::Call(fresh), Bytes::new(), 0);
            tx.0.base.value = U256::from(1);
            tx
        });
    let one_account = reference.gas.state;
    assert!(one_account > 0, "a fresh recipient costs state gas");

    // A system transaction whose sender does not exist yet creates it, for the same charge.
    let created = run_outcome(chain_db(), system_tx(0, B256::with_last_byte(0x77)));
    assert!(created.result.is_success(), "{:?}", created.result);
    let slot_charge = run_outcome(
        chain_db()
            .account_nonce(MEGA_SYSTEM_ADDRESS, 0)
            .account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1)),
        system_tx(0, B256::with_last_byte(0x77)),
    )
    .gas
    .state;
    assert_eq!(
        created.gas.state - slot_charge,
        one_account,
        "the caller's account costs one account creation, and the slot the same either way",
    );
}

/// A deposit-like transaction whose caller already exists pays no account-creation charge for
/// it.
#[test]
fn test_a_deposit_with_an_existing_caller_pays_nothing_extra() {
    let existing = chain_db().account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1));
    let fresh = chain_db();
    let (existing, fresh) = (
        run_outcome(existing, system_tx(0, B256::with_last_byte(0x88))).gas.state,
        run_outcome(fresh, system_tx(0, B256::with_last_byte(0x88))).gas.state,
    );
    assert!(fresh > existing, "{fresh} is not above {existing}");
}

/// A deposit whose recipient is its own caller pays for that one account once: by the time the
/// recipient is looked at, the caller's materialisation has created it.
#[test]
fn test_a_deposit_to_its_own_caller_pays_once() {
    let fresh = address!("0x00000000000000000000000000000000000f0002");
    let outcome =
        run_outcome(chain_db(), deposit_tx(fresh, TxKind::Call(fresh), U256::from(100), 1_000));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);

    let reference =
        run_outcome(chain_db().account_balance(CALLER, U256::from(1_000_000_000_000_u64)), {
            let recipient = address!("0x00000000000000000000000000000000000f0003");
            let mut tx = legacy_tx(CALLER, TxKind::Call(recipient), Bytes::new(), 0);
            tx.0.base.value = U256::from(1);
            tx
        });
    assert_eq!(
        outcome.gas.state, reference.gas.state,
        "one account was created, so one account-creation charge was paid",
    );
}

/// A deposit that creates its caller and sends value to another account that does not exist
/// pays for both.
#[test]
fn test_a_deposit_that_creates_two_accounts_pays_for_both() {
    let sender = address!("0x00000000000000000000000000000000000f0004");
    let recipient = address!("0x00000000000000000000000000000000000f0005");
    let deposit = || deposit_tx(sender, TxKind::Call(recipient), U256::from(100), 1_000);
    let two = run_outcome(chain_db(), deposit()).gas.state;
    let one = run_outcome(chain_db().account_balance(sender, U256::from(1)), deposit()).gas.state;

    assert_eq!(two, one * 2, "two accounts created, two charges");
}

/// A deposit that cannot pay for the account it creates for its caller is an out-of-gas halt,
/// as a transaction that runs out in its runtime phase is.
#[test]
fn test_a_deposit_that_cannot_pay_for_its_caller_halts() {
    let sender = address!("0x00000000000000000000000000000000000f0006");
    let mut tx = deposit_tx(sender, TxKind::Call(ORACLE_CONTRACT_ADDRESS), U256::ZERO, 0);
    tx.0.base.gas_limit = 30_000;

    let outcome = run_outcome(chain_db(), tx);
    assert!(
        matches!(outcome.result, ExecutionResult::Halt { .. }),
        "{:?} is not a halt",
        outcome.result,
    );
}

/// The registry the Oracle reads the current system address from is seeded as the chain seeds
/// it, so a write from the system address is the one the Oracle accepts.
#[test]
fn test_the_oracle_accepts_the_system_address_the_registry_names() {
    let current = ISequencerRegistry::currentSystemAddressCall {}.abi_encode();
    let outcome = run_outcome(
        chain_db().account_balance(CALLER, U256::from(1_000_000_000_000_u64)),
        legacy_tx(CALLER, TxKind::Call(SEQUENCER_REGISTRY_ADDRESS), current.into(), 0),
    );
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        Address::from_word(B256::from_slice(outcome.result.output().unwrap())),
        MEGA_SYSTEM_ADDRESS,
    );
}

/// Runs `tx` on `evm` and returns the error it was rejected with.
fn rejection_in<DB>(
    evm: &mut MegaEvm<DB, revm::inspector::NoOpInspector>,
    tx: MegaTransaction,
) -> String
where
    DB: alloy_evm::Database + DatabaseCommit,
{
    let error = evm.execute_transaction(tx).expect_err("the transaction must be rejected");
    format!("{error:?}")
}
