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
    backtrace::Backtrace,
    convert::Infallible,
    panic::{catch_unwind, AssertUnwindSafe},
};

use alloy_consensus::{transaction::Recovered, Sealed, Signed, TxLegacy};
use alloy_evm::{EvmEnv, EvmFactory};
use alloy_op_evm::block::receipt_builder::OpAlloyReceiptBuilder;
use alloy_primitives::{address, Address, Bytes, Signature, TxKind, B256, U256};
use mega_evm::{
    test_utils::{BytecodeBuilder, ErrorInjectingDatabase, InjectedDbError, MemoryDatabase},
    BlockLimits, LimitUsage, MegaBlockExecutionCtx, MegaBlockExecutor, MegaEvm, MegaEvmFactory,
    MegaHardforkConfig, MegaSpecId, MegaTransactionOutcome, MegaTxEnvelope, TestExternalEnvs,
    MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS,
};
use op_alloy_consensus::TxDeposit;
use op_revm::constants::BASE_FEE_RECIPIENT;
use revm::{
    bytecode::opcode::{BALANCE, CALL, GAS, MSIZE, MSTORE, POP, RETURN, TIMESTAMP},
    context::{result::ExecutionResult, BlockEnv, ContextError, ContextTr},
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
/// The sender of the probe transaction of the panic-site sweep; nothing else uses its nonce.
const PROBE_SENDER: Address = address!("2000000000000000000000000000000000000003");
/// A depositor without balance, whose deposit with a value fails validation.
const POOR_DEPOSITOR: Address = address!("2000000000000000000000000000000000000004");
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
/// `TIMESTAMP; POP; BALANCE(FAILING_ACCOUNT)`: detains compute gas, then fails with an error.
const READ_TIMESTAMP_THEN_FAIL: Address = address!("1000000000000000000000000000000000000009");
/// An account whose read fails with an error once the test arms it, for a transaction that
/// must fail without panicking.
const FAILING_ACCOUNT: Address = address!("cccccccccccccccccccccccccccccccccccccccc");
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
    /// The call stack of the last injected panic, for the tests that check where it landed.
    last_panic_backtrace: Option<String>,
}

impl Database for PanickingDatabase {
    type Error = InjectedDbError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, InjectedDbError> {
        if address == self.panic_on_account {
            self.last_panic_backtrace = Some(Backtrace::force_capture().to_string());
            panic!("injected panic reading account {address}");
        }
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
    db.set_account_balance(PROBE_SENDER, U256::from(1_000_000_000_000_000u64));
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
    db.set_account_code(
        READ_TIMESTAMP_THEN_FAIL,
        read_balance(BytecodeBuilder::default().append(TIMESTAMP).append(POP), FAILING_ACCOUNT)
            .stop()
            .build(),
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
        last_panic_backtrace: None,
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
    legacy_tx(PANICKING_SENDER, 0, to, panicking_input())
}

fn next_tx(to: Address) -> Recovered<MegaTxEnvelope> {
    legacy_tx(NEXT_SENDER, 0, to, Bytes::new())
}

/// A deposit from `from`. The block executor reads the depositor's account before the EVM runs
/// the transaction, so a deposit from the panicking account unwinds before `on_new_tx`.
fn deposit_tx(from: Address, to: Address, value: U256, input: Bytes) -> Recovered<MegaTxEnvelope> {
    let tx = TxDeposit {
        source_hash: B256::ZERO,
        from,
        to: TxKind::Call(to),
        mint: 0,
        value,
        gas_limit: TX_GAS_LIMIT,
        is_system_transaction: false,
        input,
    };
    let sealed = Sealed::new_unchecked(tx, B256::ZERO);
    Recovered::new_unchecked(MegaTxEnvelope::Deposit(sealed), from)
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

fn detained_cap(executor: &Executor<'_>) -> u64 {
    executor.evm.ctx.additional_limit.borrow().detained_compute_gas_limit()
}

fn usage(executor: &Executor<'_>) -> LimitUsage {
    executor.evm.ctx.additional_limit.borrow().get_usage()
}

/// The calldata of every panicking transaction: large enough that the data-size usage its
/// `on_new_tx` charges differs from whatever the preceding transaction left.
fn panicking_input() -> Bytes {
    (0..1024u32).map(|i| (i.wrapping_mul(7919) % 251) as u8).collect::<Vec<_>>().into()
}

fn assert_same_outcome(actual: &MegaTransactionOutcome, reference: &MegaTransactionOutcome) {
    assert_same_outcome_in(actual, reference, "");
}

fn assert_same_outcome_in(
    actual: &MegaTransactionOutcome,
    reference: &MegaTransactionOutcome,
    row: &str,
) {
    assert_eq!(actual.result_and_state, reference.result_and_state, "{row}: result and state");
    assert_eq!(actual.data_size, reference.data_size, "{row}: data size");
    assert_eq!(actual.kv_updates, reference.kv_updates, "{row}: kv updates");
    assert_eq!(actual.compute_gas_used, reference.compute_gas_used, "{row}: compute gas");
    assert_eq!(actual.state_growth_used, reference.state_growth_used, "{row}: state growth");
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

/// The block executor reads a deposit's sender before the EVM runs the transaction, so a panic in
/// that read lands before the transaction's `on_new_tx`. The discard must still restore the cap
/// the last completed transaction left, not the one that transaction started under.
#[test]
fn test_discard_transaction_restores_the_detained_limit_when_the_panic_precedes_on_new_tx() {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor(&mut state, MegaSpecId::REX);
    assert!(detained_cap(&executor) > DETENTION_CAP);

    let detaining_tx = legacy_tx(NEXT_SENDER, 0, READ_TIMESTAMP, Bytes::new());
    let outcome = executor.execute_mega_transaction(&detaining_tx).expect("detaining tx executes");
    executor.commit_execution_outcome(outcome).expect("detaining tx commits");
    assert_eq!(detained_cap(&executor), DETENTION_CAP);

    execute_panicking(
        &mut executor,
        &deposit_tx(PANICKING_ACCOUNT, RETURN_MSIZE, U256::ZERO, Bytes::new()),
    );
    assert_eq!(detained_cap(&executor), DETENTION_CAP, "the panic preceded any change");

    executor.discard_transaction();
    assert_eq!(
        detained_cap(&executor),
        DETENTION_CAP,
        "the cap must be the inherited one, not the one the detaining tx started under"
    );

    let next_tx = legacy_tx(NEXT_SENDER, 1, RETURN_MSIZE, Bytes::new());
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes");
    assert!(outcome.result_and_state.result.is_success(), "{:?}", outcome.result_and_state.result);
    assert_eq!(
        detained_cap(&executor),
        DETENTION_CAP,
        "the next tx must run under the inherited cap"
    );
}

/// Before REX1 the cap detained by a transaction that then fails with an error carries over to
/// the next transaction all the same. A discard after a panic that precedes `on_new_tx` must
/// restore that cap, not the one the failed transaction started under.
#[test]
fn test_discard_transaction_restores_the_detained_limit_a_failed_transaction_left_before_rex1() {
    let mut state = State::builder().with_database(database()).build();
    let mut executor = new_executor(&mut state, MegaSpecId::REX);
    assert!(detained_cap(&executor) > DETENTION_CAP);

    executor.evm.db_mut().database.inner.fail_on_account = Some(FAILING_ACCOUNT);
    let failing_tx = legacy_tx(NEXT_SENDER, 0, READ_TIMESTAMP_THEN_FAIL, Bytes::new());
    executor.execute_mega_transaction(&failing_tx).expect_err("the account read fails");
    executor.evm.db_mut().database.inner.fail_on_account = None;
    assert_eq!(
        detained_cap(&executor),
        DETENTION_CAP,
        "premise: before REX1 a failed tx's detention carries over"
    );

    execute_panicking(
        &mut executor,
        &deposit_tx(PANICKING_ACCOUNT, RETURN_MSIZE, U256::ZERO, Bytes::new()),
    );
    executor.discard_transaction();
    assert_eq!(
        detained_cap(&executor),
        DETENTION_CAP,
        "the cap must be the one the failed tx left, not the one it started under"
    );

    // The failed tx was not committed, so its nonce is still free.
    let next_tx = legacy_tx(NEXT_SENDER, 0, RETURN_MSIZE, Bytes::new());
    let outcome = executor.execute_mega_transaction(&next_tx).expect("next tx executes");
    assert!(outcome.result_and_state.result.is_success(), "{:?}", outcome.result_and_state.result);
    assert_eq!(
        detained_cap(&executor),
        DETENTION_CAP,
        "the next tx must run under the inherited cap"
    );
}

/// Where along `execute_mega_transaction` the injected panic lands, earliest first. Each site is
/// a database read the fixture makes panic.
#[derive(Clone, Copy, Debug)]
enum PanicSite {
    /// The block executor's depositor pre-read, before the EVM runs the transaction.
    DepositorPreRead,
    /// The system transaction's account inspection in the handler's `before_run`, which precedes
    /// `on_new_tx` from REX5 on; before REX5 the system address is first read by the caller load
    /// in pre-execution.
    SystemTxPreRun,
    /// The caller load in pre-execution, after `on_new_tx` and before any frame.
    CallerLoad,
    /// The load of the call target when the first frame is built.
    TargetLoad,
    /// A BALANCE read inside the contract, with a frame open.
    MidExecution,
    /// The beneficiary load in the post-execution reward, after the frames returned.
    BeneficiaryReward,
}

impl PanicSite {
    const ALL: [Self; 6] = [
        Self::DepositorPreRead,
        Self::SystemTxPreRun,
        Self::CallerLoad,
        Self::TargetLoad,
        Self::MidExecution,
        Self::BeneficiaryReward,
    ];

    /// The account whose read panics, and the transaction whose execution reads it there.
    fn inject(self) -> (Address, Recovered<MegaTxEnvelope>) {
        let input = panicking_input();
        match self {
            Self::DepositorPreRead => {
                (PANICKING_ACCOUNT, deposit_tx(PANICKING_ACCOUNT, RETURN_MSIZE, U256::ZERO, input))
            }
            Self::SystemTxPreRun => (
                MEGA_SYSTEM_ADDRESS,
                legacy_tx(MEGA_SYSTEM_ADDRESS, 0, ORACLE_CONTRACT_ADDRESS, input),
            ),
            Self::CallerLoad => {
                (PANICKING_ACCOUNT, legacy_tx(PANICKING_ACCOUNT, 0, RETURN_MSIZE, input))
            }
            Self::TargetLoad => {
                (PANICKING_ACCOUNT, legacy_tx(PANICKING_SENDER, 0, PANICKING_ACCOUNT, input))
            }
            Self::MidExecution => {
                (PANICKING_ACCOUNT, legacy_tx(PANICKING_SENDER, 0, RESIDUE_THEN_PANIC, input))
            }
            Self::BeneficiaryReward => {
                (BENEFICIARY, legacy_tx(PANICKING_SENDER, 0, RETURN_MSIZE, input))
            }
        }
    }

    /// Whether the panic lands before the transaction's `on_new_tx`.
    fn precedes_on_new_tx(self, spec: MegaSpecId) -> bool {
        match self {
            Self::DepositorPreRead => true,
            Self::SystemTxPreRun => spec.is_enabled(MegaSpecId::REX5),
            _ => false,
        }
    }

    /// Whether the call stack of the injected panic is the one this site stands for.
    fn landed_as_intended(self, spec: MegaSpecId, backtrace: &str) -> bool {
        match self {
            Self::DepositorPreRead => {
                backtrace.contains("run_transaction_with_sizes") &&
                    !backtrace.contains("MegaHandler")
            }
            Self::SystemTxPreRun if spec.is_enabled(MegaSpecId::REX5) => {
                backtrace.contains("before_run")
            }
            Self::SystemTxPreRun | Self::CallerLoad => {
                backtrace.contains("validate_against_state_and_deduct_caller")
            }
            Self::TargetLoad => {
                backtrace.contains("frame_init") && !backtrace.contains("frame_run")
            }
            Self::MidExecution => backtrace.contains("frame_run"),
            Self::BeneficiaryReward => backtrace.contains("reward_beneficiary"),
        }
    }
}

/// What the executor did before the panicking transaction.
#[derive(Clone, Copy, Debug)]
enum Preceding {
    /// Nothing: the panicking transaction is the first one.
    Nothing,
    /// A TIMESTAMP transaction, executed and committed: before REX1 it detains the next one.
    CommittedDetaining,
    /// The same, executed but not committed, as when an embedder skips a transaction.
    UncommittedDetaining,
    /// A TIMESTAMP transaction that then failed with a database error: before REX1 its detention
    /// carries over all the same.
    FailedDetaining,
    /// A deposit that failed validation: the `FailedDeposit` halt `catch_error` builds, committed.
    FailedDeposit,
}

impl Preceding {
    const ALL: [Self; 5] = [
        Self::Nothing,
        Self::CommittedDetaining,
        Self::UncommittedDetaining,
        Self::FailedDetaining,
        Self::FailedDeposit,
    ];

    /// Whether the preceding transaction got as far as loading the beneficiary, which leaves it in
    /// the state cache where no later read of it can reach the panicking database. A database
    /// error inside a frame is deferred to the result, so that transaction still runs its
    /// post-execution and the beneficiary reward loads it. A failed deposit reaches post-execution
    /// too: before REX6 the reward skips deposits, from REX6 the fee-recipient snapshot taken
    /// there loads the beneficiary first.
    fn caches_beneficiary(self, spec: MegaSpecId) -> bool {
        match self {
            Self::Nothing => false,
            Self::CommittedDetaining | Self::UncommittedDetaining | Self::FailedDetaining => true,
            Self::FailedDeposit => spec.is_enabled(MegaSpecId::REX6),
        }
    }

    fn apply(self, executor: &mut Executor<'_>) {
        match self {
            Self::Nothing => {}
            Self::CommittedDetaining => {
                let tx = legacy_tx(NEXT_SENDER, 0, READ_TIMESTAMP, Bytes::new());
                let outcome =
                    executor.execute_mega_transaction(&tx).expect("detaining tx executes");
                executor.commit_execution_outcome(outcome).expect("detaining tx commits");
            }
            Self::UncommittedDetaining => {
                let tx = legacy_tx(NEXT_SENDER, 0, READ_TIMESTAMP, Bytes::new());
                executor.execute_mega_transaction(&tx).expect("detaining tx executes");
            }
            Self::FailedDetaining => {
                executor.evm.db_mut().database.inner.fail_on_account = Some(FAILING_ACCOUNT);
                let tx = legacy_tx(NEXT_SENDER, 0, READ_TIMESTAMP_THEN_FAIL, Bytes::new());
                executor.execute_mega_transaction(&tx).expect_err("the account read fails");
                executor.evm.db_mut().database.inner.fail_on_account = None;
            }
            Self::FailedDeposit => {
                let tx = deposit_tx(POOR_DEPOSITOR, RETURN_MSIZE, U256::from(1), Bytes::new());
                let outcome = executor
                    .execute_mega_transaction(&tx)
                    .expect("a failed deposit halts rather than errors");
                assert!(
                    matches!(outcome.result_and_state.result, ExecutionResult::Halt { .. }),
                    "{:?}",
                    outcome.result_and_state.result
                );
                executor.commit_execution_outcome(outcome).expect("the failed deposit commits");
            }
        }
    }
}

/// The discard is right wherever the panic lands and whatever the preceding transaction did:
/// afterwards the probe transaction behaves exactly as on an executor with the same history and no
/// panicked transaction, and the detained cap is the one that history left. The rows are
/// (spec) × (what preceded) × (panic site). Each row also checks that the injected panic landed at
/// the intended stage, so a site that silently moved fails instead of passing vacuously. The
/// beneficiary-reward site is unreachable once a preceding transaction has put the beneficiary in
/// the state cache; those rows are skipped, and the skip set is asserted.
#[test]
fn test_discard_transaction_is_right_for_every_panic_site_and_preceding_outcome() {
    let probe = legacy_tx(PROBE_SENDER, 0, RETURN_MSIZE, Bytes::new());
    let specs = [MegaSpecId::REX, MegaSpecId::REX5, MegaSpecId::REX6];
    let (mut rows, mut skipped) = (0, 0);
    for spec in specs {
        for preceding in Preceding::ALL {
            for site in PanicSite::ALL {
                let row = format!("{spec:?} / {preceding:?} / {site:?}");

                let mut reference_state = State::builder().with_database(database()).build();
                let mut reference = new_executor(&mut reference_state, spec);
                preceding.apply(&mut reference);
                let cap_before = detained_cap(&reference);
                let expected = reference
                    .execute_mega_transaction(&probe)
                    .expect("the probe executes on the reference");
                let cap_after = detained_cap(&reference);

                let mut state = State::builder().with_database(database()).build();
                let mut executor = new_executor(&mut state, spec);
                preceding.apply(&mut executor);
                let usage_before = usage(&executor);
                let (panic_on, tx) = site.inject();
                let cached = executor.evm.db_mut().cache.accounts.contains_key(&panic_on);
                assert_eq!(
                    cached,
                    matches!(site, PanicSite::BeneficiaryReward) &&
                        preceding.caches_beneficiary(spec),
                    "{row}: the panicking account is unexpectedly (not) in the state cache"
                );
                if cached {
                    skipped += 1;
                    continue;
                }
                rows += 1;
                executor.evm.db_mut().database.panic_on_account = panic_on;
                execute_panicking(&mut executor, &tx);
                executor.evm.db_mut().database.panic_on_account = PANICKING_ACCOUNT;

                // The injection landed where the row says it does: the call stack of the panic
                // names the stage, and the trackers show whether `on_new_tx` ran.
                let backtrace = executor
                    .evm
                    .db_mut()
                    .database
                    .last_panic_backtrace
                    .take()
                    .expect("the injected panic captured its call stack");
                assert!(
                    site.landed_as_intended(spec, &backtrace),
                    "{row}: the panic landed elsewhere:\n{backtrace}"
                );
                let usage_after = usage(&executor);
                assert_eq!(
                    usage_after.data_size == usage_before.data_size,
                    site.precedes_on_new_tx(spec),
                    "{row}: the panic did not land on the intended side of on_new_tx"
                );

                executor.discard_transaction();
                assert_eq!(detained_cap(&executor), cap_before, "{row}: the cap after the discard");
                let cleared = usage(&executor);
                assert_eq!(
                    (
                        cleared.data_size,
                        cleared.kv_updates,
                        cleared.compute_gas,
                        cleared.state_growth
                    ),
                    (0, 0, 0, 0),
                    "{row}: the usage after the discard"
                );

                let actual = executor
                    .execute_mega_transaction(&probe)
                    .expect("the probe executes after the discard");
                assert_same_outcome_in(&actual, &expected, &row);
                assert_eq!(detained_cap(&executor), cap_after, "{row}: the cap after the probe");
            }
        }
    }
    assert_eq!(rows + skipped, specs.len() * Preceding::ALL.len() * PanicSite::ALL.len());
    assert_eq!(skipped, 3 + 3 + 4, "one beneficiary row per caching history per spec");
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
