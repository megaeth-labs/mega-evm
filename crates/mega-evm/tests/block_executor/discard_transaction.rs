//! Tests for `MegaBlockExecutor::discard_transaction`, the cleanup an embedder runs after a
//! transaction's execution unwound with a panic.
//!
//! A panic skips the handler's own cleanup (`catch_error` and the journal `finalize`), so the
//! panicked transaction's journal, shared memory, cached L1 cost and `MegaETH` per-transaction
//! state are still in the EVM when the next transaction starts. These tests drive that path with
//! a database that panics on one account read, and check that after `discard_transaction` the
//! next transaction executes exactly as it does on a fresh executor. They also pin the other half
//! of the contract: a transaction that returns an error leaves nothing behind on its own, on the
//! plain and on the inspected execution path alike.
//!
//! The tests live in a module of their own because the panic seam (a database that panics) and
//! the `catch_unwind` harness have no place in the other executor tests.

use std::{
    convert::Infallible,
    panic::{catch_unwind, AssertUnwindSafe},
};

use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
use alloy_evm::{EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, InjectedDbError, MemoryDatabase},
    BlockLimits, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvm, MegaEvmFactory,
    MegaHardforkConfig, MegaSpecId, MegaTransactionOutcome, MegaTxEnvelope, TestExternalEnvs,
    ORACLE_CONTRACT_ADDRESS,
};
use op_revm::constants::BASE_FEE_RECIPIENT;
use revm::{
    bytecode::opcode::{BALANCE, CALL, GAS, MSIZE, MSTORE, POP, RETURN, TIMESTAMP},
    context::{BlockEnv, ContextError, ContextTr},
    database::State,
    inspector::NoOpInspector,
    primitives::{StorageKey, StorageValue},
    state::{AccountInfo, Bytecode, EvmState},
    Database, DatabaseCommit,
};

const CHAIN_ID: u64 = 8453;
const BLOCK_NUMBER: u64 = 1000;
const BLOCK_GAS_LIMIT: u64 = 30_000_000;
const TX_GAS_LIMIT: u64 = 1_000_000;
const GAS_PRICE: u128 = 1_000_000;
/// Compute gas a transaction may still spend after accessing the block environment. Finite so
/// that detention is observable through `detained_compute_gas_limit`.
const DETENTION_CAP: u64 = 20_000_000;
/// Compute gas a transaction may still spend after accessing the oracle; lower than
/// `DETENTION_CAP` so the two detentions are distinguishable.
const ORACLE_CAP: u64 = 1_000_000;

/// Sender of the transaction that panics.
const PANICKING_SENDER: Address = address!("2000000000000000000000000000000000000001");
/// Sender of the transaction executed after the panic.
const NEXT_SENDER: Address = address!("2000000000000000000000000000000000000002");
/// The account whose database read panics. It is never created, so every touch reaches the
/// database.
const PANICKING_ACCOUNT: Address = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
/// An EIP-7702 delegator to `PANICKING_ACCOUNT`. A CALL to it loads the delegator (fine) and
/// then, inside the CALL body's target resolution, the delegate: that read panics.
const DELEGATOR_TO_PANICKING: Address = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1");
/// The block beneficiary.
const BENEFICIARY: Address = address!("3000000000000000000000000000000000000099");
/// Contract that expands memory, writes a storage slot, reads the beneficiary's balance and then
/// reads the panicking account's balance: every kind of per-transaction residue, then the panic.
const RESIDUE_THEN_PANIC: Address = address!("1000000000000000000000000000000000000001");
/// Contract that `CALL`s `DELEGATOR_TO_PANICKING`, so the panic lands inside a CALL-family body.
const CALL_PANICKING: Address = address!("1000000000000000000000000000000000000002");
/// Contract that reads the block timestamp, which detains compute gas, and stops.
const READ_TIMESTAMP: Address = address!("1000000000000000000000000000000000000006");
/// Contract that `CALL`s the oracle, which detains compute gas further before REX3, and then
/// panics.
const CALL_ORACLE_THEN_PANIC: Address = address!("1000000000000000000000000000000000000008");
/// Contract that writes a storage slot and stops.
const WRITE_SLOT: Address = address!("1000000000000000000000000000000000000007");
/// Contract that returns the memory size it starts with.
const RETURN_MSIZE: Address = address!("1000000000000000000000000000000000000003");
/// Contract that reads the block beneficiary's balance.
const READ_BENEFICIARY: Address = address!("1000000000000000000000000000000000000004");
/// Contract that `CALL`s `RETURN_MSIZE`.
const CALL_RETURN_MSIZE: Address = address!("1000000000000000000000000000000000000005");

/// A database whose account read for one address panics, standing in for a node-side read that
/// panics inside the EVM. The wrapped database can also fail a read with an error, for the
/// transactions that must fail without panicking.
#[derive(Debug)]
struct PanickingDatabase {
    inner: ErrorInjectingDatabase,
    panic_on_account: Address,
}

impl Database for PanickingDatabase {
    type Error = InjectedDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, InjectedDbError> {
        assert!(address != self.panic_on_account, "injected panic reading account {address}");
        self.inner.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, InjectedDbError> {
        self.inner.code_by_hash(code_hash)
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, InjectedDbError> {
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, InjectedDbError> {
        self.inner.block_hash(number)
    }
}

impl DatabaseCommit for PanickingDatabase {
    fn commit(&mut self, changes: EvmState) {
        self.inner.commit(changes);
    }
}

type Executor<'a> = MegaBlockExecutor<
    MegaHardforkConfig,
    MegaEvm<&'a mut State<PanickingDatabase>, NoOpInspector, TestExternalEnvs<Infallible>>,
    OpAlloyReceiptBuilder,
>;

/// `CALL(GAS, target, 0, 0, 0, 0, 0); POP`.
fn call(builder: BytecodeBuilder, target: Address) -> BytecodeBuilder {
    builder
        .push_number(0u8) // return size
        .push_number(0u8) // return offset
        .push_number(0u8) // args size
        .push_number(0u8) // args offset
        .push_number(0u8) // value
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(POP)
}

/// `CALL(GAS, target, 0, 0, 0, 0, 0); POP; STOP`.
fn call_code(target: Address) -> Bytes {
    call(BytecodeBuilder::default(), target).stop().build()
}

/// `BALANCE(address); POP`.
fn read_balance(builder: BytecodeBuilder, address: Address) -> BytecodeBuilder {
    builder.push_address(address).append(BALANCE).append(POP)
}

fn database() -> PanickingDatabase {
    let mut db = MemoryDatabase::default();
    db.set_account_balance(PANICKING_SENDER, U256::from(1_000_000_000_000_000u64));
    db.set_account_balance(NEXT_SENDER, U256::from(1_000_000_000_000_000u64));
    let residue_then_panic =
        BytecodeBuilder::default().mstore(0, [0xff; 32]).sstore(U256::ZERO, U256::from(1));
    let residue_then_panic = read_balance(residue_then_panic, BENEFICIARY);
    db.set_account_code(
        RESIDUE_THEN_PANIC,
        read_balance(residue_then_panic, PANICKING_ACCOUNT).stop().build(),
    );
    let delegation = Bytecode::new_eip7702(PANICKING_ACCOUNT);
    db.insert_account_info(
        DELEGATOR_TO_PANICKING,
        AccountInfo {
            code_hash: delegation.hash_slow(),
            code: Some(delegation),
            ..Default::default()
        },
    );
    db.set_account_code(CALL_PANICKING, call_code(DELEGATOR_TO_PANICKING));
    // MSIZE; MSTORE(0, msize); RETURN(0, 32)
    db.set_account_code(
        RETURN_MSIZE,
        BytecodeBuilder::default()
            .append(MSIZE)
            .push_number(0u8)
            .append(MSTORE)
            .push_number(32u8)
            .push_number(0u8)
            .append(RETURN)
            .build(),
    );
    db.set_account_code(
        READ_BENEFICIARY,
        read_balance(BytecodeBuilder::default(), BENEFICIARY).stop().build(),
    );
    db.set_account_code(CALL_RETURN_MSIZE, call_code(RETURN_MSIZE));
    db.set_account_code(
        READ_TIMESTAMP,
        BytecodeBuilder::default().append(TIMESTAMP).append(POP).stop().build(),
    );
    let call_oracle = call(BytecodeBuilder::default(), ORACLE_CONTRACT_ADDRESS);
    db.set_account_code(
        CALL_ORACLE_THEN_PANIC,
        read_balance(call_oracle, PANICKING_ACCOUNT).stop().build(),
    );
    db.set_account_code(
        WRITE_SLOT,
        BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build(),
    );
    PanickingDatabase {
        inner: ErrorInjectingDatabase::new(db),
        panic_on_account: PANICKING_ACCOUNT,
    }
}

/// Builds an executor on `state`; `inspect` enables the inspected execution path, which runs
/// the same handler through `inspect_tx` instead of `transact`.
fn new_executor_with(
    state: &mut State<PanickingDatabase>,
    spec: MegaSpecId,
    inspect: bool,
) -> Executor<'_> {
    let mut cfg_env = revm::context::CfgEnv::default();
    cfg_env.spec = spec;
    cfg_env.chain_id = CHAIN_ID;
    let block_env = BlockEnv {
        number: U256::from(BLOCK_NUMBER),
        timestamp: U256::from(1_800_000_000),
        gas_limit: BLOCK_GAS_LIMIT,
        beneficiary: BENEFICIARY,
        ..Default::default()
    };
    let evm_factory =
        MegaEvmFactory::new().with_external_env_factory(TestExternalEnvs::<Infallible>::new());
    let evm_env = EvmEnv::new(cfg_env, block_env);
    let evm = if inspect {
        evm_factory.create_evm_with_inspector(state, evm_env, NoOpInspector)
    } else {
        evm_factory.create_evm(state, evm_env)
    };
    let block_ctx = MegaBlockExecutionCtx::new(
        B256::ZERO,
        None,
        Bytes::new(),
        BlockLimits::no_limits()
            .with_block_env_access_compute_gas_limit(DETENTION_CAP)
            .with_oracle_access_compute_gas_limit(ORACLE_CAP),
    );
    let chain_spec = MegaHardforkConfig::default().with_all_activated_through(spec);
    let mut executor =
        MegaBlockExecutor::new(evm, block_ctx, chain_spec, OpAlloyReceiptBuilder::default());
    // Non-zero L1 fee parameters, pinned to this block so the handler does not reload them from
    // the (empty) L1 block contract: the L1 cost then depends on the transaction's calldata, which
    // makes a cached cost carried over from another transaction visible in the sender's balance.
    executor.evm.ctx.modify_chain(|chain| {
        chain.l2_block = Some(U256::from(BLOCK_NUMBER));
        chain.l1_base_fee = U256::from(1_000_000_000u64);
        chain.l1_base_fee_scalar = U256::from(1_000_000u64);
        chain.l1_blob_base_fee = Some(U256::from(1));
        chain.l1_blob_base_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_scalar = Some(U256::ZERO);
        chain.operator_fee_constant = Some(U256::ZERO);
    });
    executor
}

fn new_executor(state: &mut State<PanickingDatabase>, spec: MegaSpecId) -> Executor<'_> {
    new_executor_with(state, spec, false)
}

fn legacy_tx(sender: Address, nonce: u64, to: Address, input: Bytes) -> Recovered<MegaTxEnvelope> {
    let tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce,
        gas_price: GAS_PRICE,
        gas_limit: TX_GAS_LIMIT,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input,
    };
    let signed = Signed::new_unchecked(tx, Signature::test_signature(), Default::default());
    Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), sender)
}

/// The panicking transaction carries calldata that does not compress down to the minimum
/// charged size, so its L1 cost is well above the cost of the empty-calldata transaction that
/// follows it.
fn panicking_tx(to: Address) -> Recovered<MegaTxEnvelope> {
    let input = (0..1024u32).map(|i| (i.wrapping_mul(7919) % 251) as u8).collect::<Vec<_>>();
    legacy_tx(PANICKING_SENDER, 0, to, input.into())
}

fn next_tx(to: Address) -> Recovered<MegaTxEnvelope> {
    legacy_tx(NEXT_SENDER, 0, to, Bytes::new())
}

/// Executes `tx` on `executor`, asserting that the injected database panic unwinds out of the
/// executor.
fn execute_panicking(executor: &mut Executor<'_>, tx: &Recovered<MegaTxEnvelope>) {
    let unwound = catch_unwind(AssertUnwindSafe(|| executor.execute_mega_transaction(tx)));
    assert!(unwound.is_err(), "the injected database panic must unwind out of execution");
}

/// Executes `tx` as the first transaction of a fresh executor: what the same transaction must
/// produce after a discard.
fn reference_outcome(spec: MegaSpecId, tx: &Recovered<MegaTxEnvelope>) -> MegaTransactionOutcome {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor(&mut state, spec);
    executor.execute_mega_transaction(tx).expect("reference tx executes").inner
}

fn assert_same_outcome(actual: &MegaTransactionOutcome, reference: &MegaTransactionOutcome) {
    assert_eq!(actual.result_and_state, reference.result_and_state);
    assert_eq!(actual.data_size, reference.data_size);
    assert_eq!(actual.kv_updates, reference.kv_updates);
    assert_eq!(actual.compute_gas_used, reference.compute_gas_used);
    assert_eq!(actual.state_growth_used, reference.state_growth_used);
}

/// Everything the panicked transaction left behind is gone: its storage write and nonce bump stay
/// out of the committed state, its loaded accounts, expanded memory and cached L1 cost stay out of
/// the next transaction's outcome, and the trackers no longer report its usage.
#[test]
fn test_discard_transaction_drops_the_panicked_transactions_state() {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor(&mut state, MegaSpecId::REX6);
    let initial_cap = executor.evm.ctx.additional_limit.borrow().detained_compute_gas_limit();

    execute_panicking(&mut executor, &panicking_tx(RESIDUE_THEN_PANIC));
    // The residue is really there before the discard.
    assert!(executor.evm.ctx.volatile_data_tracker.borrow().has_accessed_beneficiary_balance());
    assert_ne!(executor.evm.ctx.additional_limit.borrow().get_usage().kv_updates, 0);
    assert_ne!(
        executor.evm.ctx.additional_limit.borrow().detained_compute_gas_limit(),
        initial_cap
    );

    executor.discard_transaction();
    assert!(!executor.evm.ctx.volatile_data_tracker.borrow().has_accessed_beneficiary_balance());
    let usage = executor.evm.ctx.additional_limit.borrow().get_usage();
    assert_eq!(
        (usage.data_size, usage.kv_updates, usage.compute_gas, usage.state_growth),
        (0, 0, 0, 0)
    );
    assert_eq!(
        executor.evm.ctx.additional_limit.borrow().detained_compute_gas_limit(),
        initial_cap
    );

    let next_tx = next_tx(RETURN_MSIZE);
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes");
    assert!(outcome.result_and_state.result.is_success(), "{:?}", outcome.result_and_state.result);
    assert_eq!(
        outcome.result_and_state.result.output().map(|output| U256::from_be_slice(output)),
        Some(U256::ZERO),
        "the next tx started with the panicked tx's memory"
    );
    let touched = &outcome.result_and_state.state;
    assert!(
        !touched.contains_key(&RESIDUE_THEN_PANIC),
        "the contract loaded by the panicked tx leaked into the next tx's state"
    );
    assert!(
        !touched.contains_key(&PANICKING_SENDER),
        "the sender of the panicked tx leaked into the next tx's state"
    );
    assert_same_outcome(&outcome, &reference_outcome(MegaSpecId::REX6, &next_tx));

    executor.commit_execution_outcome(outcome).expect("next tx commits");
    assert_eq!(executor.receipts.len(), 1, "only the next tx produces a receipt");
    drop(executor);

    assert_eq!(
        state.storage(RESIDUE_THEN_PANIC, U256::ZERO).unwrap(),
        U256::ZERO,
        "the panicked tx's storage write reached the state"
    );
    assert_eq!(
        state.basic(PANICKING_SENDER).unwrap().unwrap().nonce,
        0,
        "the panicked tx's nonce bump reached the state"
    );
    assert_eq!(state.basic(NEXT_SENDER).unwrap().unwrap().nonce, 1);
}

/// A panic inside a CALL-family body leaves the host's target-resolution scope open. The discard
/// closes it, so the next transaction's own CALL opens a fresh scope and executes as on a fresh
/// executor. From REX6 the open scope changes nothing the outcome shows, so this pins the scope
/// through the handlers' debug assertion that scopes do not nest; the REX5 test below pins the
/// observable effect.
#[test]
fn test_discard_transaction_closes_the_call_target_resolution_scope() {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor(&mut state, MegaSpecId::REX6);

    execute_panicking(&mut executor, &panicking_tx(CALL_PANICKING));
    executor.discard_transaction();

    let next_tx = next_tx(CALL_RETURN_MSIZE);
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes");
    assert!(outcome.result_and_state.result.is_success(), "{:?}", outcome.result_and_state.result);
    assert_same_outcome(&outcome, &reference_outcome(MegaSpecId::REX6, &next_tx));
}

/// Before REX6 a CALL's delegate hop does not mark the beneficiary. With the panicked CALL's scope
/// left open, the next transaction's plain balance read of the beneficiary would be classified as
/// that hop and go unmarked; after the discard it marks, as it does on every spec outside a CALL.
#[test]
fn test_discard_transaction_restores_beneficiary_marking_before_rex6() {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor(&mut state, MegaSpecId::REX5);

    execute_panicking(&mut executor, &panicking_tx(CALL_PANICKING));
    executor.discard_transaction();

    let next_tx = next_tx(READ_BENEFICIARY);
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes");
    assert!(outcome.result_and_state.result.is_success(), "{:?}", outcome.result_and_state.result);
    assert!(
        executor.evm.ctx.volatile_data_tracker.borrow().has_accessed_beneficiary_balance(),
        "the beneficiary balance read of the next tx was not marked"
    );
}

/// A database error that a load stashed in the context is normally taken by the handler when it
/// builds the result; an unwind in between leaves it there, and the next transaction would fail
/// with it. The discard clears the slot. The stash is injected directly, since the panicking
/// database cannot both fail a read and panic before the handler takes the error.
#[test]
fn test_discard_transaction_clears_a_stashed_context_error() {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor(&mut state, MegaSpecId::REX6);

    execute_panicking(&mut executor, &panicking_tx(RESIDUE_THEN_PANIC));
    *executor.evm.ctx.error() = Err(ContextError::Custom("stashed by the panicked tx".into()));
    executor.discard_transaction();

    let next_tx = next_tx(RETURN_MSIZE);
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes");
    assert_same_outcome(&outcome, &reference_outcome(MegaSpecId::REX6, &next_tx));
}

/// Before REX1 the compute gas limit detained by a volatile access persists across transactions,
/// so the start-of-transaction reset alone would hand the next transaction the cap the panicked
/// one lowered, while resetting the cap outright would drop the detention a completed earlier
/// transaction left in force. The discard restores exactly the cap the panicked transaction
/// started under.
#[test]
fn test_discard_transaction_restores_the_detained_limit_before_rex1() {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor(&mut state, MegaSpecId::REX);
    let detained_cap = |executor: &Executor<'_>| {
        executor.evm.ctx.additional_limit.borrow().detained_compute_gas_limit()
    };
    assert!(detained_cap(&executor) > DETENTION_CAP);

    // A completed transaction's detention is what the panicked one starts under.
    let detaining_tx = legacy_tx(NEXT_SENDER, 0, READ_TIMESTAMP, Bytes::new());
    let outcome = executor.execute_mega_transaction(&detaining_tx).expect("detaining tx executes");
    executor.commit_execution_outcome(outcome).expect("detaining tx commits");
    assert_eq!(detained_cap(&executor), DETENTION_CAP);

    execute_panicking(&mut executor, &panicking_tx(CALL_ORACLE_THEN_PANIC));
    assert_eq!(
        detained_cap(&executor),
        ORACLE_CAP,
        "the panicked tx's oracle call did not detain compute gas further"
    );

    executor.discard_transaction();
    assert_eq!(detained_cap(&executor), DETENTION_CAP, "the cap must be the inherited one");

    let next_tx = legacy_tx(NEXT_SENDER, 1, RETURN_MSIZE, Bytes::new());
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes");
    assert!(outcome.result_and_state.result.is_success(), "{:?}", outcome.result_and_state.result);
    assert_eq!(
        detained_cap(&executor),
        DETENTION_CAP,
        "the next tx must run under the inherited cap"
    );
}

/// A transaction that fails with an error after its frame committed into the journal (a database
/// error while the post-execution fee accounting reads a fee recipient) leaves nothing behind on
/// its own: the plain path finalizes the journal in `transact`, and the inspected path in this
/// crate's `inspect_tx`. Discarding afterwards, once or twice, changes nothing.
fn assert_error_path_leaves_nothing_behind(inspect: bool) {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor_with(&mut state, MegaSpecId::REX6, inspect);

    executor.evm.db_mut().database.inner.fail_on_account = Some(BASE_FEE_RECIPIENT);
    let failing_tx = legacy_tx(PANICKING_SENDER, 0, WRITE_SLOT, Bytes::new());
    executor.execute_mega_transaction(&failing_tx).expect_err("the fee recipient read fails");
    executor.evm.db_mut().database.inner.fail_on_account = None;

    let next_tx = next_tx(RETURN_MSIZE);
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes");
    let touched = &outcome.result_and_state.state;
    assert!(
        !touched.contains_key(&WRITE_SLOT),
        "the failed tx's contract leaked (inspect: {inspect})"
    );
    assert!(
        !touched.contains_key(&PANICKING_SENDER),
        "the failed tx's sender leaked (inspect: {inspect})"
    );
    assert_same_outcome(&outcome, &reference_outcome(MegaSpecId::REX6, &next_tx));

    // Nothing was committed, so the same transaction is valid again.
    executor.discard_transaction();
    executor.discard_transaction();
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes again");
    assert_same_outcome(&outcome, &reference_outcome(MegaSpecId::REX6, &next_tx));
}

#[test]
fn test_error_path_leaves_nothing_behind_without_inspector() {
    assert_error_path_leaves_nothing_behind(false);
}

#[test]
fn test_error_path_leaves_nothing_behind_with_inspector() {
    assert_error_path_leaves_nothing_behind(true);
}
