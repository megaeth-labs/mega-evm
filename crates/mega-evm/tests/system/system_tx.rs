//! The system-address transaction: what it may do, what it pays, and what still validates it.

use alloy_evm::Evm;
use alloy_op_evm::{OpTx, OpTxError};
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use alloy_sol_types::SolCall;
use mega_evm::{
    system::{
        is_mega_system_transaction_with, IOracle, ISequencerRegistry, MEGA_SYSTEM_ADDRESS,
        MEGA_SYSTEM_TRANSACTION_SOURCE_HASH, ORACLE_CONTRACT_ADDRESS, SEQUENCER_REGISTRY_ADDRESS,
        SYSTEM_ADDRESS_WITHOUT_SYSTEM_SHAPE,
    },
    test_utils::{op_transaction, ErrorInjectingDatabase, GasInspector, MemoryDatabase},
    EvmTxRuntimeLimits, LimitUsage, MegaContext, MegaEvm, MegaHaltReason, MegaTransaction,
    MegaTransactionOutcome,
};
use mega_system_contracts::sequencer_registry::storage_slots::CURRENT_SYSTEM_ADDRESS;
use op_revm::{transaction::deposit::DEPOSIT_TRANSACTION_TYPE, OpTransactionError};
use revm::{
    bytecode::opcode::{SLOAD, STOP},
    context::{
        result::{EVMError, ExecutionResult, InvalidTransaction},
        transaction::{AccessList, AccessListItem},
        CfgEnv, TxEnv,
    },
    database::{states::bundle_state::BundleRetention, State},
    DatabaseCommit,
};

use crate::common::{
    account_state_gas, block, context, state_is_free, system_db, CALLER, GAS_LIMIT,
};

/// The chain the tests run on.
const CHAIN_ID: u64 = 4326;

/// A contract that is not on the whitelist.
const OFF_WHITELIST: Address = address!("0x00000000000000000000000000000000000dead1");

/// A balance that pays any fee the tests' transactions carry.
const FUNDS: u64 = 1_000_000_000_000_000;

/// The Oracle's slot the system transactions write.
const ORACLE_SLOT: U256 = U256::ZERO;

/// A database with the system contracts in place and the registry naming
/// [`MEGA_SYSTEM_ADDRESS`] the current system address: that is what makes a legacy call from it
/// to the Oracle a system-address transaction, and what lets its Oracle write through.
fn chain_db() -> MemoryDatabase {
    system_db().sequencer_registry(MEGA_SYSTEM_ADDRESS)
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

/// `tx` with its type set to `tx_type`: an EIP-2930 (1) or EIP-1559 (2) transaction with the same
/// fields and an empty access list. An EIP-1559 one tips its whole fee cap, so it pays the price
/// the legacy one does on a block with no base fee.
fn typed(mut tx: MegaTransaction, tx_type: u8) -> MegaTransaction {
    tx.0.base.tx_type = tx_type;
    if tx_type == 2 {
        tx.0.base.gas_priority_fee = Some(tx.0.base.gas_price);
    }
    tx
}

/// A call to the Oracle's `getSlot`, which reads nothing of the registry itself.
fn get_slot() -> Bytes {
    IOracle::getSlotCall { slot: ORACLE_SLOT }.abi_encode().into()
}

/// The transactions from `caller` that do not have the system shape: a legacy call off the
/// whitelist, a legacy creation, and an EIP-2930 and an EIP-1559 call to the Oracle.
fn other_shapes(caller: Address) -> [(&'static str, MegaTransaction); 4] {
    let oracle = TxKind::Call(ORACLE_CONTRACT_ADDRESS);
    [
        ("off the whitelist", legacy_tx(caller, TxKind::Call(OFF_WHITELIST), Bytes::new(), 0)),
        ("a creation", legacy_tx(caller, TxKind::Create, Bytes::from_static(&[STOP]), 0)),
        ("EIP-2930", typed(legacy_tx(caller, oracle, get_slot(), 0), 1)),
        ("EIP-1559", typed(legacy_tx(caller, oracle, get_slot(), 0), 2)),
    ]
}

/// A transaction from the live system address that does not have the system shape — off the
/// whitelist, a creation, another type than legacy — is invalid: refused with the same error
/// whether or not the system address could pay its fee, and changing nothing.
#[test]
fn test_another_shape_from_the_live_system_address_is_invalid() {
    let db = || chain_db().account_code(OFF_WHITELIST, Bytes::from_static(&[STOP]));
    for (shape, tx) in other_shapes(MEGA_SYSTEM_ADDRESS) {
        for db in [db(), db().account_balance(MEGA_SYSTEM_ADDRESS, U256::from(FUNDS))] {
            let error = MegaEvm::new(chain_context(db))
                .execute_transaction(tx.clone())
                .expect_err("the system address sends the system shape alone");
            let EVMError::Transaction(OpTxError(OpTransactionError::Base(
                InvalidTransaction::Str(message),
            ))) = &error
            else {
                panic!("{shape}: not the shape's refusal: {error:?}");
            };
            assert_eq!(message, SYSTEM_ADDRESS_WITHOUT_SYSTEM_SHAPE, "{shape}");
        }
    }
}

/// Every transaction reads the registry when it is validated, whatever its shape and whoever
/// sends it: its returned state holds the registry's account, untouched, and the system-address
/// slot, unchanged, and a database that cannot serve the registry's account or that slot fails it
/// with the database's error, a deposit included.
#[test]
fn test_every_transaction_reads_the_registry() {
    let funded = || {
        chain_db()
            .account_code(OFF_WHITELIST, Bytes::from_static(&[STOP]))
            .account_balance(CALLER, U256::from(FUNDS))
    };
    let shaped = legacy_tx(CALLER, TxKind::Call(ORACLE_CONTRACT_ADDRESS), get_slot(), 0);
    let deposit = deposit_tx(CALLER, TxKind::Call(OFF_WHITELIST), U256::ZERO, 0);
    let transactions = other_shapes(CALLER)
        .into_iter()
        .chain([("the system shape", shaped), ("a deposit", deposit)]);
    for (shape, tx) in transactions {
        let outcome = MegaEvm::new(chain_context(funded()))
            .execute_transaction(tx.clone())
            .expect("an ordinary transaction");
        assert!(outcome.result.is_success(), "{shape}: {:?}", outcome.result);
        let registry = &outcome.state[&SEQUENCER_REGISTRY_ADDRESS];
        assert!(!registry.is_touched(), "{shape}: a read-only entry");
        let slot = &registry.storage[&CURRENT_SYSTEM_ADDRESS];
        assert!(!slot.is_changed(), "{shape}");
        assert_eq!(slot.present_value(), U256::from_be_slice(MEGA_SYSTEM_ADDRESS.as_slice()));

        let mut account = ErrorInjectingDatabase::new(funded());
        account.fail_on_account = Some(SEQUENCER_REGISTRY_ADDRESS);
        let mut slot = ErrorInjectingDatabase::new(funded());
        slot.fail_on_storage = Some((SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS));
        for db in [account, slot] {
            let error = MegaEvm::new(chain_context(db))
                .execute_transaction(tx.clone())
                .expect_err("the registry is read");
            assert!(matches!(error, EVMError::Database(_)), "{shape}: {error:?}");
        }
    }
}

/// Every transaction starts with the registry's account and its system-address slot warm, as
/// EIP-3651 starts the block beneficiary warm: validation reads the live system address for every
/// transaction, whatever its shape. A user's call to the Oracle's `setSlots` makes the Oracle ask
/// the registry for the system address itself, and that `SLOAD` pays the warm price, as a legacy
/// call, as an EIP-2930 call, and as one whose access list names the slot alike: the shape of the
/// envelope changes nothing the transaction pays, and the access list adds only its own
/// intrinsic cost.
#[test]
fn test_every_transaction_warms_the_registry_and_its_slot() {
    let run = |tx: MegaTransaction| {
        let mut evm =
            MegaEvm::new(chain_context(chain_db().account_balance(CALLER, U256::from(FUNDS))))
                .with_inspector(GasInspector::new());
        let outcome = evm.execute_transaction(tx).expect("an ordinary transaction");
        assert!(!outcome.result.is_success(), "the Oracle refuses a write from a user");
        let sloads: Vec<u64> = evm
            .inspector()
            .records()
            .iter()
            // The transaction's frame is at journal depth 1, the registry's call below it at 2.
            .filter(|record| record.opcode.get() == SLOAD && record.depth == 2)
            .map(|record| record.gas_cost())
            .collect();
        assert_eq!(sloads.len(), 1, "the registry reads its one slot: {sloads:?}");
        (outcome, sloads[0])
    };
    let write = || {
        legacy_tx(
            CALLER,
            TxKind::Call(ORACLE_CONTRACT_ADDRESS),
            set_slot(ORACLE_SLOT, B256::with_last_byte(1)),
            0,
        )
    };

    let warm = mega_evm::satin_gas_params().warm_storage_read_cost();

    let (shaped, legacy) = run(write());
    let (unshaped, typed_call) = run(typed(write(), 1));
    for (shape, outcome) in [("legacy", &shaped), ("EIP-2930", &unshaped)] {
        let registry = &outcome.state[&SEQUENCER_REGISTRY_ADDRESS];
        assert!(!registry.is_touched(), "{shape}: the read writes nothing");
        assert!(registry.storage.contains_key(&CURRENT_SYSTEM_ADDRESS), "{shape}: the slot");
    }
    assert_eq!(legacy, warm, "the legacy call's SLOAD is warm");
    assert_eq!(typed_call, warm, "and so is the EIP-2930 call's");
    assert_eq!(shaped.gas, unshaped.gas, "the two spend the same gas on every ledger");

    let mut listed = typed(write(), 1);
    listed.0.base.access_list = AccessList(vec![AccessListItem {
        address: SEQUENCER_REGISTRY_ADDRESS,
        storage_keys: vec![B256::from(CURRENT_SYSTEM_ADDRESS)],
    }]);
    let (_, listed) = run(listed);
    assert_eq!(listed, warm, "an access list naming the slot makes it no warmer");
}

/// The registry's account and slot every transaction reads are an untouched entry of its state,
/// and a commit skips an account nothing touched: committing a transaction writes nothing of the
/// registry, whether the state holds a registry or not, so the state root moves as it would
/// without the read.
#[test]
fn test_committing_a_transaction_writes_nothing_of_the_registry() {
    for (holds, db) in [("a registry", chain_db()), ("no registry", MemoryDatabase::default())] {
        let db = db
            .account_balance(CALLER, U256::from(FUNDS))
            .account_code(OFF_WHITELIST, Bytes::from_static(&[STOP]));
        let mut state = State::builder().with_database(db).with_bundle_update().build();
        let mut evm = MegaEvm::new(chain_context(&mut state));
        let tx = legacy_tx(CALLER, TxKind::Call(OFF_WHITELIST), Bytes::new(), 0);
        let outcome = evm.execute_transaction(tx).expect("an ordinary transaction");
        assert!(outcome.result.is_success(), "{holds}: {:?}", outcome.result);
        assert!(outcome.state.contains_key(&SEQUENCER_REGISTRY_ADDRESS), "{holds}: it was read");
        drop(evm);
        state.commit(outcome.state.clone());

        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        assert!(bundle.state.contains_key(&CALLER), "{holds}: the commit wrote the sender");
        assert!(
            !bundle.state.contains_key(&SEQUENCER_REGISTRY_ADDRESS),
            "{holds}: and nothing of the registry"
        );
    }
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

    assert_eq!(revm::context::Transaction::tx_type(&tx), DEPOSIT_TRANSACTION_TYPE);
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

/// Runs `tx` on `db` under the chain's configuration with `mutate` applied.
fn run_with(
    mutate: fn(&mut CfgEnv<mega_evm::MegaSpecId>),
    db: MemoryDatabase,
    tx: MegaTransaction,
) -> Result<MegaTransactionOutcome, EVMError<core::convert::Infallible, OpTxError>> {
    let mut cfg = CfgEnv::new_with_spec(mega_evm::MegaSpecId::SATIN);
    cfg.chain_id = CHAIN_ID;
    mutate(&mut cfg);
    MegaEvm::new(context(db).with_cfg(cfg)).execute_transaction(tx)
}

/// The chain id is checked whatever the caller's configuration says: the spec fixes the check on.
/// A transaction carrying another chain's id is refused, a system transaction as a user's, and a
/// typed transaction must carry one; a user's legacy transaction may carry none (pre-EIP-155), a
/// system transaction may not.
#[test]
fn test_the_chain_id_is_checked_whatever_the_configuration_says() {
    let off = |cfg: &mut CfgEnv<mega_evm::MegaSpecId>| cfg.tx_chain_id_check = false;
    let funded = || chain_db().account_balance(CALLER, U256::from(FUNDS));
    let oracle = TxKind::Call(ORACLE_CONTRACT_ADDRESS);
    let write = || set_slot(ORACLE_SLOT, B256::with_last_byte(0xee));
    let refusal = |db, tx| format!("{:?}", run_with(off, db, tx).expect_err("refused"));

    let foreign_system = legacy_tx_with_chain_id(MEGA_SYSTEM_ADDRESS, oracle, write(), 0, Some(1));
    assert!(refusal(chain_db(), foreign_system).contains("InvalidChainId"));
    let bare_system = legacy_tx_with_chain_id(MEGA_SYSTEM_ADDRESS, oracle, write(), 0, None);
    assert!(refusal(chain_db(), bare_system).contains("MissingChainId"));

    let foreign_user = legacy_tx_with_chain_id(CALLER, oracle, get_slot(), 0, Some(1));
    assert!(refusal(funded(), foreign_user.clone()).contains("InvalidChainId"));
    assert!(refusal(funded(), typed(foreign_user, 2)).contains("InvalidChainId"));
    let bare_user = legacy_tx_with_chain_id(CALLER, oracle, get_slot(), 0, None);
    assert!(refusal(funded(), typed(bare_user.clone(), 2)).contains("MissingChainId"));
    let outcome = run_with(off, funded(), bare_user).expect("a pre-EIP-155 transaction is valid");
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
}

/// The nonce and code checks a caller switches off for user transactions are switched off here
/// too: one shape of validation, not two.
#[test]
fn test_the_configuration_switches_hold_for_system_transactions() {
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

/// Validating a system transaction reads the system address's account, the transaction's caller,
/// which every transaction starts warm: the system transaction pays for its own account what any
/// sender pays for its own.
#[test]
fn test_validating_a_system_transaction_charges_its_caller_as_any_senders() {
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
    assert_eq!(one_account, account_state_gas(), "a fresh recipient costs state gas");

    // A system transaction whose sender does not exist yet creates it, for the same charge.
    let created = run_outcome(chain_db(), system_tx(0, B256::with_last_byte(0x77)));
    assert!(created.result.is_success(), "{:?}", created.result);
    // The same transaction from a sender that already exists: what it pays is the slot it
    // writes, and nothing for its own account.
    let slot_charge = run_outcome(
        chain_db().account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1)),
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

/// A deposit from `sender`, an account that does not exist, to the Oracle, whose gas limit is one
/// short of its intrinsic gas and the account it creates for its caller, at the byte prices in
/// effect. It carries no value and no calldata, so its intrinsic gas, which is its floor too, is
/// EIP-2780's base of 12,000 and 3,000 for the recipient.
fn deposit_one_short_of_its_caller(sender: Address) -> MegaTransaction {
    const INTRINSIC: u64 = 12_000 + 3_000;
    let mut tx = deposit_tx(sender, TxKind::Call(ORACLE_CONTRACT_ADDRESS), U256::ZERO, 0);
    tx.0.base.gas_limit = INTRINSIC + account_state_gas() - 1;
    tx
}

/// A deposit that cannot pay for the account it creates for its caller runs out of gas before its
/// first frame, and a deposit that does is a failed deposit: the `FailedDeposit` halt, not an
/// out-of-gas, billed its whole gas limit.
///
/// Rules: [S5], [S21.5], [S21.7], [S5.12]. Independence: constants — the intrinsic gas is
/// EIP-2780's, the caller account's state gas the schedule's entry at the price in effect.
#[test]
fn test_a_deposit_that_cannot_pay_for_its_caller_is_a_failed_deposit() {
    // An account that costs nothing is one any deposit can pay for.
    if state_is_free() {
        return;
    }
    let sender = address!("0x00000000000000000000000000000000000f0006");
    let tx = deposit_one_short_of_its_caller(sender);
    let gas_limit = tx.0.base.gas_limit;
    let outcome = run_outcome(chain_db(), tx);
    assert!(
        matches!(
            outcome.result,
            ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
        ),
        "{:?} is not a failed deposit",
        outcome.result,
    );
    assert_eq!(outcome.gas.gas_used, gas_limit, "a failed deposit bills its whole gas limit");
}

/// The account a deposit creates for its caller is charged before its first frame, and a body over
/// the data-size limit does not waive the charge: a deposit that cannot pay for it is a failed
/// deposit, which reports the failure and not the data-size stop, and keeps its body counted.
///
/// Rules: [S11], [S10.15], [S21.7]. Independence: constants — the body is `TX_BODY_SIZE`, the gas
/// limit as [`deposit_one_short_of_its_caller`] works it out.
#[test]
fn test_a_deposit_over_the_data_size_limit_that_cannot_pay_for_its_caller_is_a_failed_deposit() {
    // An account that costs nothing is one any deposit can pay for.
    if state_is_free() {
        return;
    }
    let sender = address!("0x00000000000000000000000000000000000f0008");
    let limits =
        EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(mega_evm::TX_BODY_SIZE - 1);

    let tx = deposit_one_short_of_its_caller(sender);
    let gas_limit = tx.0.base.gas_limit;
    let outcome = MegaEvm::new(chain_context(chain_db()).with_tx_runtime_limits(limits))
        .execute_transaction(tx)
        .expect("the deposit is included");
    assert!(
        matches!(
            outcome.result,
            ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
        ),
        "{:?} is not a failed deposit",
        outcome.result,
    );
    assert_eq!(outcome.gas.gas_used, gas_limit, "a failed deposit bills its whole gas limit");
    assert_eq!(outcome.limit_exceeded, None, "the failure is not a data-size stop");
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
        "the body stays counted",
    );
}

/// The registry the Oracle reads the current system address from holds it as a chain's registry
/// does, so a write from the system address is the one the Oracle accepts.
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

/// The contract a system transaction calls sees the system address as its caller: the promotion
/// does not put another address in its place.
#[test]
fn test_the_callee_sees_the_system_address_as_its_caller() {
    use mega_evm::test_utils::BytecodeBuilder;
    use revm::bytecode::opcode::{CALLER as CALLER_OP, MSTORE, RETURN};

    let returns_caller = BytecodeBuilder::default()
        .append(CALLER_OP)
        .push_number(0_u64)
        .append(MSTORE)
        .push_number(32_u64)
        .push_number(0_u64)
        .append(RETURN)
        .build();
    let db = chain_db().account_code(ORACLE_CONTRACT_ADDRESS, returns_caller);
    let outcome = run_outcome(
        db,
        legacy_tx(MEGA_SYSTEM_ADDRESS, TxKind::Call(ORACLE_CONTRACT_ADDRESS), Bytes::new(), 0),
    );

    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(
        Address::from_word(B256::from_slice(outcome.result.output().unwrap())),
        MEGA_SYSTEM_ADDRESS,
    );
}

/// The account a deposit-like transaction creates for its caller is the transaction's own
/// account: it is part of the transaction body, so it is not a write record of its own.
#[test]
fn test_the_created_caller_is_not_a_write_record() {
    let fresh = run_outcome(chain_db(), system_tx(0, B256::with_last_byte(0x99)));
    let existing = run_outcome(
        chain_db().account_balance(MEGA_SYSTEM_ADDRESS, U256::from(1)),
        system_tx(0, B256::with_last_byte(0x99)),
    );
    assert!(fresh.result.is_success() && existing.result.is_success());
    assert_eq!(
        fresh.usage, existing.usage,
        "creating the sender's own account records nothing a transaction with an existing sender \
         does not record",
    );
}

/// A deposit-like transaction that creates a contract pays for the account it creates for its
/// caller as well as for the contract.
#[test]
fn test_a_deposit_that_creates_a_contract_pays_for_its_caller_too() {
    let creator = address!("0x00000000000000000000000000000000000f0007");
    let init_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]); // returns empty code
    let deposit = |db: MemoryDatabase| {
        let mut tx = legacy_tx(creator, TxKind::Create, init_code.clone(), 0);
        tx.0.base.gas_price = 0;
        tx.0.deposit.source_hash = B256::repeat_byte(0x55);
        run_outcome(db, tx)
    };
    let fresh = deposit(chain_db());
    let existing = deposit(chain_db().account_balance(creator, U256::from(1)));

    assert!(fresh.result.is_success(), "{:?}", fresh.result);
    assert!(existing.result.is_success(), "{:?}", existing.result);
    assert_eq!(
        fresh.gas.state,
        existing.gas.state + account_state_gas(),
        "the created caller costs state gas on top of the created contract",
    );
}

/// A transaction that already carries the system source hash is a deposit by type, so it does
/// not have the system shape: from the live system address it is a failed deposit, as every
/// deposit from that address is. No promotion is made for
/// it, so none of the promotion's checks runs: its nonce is not the system address's.
///
/// A block's transactions are decoded from their legacy encoding, which carries no source hash,
/// so this is the shape a caller hands the engine directly, not one a block holds.
#[test]
fn test_a_transaction_that_arrives_promoted_is_a_failed_deposit() {
    let mut tx =
        legacy_tx(MEGA_SYSTEM_ADDRESS, TxKind::Call(ORACLE_CONTRACT_ADDRESS), get_slot(), 7);
    tx.0.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
    let outcome = MegaEvm::new(chain_context(chain_db()))
        .execute_transaction(tx)
        .expect("a deposit is not refused");
    assert!(
        matches!(
            outcome.result,
            ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }
        ),
        "{:?}",
        outcome.result
    );
}
