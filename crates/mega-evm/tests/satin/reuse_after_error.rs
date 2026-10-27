//! An EVM reused after a call failed with an error runs its next call as a fresh EVM runs it.
//!
//! A call that fails with an error — a read the database could not serve — is no result a block
//! includes, and what it left in the EVM is the handler's fatal cleanup's to drop
//! (`MegaHandler::catch_error`): the error a Host load recorded and nothing took, the failed
//! call's journal transaction, op-revm's per-transaction caches and the frame stack. Every entry
//! point that takes the state off the journal on a success does so on an error too; a `_one` entry
//! point leaves the journal to its caller, so the calls before the failed one stay in it and none
//! of the failed call's writes do; a failed deposit is a result, whose nonce bump and mint stay.
//! The entry-point table runs a failing call through every public entry point. Nothing here needs
//! an inspector: the inspected entry points run on a no-op one.
//!
//! This file pins the state a failed call leaves behind; `db_error.rs` pins the error itself.

use std::collections::BTreeMap;

use alloy_eips::{eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS};
use alloy_op_evm::OpTx;
use alloy_primitives::{address, Address, Bytes, TxKind, B256, U256};
use mega_evm::{
    mainnet_hardforks,
    op_revm::constants::{BASE_FEE_RECIPIENT, L1_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT},
    system::{transact_apply_pending_changes, SEQUENCER_REGISTRY_ADDRESS},
    test_utils::{op_transaction, BytecodeBuilder, ErrorInjectingDatabase, MemoryDatabase},
    transact_beacon_root_contract_call, transact_blockhashes_contract_call, EvmTxRuntimeLimits,
    MegaEvm, MegaHaltReason, MegaTransaction, MegaTransactionOutcome, TX_BODY_SIZE,
};
use revm::{
    bytecode::opcode::{BALANCE, POP, SLOAD},
    context::{
        result::{ExecResultAndState, ExecutionResult},
        BlockEnv, ContextSetters, ContextTr, TxEnv,
    },
    context_interface::LocalContextTr,
    handler::{EvmTr, SystemCallEvm},
    inspector::NoOpInspector,
    state::EvmState,
    ExecuteCommitEvm, ExecuteEvm, InspectCommitEvm, InspectEvm, InspectSystemCallEvm,
};

use crate::common::{block, call, context};

const SENDER: Address = address!("0000000000000000000000000000000000e10000");
/// Another sender, of the transaction a `_one` journal holds before the failing one.
const EARLIER_SENDER: Address = address!("0000000000000000000000000000000000e10001");
/// `SSTORE(0, 7)`, then a call to `CHILD`.
const WRITER: Address = address!("0000000000000000000000000000000000e10002");
/// Reads slot 1, which the database fails.
const CHILD: Address = address!("0000000000000000000000000000000000e10003");
/// `SSTORE(0, 1)`: the call that runs before the failing one.
const KEPT: Address = address!("0000000000000000000000000000000000e10004");
/// Reads the balance of the account the test names.
const READS: Address = address!("0000000000000000000000000000000000e10005");
/// `SSTORE(0, 7)`, then a read of the beneficiary's balance.
const READS_BENEFICIARY: Address = address!("0000000000000000000000000000000000e10006");
/// `SSTORE(0, 7)`.
const WRITES: Address = address!("0000000000000000000000000000000000e10007");
const BENEFICIARY: Address = address!("0000000000000000000000000000000000e1bef0");
const GAS_LIMIT: u64 = 100_000_000;
/// What each sender holds before the test.
const FUNDS: u128 = 10u128.pow(18);

/// A block whose beneficiary is [`BENEFICIARY`].
fn block_with_beneficiary() -> BlockEnv {
    BlockEnv { beneficiary: BENEFICIARY, ..block() }
}

/// [`WRITER`]'s code.
fn writer() -> Bytes {
    BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(7))
        .call(CHILD, U256::ZERO)
        .append(POP)
        .stop()
        .build()
}

/// The pre-state every test starts from: the senders funded, and the contracts.
fn database() -> MemoryDatabase {
    let child = BytecodeBuilder::default().push_number(1_u64).append(SLOAD).append(POP).stop();
    MemoryDatabase::default()
        .account_balance(SENDER, U256::from(FUNDS))
        .account_balance(EARLIER_SENDER, U256::from(FUNDS))
        .account_code(WRITER, writer())
        .account_code(CHILD, child.build())
        .account_code(KEPT, BytecodeBuilder::default().sstore(U256::ZERO, U256::ONE).stop().build())
}

/// A Satin EVM over `db`, in a block whose beneficiary is [`BENEFICIARY`].
fn evm_over(db: ErrorInjectingDatabase) -> Evm {
    MegaEvm::new(context(db).with_block(block_with_beneficiary()))
}

/// Asserts `reused` is the outcome `fresh` is, in every part: the result, the state, the gas by
/// ledger, the usage, the stop and the oracle reads.
fn assert_same_outcome(reused: &MegaTransactionOutcome, fresh: &MegaTransactionOutcome, row: &str) {
    assert_eq!(reused.result, fresh.result, "{row}: the result");
    assert_eq!(reused.state, fresh.state, "{row}: the state");
    assert_eq!(reused.gas, fresh.gas, "{row}: the gas by ledger");
    assert_eq!(reused.usage, fresh.usage, "{row}: the usage");
    assert_eq!(reused.limit_exceeded, fresh.limit_exceeded, "{row}: the stop");
    assert_eq!(reused.oracle_reads, fresh.oracle_reads, "{row}: the oracle reads");
}

/// The transaction's own frame reads an account and the database fails the read: the Host records
/// the error and the frame ends, and post-execution then reads the same account — the beneficiary
/// it rewards, or a fee recipient it credits — and fails first, so the handler never takes the
/// recorded error. The fatal cleanup drops it with the failed transaction: the EVM's error slot is
/// empty, and a different transaction run next on the same EVM, the database serving again, has
/// the outcome a fresh EVM gives it.
#[test]
fn test_an_error_post_execution_preempts_does_not_fail_the_next_transaction() {
    for account in [BENEFICIARY, L1_FEE_RECIPIENT, BASE_FEE_RECIPIENT, OPERATOR_FEE_RECIPIENT] {
        let row = format!("a read of {account}");
        let reads =
            BytecodeBuilder::default().push_address(account).append(BALANCE).append(POP).stop();
        let reads = reads.build();
        let db = || database().account_code(READS, reads.clone());
        let mut armed = ErrorInjectingDatabase::new(db());
        armed.fail_on_account = Some(account);
        let mut evm = evm_over(armed);

        let error = evm
            .execute_transaction(call(SENDER, READS, U256::ZERO, GAS_LIMIT))
            .expect_err("the read fails");
        assert!(format!("{error:?}").contains(&format!("{account}")), "{row}: {error:?}");
        assert!(evm.ctx_mut().error().is_ok(), "{row}: the recorded error is dropped");

        evm.ctx_mut().db_mut().fail_on_account = None;
        let next = call(SENDER, KEPT, U256::ZERO, GAS_LIMIT);
        let reused = evm.execute_transaction(next.clone()).expect("the next transaction is valid");
        let fresh = evm_over(ErrorInjectingDatabase::new(db()))
            .execute_transaction(next)
            .expect("the next transaction is valid");
        assert!(fresh.result.is_success(), "{row}: {:?}", fresh.result);
        assert_same_outcome(&reused, &fresh, &row);
    }
}

/// A transaction run through `transact_one` whose child's read fails after its own frame wrote a
/// slot leaves none of its writes in the journal — its nonce bump, its fee, the slot — and the
/// transaction the journal held before it stays: `transact_one` leaves the journal to its caller
/// on an error as on a success, and the failure takes back only its own transaction.
#[test]
fn test_a_failed_transact_one_keeps_the_transaction_before_it_and_none_of_its_own_writes() {
    let mut db = ErrorInjectingDatabase::new(database());
    db.fail_on_storage = Some((CHILD, U256::ONE));
    let mut evm = evm_over(db);
    let earlier = evm
        .transact_one(call(EARLIER_SENDER, KEPT, U256::ZERO, GAS_LIMIT))
        .expect("the earlier transaction is valid");
    assert!(earlier.is_success(), "{earlier:?}");
    let error =
        evm.transact_one(call(SENDER, WRITER, U256::ZERO, GAS_LIMIT)).expect_err("the read fails");
    assert!(format!("{error:?}").contains("injected storage()"), "{error:?}");

    let state = evm.finalize();
    assert_eq!(state[&EARLIER_SENDER].info.nonce, 1, "the earlier transaction's nonce bump");
    assert_eq!(state[&KEPT].storage[&U256::ZERO].present_value, U256::ONE, "its slot");
    let sender = &state[&SENDER];
    assert_eq!(
        (sender.info.nonce, sender.info.balance),
        (0, U256::from(FUNDS)),
        "the failed transaction's nonce bump and fee"
    );
    assert!(!sender.is_touched(), "the failed transaction's sender is touched by nothing kept");
    let written = state.get(&WRITER).and_then(|writer| writer.storage.get(&U256::ZERO));
    assert!(
        written.is_none_or(|slot| slot.present_value == slot.original_value),
        "the failed transaction's slot: {written:?}"
    );
}

/// A deposit run through `transact_one` whose validation refuses it is a failed deposit, a result:
/// its nonce bump and mint stay in the journal, as op-revm commits them, and the journal hands them
/// out when the caller finalizes it.
#[test]
fn test_a_failed_deposit_through_transact_one_keeps_its_nonce_bump_and_mint() {
    const MINT: u128 = 7;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE - 1);
    let mut evm = evm_over(ErrorInjectingDatabase::new(database())).with_tx_runtime_limits(limits);
    let result =
        evm.transact_one(over_the_limit_deposit(SENDER, MINT)).expect("the deposit is included");
    assert!(
        matches!(result, ExecutionResult::Halt { reason: MegaHaltReason::FailedDeposit, .. }),
        "{result:?}"
    );
    let state = evm.finalize();
    let sender = &state[&SENDER].info;
    assert_eq!(sender.nonce, 1, "the failed deposit's nonce bump");
    assert_eq!(sender.balance, U256::from(FUNDS + MINT), "its mint");
}

/// A deposit from `sender` minting `mint`, whose body is over a data-size limit of one byte below
/// [`TX_BODY_SIZE`]: validation refuses it before it reads its sender's account.
fn over_the_limit_deposit(sender: Address, mint: u128) -> MegaTransaction {
    let mut tx = op_transaction(TxEnv {
        caller: sender,
        kind: TxKind::Call(KEPT),
        gas_limit: 100_000,
        ..Default::default()
    });
    tx.deposit.source_hash = B256::repeat_byte(0x22);
    tx.deposit.mint = Some(mint);
    OpTx(tx)
}

/// op-revm turns a deposit's refusal into a failed deposit by reading its sender's account, and a
/// read the database fails there is an error out of that recovery, which skips op-revm's own
/// cleanup of the per-transaction caches. The engine's cleanup runs on every path out of the
/// handler's error handling, so the error leaves the caches empty, the frame stack cleared, the
/// error slot empty and the journal with no transaction open, and the EVM runs the next transaction
/// as a fresh one does.
///
/// No path fills the caches and then fails that read — a deposit computes no L1 cost and runs no
/// frame before its sender is read — so the test fills them itself: the cleanup makes no
/// assumption about what the failed call left.
#[test]
fn test_an_error_in_the_failed_deposit_recovery_leaves_the_evm_clean() {
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_BODY_SIZE - 1);
    let mut db = ErrorInjectingDatabase::new(database());
    db.fail_on_account = Some(SENDER);
    let mut evm = evm_over(db).with_tx_runtime_limits(limits);
    evm.ctx_mut().chain_mut().tx_l1_cost = Some(U256::from(1_000));
    evm.ctx_mut().local_mut().shared_memory_buffer().borrow_mut().resize(64, 0xff);
    evm.ctx_mut().local_mut().set_precompile_error_context("left behind".into());

    let error = evm
        .transact_one(over_the_limit_deposit(SENDER, 7))
        .expect_err("the recovery's read of the sender fails");
    assert!(format!("{error:?}").contains("injected basic()"), "{error:?}");
    assert_eq!(evm.ctx_mut().chain_mut().tx_l1_cost, None, "the cached L1 cost");
    assert!(
        evm.ctx_mut().local_mut().shared_memory_buffer().borrow().is_empty(),
        "the shared memory"
    );
    assert_eq!(evm.ctx_mut().local_mut().take_precompile_error_context(), None);
    assert_eq!(EvmTr::frame_stack(&mut evm).index(), None, "the frame stack");
    assert!(evm.ctx_mut().error().is_ok(), "the error slot");
    let journal = evm.ctx_mut().journal_mut();
    assert_eq!((journal.depth, journal.journal.len()), (0, 0), "no transaction is open");

    // What the refused deposit loaded is the caller's to drop, as after any `transact_one`; the
    // next transaction runs under the spec's limits.
    evm.ctx_mut().db_mut().fail_on_account = None;
    let _ = evm.finalize();
    evm.set_tx_runtime_limits(EvmTxRuntimeLimits::default());
    let next = call(SENDER, KEPT, U256::ZERO, GAS_LIMIT);
    let reused = evm.execute_transaction(next.clone()).expect("the next transaction is valid");
    let fresh = evm_over(ErrorInjectingDatabase::new(database()))
        .execute_transaction(next)
        .expect("the next transaction is valid");
    assert_same_outcome(&reused, &fresh, "after the recovery's error");
}

/// The EVM every row of the entry-point table runs on.
type Evm = MegaEvm<ErrorInjectingDatabase, NoOpInspector>;

/// [`database`], with the contracts the entry-point table's calls run: [`WRITER`]'s code is also
/// at the three addresses the pre-block calls target.
fn table_database() -> MemoryDatabase {
    let writes = || BytecodeBuilder::default().sstore(U256::ZERO, U256::from(7));
    let reads_beneficiary = writes().push_address(BENEFICIARY).append(BALANCE).append(POP);
    database()
        .account_code(WRITES, writes().stop().build())
        .account_code(READS_BENEFICIARY, reads_beneficiary.stop().build())
        .account_code(HISTORY_STORAGE_ADDRESS, writer())
        .account_code(BEACON_ROOTS_ADDRESS, writer())
        .account_code(SEQUENCER_REGISTRY_ADDRESS, writer())
}

/// An EVM over `db` for the entry-point table, with inspection enabled when `inspect`.
fn table_evm(db: ErrorInjectingDatabase, inspect: bool) -> Evm {
    let evm = evm_over(db);
    if inspect {
        evm.with_inspector(NoOpInspector)
    } else {
        evm
    }
}

/// Where the failing call's error comes from.
#[derive(Clone, Copy, Debug)]
enum Site {
    /// A child's read of slot 1, after the call's own frame wrote slot 0: the child ends with the
    /// error, its caller's return takes it, and the call fails with its own frame's write open in
    /// the journal.
    Nested,
    /// The call's own frame writes slot 0 and reads the beneficiary, which the database fails: the
    /// Host records the error and the frame ends, and post-execution's read of the beneficiary
    /// fails first, so the recorded error is never taken. A system call has no post-execution.
    Preempted,
    /// The call's own frame writes slot 0 and succeeds, and post-execution's read of a fee
    /// recipient fails. A system call has no post-execution.
    PostExecution,
}

impl Site {
    const ALL: [Self; 3] = [Self::Nested, Self::Preempted, Self::PostExecution];

    /// The contract a transaction or a system call of the table calls.
    const fn contract(self) -> Address {
        match self {
            Self::Nested => WRITER,
            Self::Preempted => READS_BENEFICIARY,
            Self::PostExecution => WRITES,
        }
    }

    /// Makes `db` fail the read the site's call fails on.
    const fn arm(self, db: &mut ErrorInjectingDatabase) {
        match self {
            Self::Nested => db.fail_on_storage = Some((CHILD, U256::ONE)),
            Self::Preempted => db.fail_on_account = Some(BENEFICIARY),
            Self::PostExecution => db.fail_on_account = Some(BASE_FEE_RECIPIENT),
        }
    }

    /// Makes `db` serve every read again.
    const fn disarm(db: &mut ErrorInjectingDatabase) {
        db.fail_on_storage = None;
        db.fail_on_account = None;
    }
}

/// Every public way to run a transaction or a system call on a `MegaEvm`: the inherent method,
/// the required and provided methods of every execution trait it implements, revm's and
/// alloy-evm's `Evm`, and the pre-block calls. The deprecated aliases are still callable and route
/// through the provided methods. alloy-evm's `EvmExt` replays compose `transact` and
/// `transact_commit`, which are rows. [`EntryPoint::kind`] classifies every variant without a
/// wildcard, so a new variant does not compile until it is classified; it runs once `ALL` lists it.
#[derive(Clone, Copy, Debug)]
enum EntryPoint {
    ExecuteTransaction,
    Transact,
    TransactMany,
    TransactManyFinalize,
    Replay,
    TransactCommit,
    TransactManyCommit,
    ReplayCommit,
    InspectTx,
    Inspect,
    InspectTxCommit,
    InspectCommit,
    AlloyTransact,
    AlloyTransactRaw,
    AlloyTransactCommit,
    TransactOne,
    InspectOneTx,
    InspectOne,
    SystemCall,
    SystemCallWithCaller,
    DeprecatedTransactSystemCallFinalize,
    DeprecatedTransactSystemCallWithCallerFinalize,
    AlloyTransactSystemCall,
    InspectSystemCall,
    InspectSystemCallWithCaller,
    InspectSystemCallWithInspector,
    InspectSystemCallWithInspectorAndCaller,
    BlockHashesPreBlockCall,
    BeaconRootPreBlockCall,
    ApplyPendingChangesPreBlockCall,
    SystemCallOne,
    SystemCallOneWithCaller,
    DeprecatedTransactSystemCall,
    DeprecatedTransactSystemCallWithCaller,
    InspectOneSystemCall,
    InspectOneSystemCallWithCaller,
    InspectOneSystemCallWithInspector,
    InspectOneSystemCallWithInspectorAndCaller,
}

impl EntryPoint {
    const ALL: [Self; 38] = [
        Self::ExecuteTransaction,
        Self::Transact,
        Self::TransactMany,
        Self::TransactManyFinalize,
        Self::Replay,
        Self::TransactCommit,
        Self::TransactManyCommit,
        Self::ReplayCommit,
        Self::InspectTx,
        Self::Inspect,
        Self::InspectTxCommit,
        Self::InspectCommit,
        Self::AlloyTransact,
        Self::AlloyTransactRaw,
        Self::AlloyTransactCommit,
        Self::TransactOne,
        Self::InspectOneTx,
        Self::InspectOne,
        Self::SystemCall,
        Self::SystemCallWithCaller,
        Self::DeprecatedTransactSystemCallFinalize,
        Self::DeprecatedTransactSystemCallWithCallerFinalize,
        Self::AlloyTransactSystemCall,
        Self::InspectSystemCall,
        Self::InspectSystemCallWithCaller,
        Self::InspectSystemCallWithInspector,
        Self::InspectSystemCallWithInspectorAndCaller,
        Self::BlockHashesPreBlockCall,
        Self::BeaconRootPreBlockCall,
        Self::ApplyPendingChangesPreBlockCall,
        Self::SystemCallOne,
        Self::SystemCallOneWithCaller,
        Self::DeprecatedTransactSystemCall,
        Self::DeprecatedTransactSystemCallWithCaller,
        Self::InspectOneSystemCall,
        Self::InspectOneSystemCallWithCaller,
        Self::InspectOneSystemCallWithInspector,
        Self::InspectOneSystemCallWithInspectorAndCaller,
    ];

    /// Whether the entry point takes the state off the journal on an error, and whether it runs a
    /// system call rather than a transaction. The `_one` entry points leave the journal to the
    /// caller on both outcomes, so several calls can share one journal; `transact_many` takes it
    /// off on an error only, leaving a success's state to the caller as they do.
    const fn kind(self) -> (bool, bool) {
        match self {
            Self::ExecuteTransaction |
            Self::Transact |
            Self::TransactMany |
            Self::TransactManyFinalize |
            Self::Replay |
            Self::TransactCommit |
            Self::TransactManyCommit |
            Self::ReplayCommit |
            Self::InspectTx |
            Self::Inspect |
            Self::InspectTxCommit |
            Self::InspectCommit |
            Self::AlloyTransact |
            Self::AlloyTransactRaw |
            Self::AlloyTransactCommit => (true, false),
            Self::TransactOne | Self::InspectOneTx | Self::InspectOne => (false, false),
            Self::SystemCall |
            Self::SystemCallWithCaller |
            Self::DeprecatedTransactSystemCallFinalize |
            Self::DeprecatedTransactSystemCallWithCallerFinalize |
            Self::AlloyTransactSystemCall |
            Self::InspectSystemCall |
            Self::InspectSystemCallWithCaller |
            Self::InspectSystemCallWithInspector |
            Self::InspectSystemCallWithInspectorAndCaller |
            Self::BlockHashesPreBlockCall |
            Self::BeaconRootPreBlockCall |
            Self::ApplyPendingChangesPreBlockCall => (true, true),
            Self::SystemCallOne |
            Self::SystemCallOneWithCaller |
            Self::DeprecatedTransactSystemCall |
            Self::DeprecatedTransactSystemCallWithCaller |
            Self::InspectOneSystemCall |
            Self::InspectOneSystemCallWithCaller |
            Self::InspectOneSystemCallWithInspector |
            Self::InspectOneSystemCallWithInspectorAndCaller => (false, true),
        }
    }

    /// Whether the entry point takes the state off the journal on an error ([`kind`](Self::kind)).
    const fn finalizes(self) -> bool {
        self.kind().0
    }

    /// Whether the entry point runs a system call ([`kind`](Self::kind)).
    const fn is_system_call(self) -> bool {
        self.kind().1
    }

    /// The contract the call runs: a pre-block call's own, and otherwise the site's.
    const fn target(self, site: Site) -> Address {
        match self {
            Self::BlockHashesPreBlockCall => HISTORY_STORAGE_ADDRESS,
            Self::BeaconRootPreBlockCall => BEACON_ROOTS_ADDRESS,
            Self::ApplyPendingChangesPreBlockCall => SEQUENCER_REGISTRY_ADDRESS,
            _ => site.contract(),
        }
    }

    /// Runs the site's call through the entry point — a transaction from [`SENDER`], or a system
    /// call, from [`SENDER`] where the entry point takes a caller — and answers its result or its
    /// error. The state an entry point hands back is committed to the database; what a `_one`
    /// entry point leaves in the journal stays there.
    fn run(self, evm: &mut Evm, site: Site) -> Result<ExecutionResult<MegaHaltReason>, String> {
        let (result, state) = self.run_handing_back(evm, site)?;
        if let Some(state) = state {
            ExecuteCommitEvm::commit(evm, state);
        }
        Ok(result)
    }

    /// [`run`](Self::run), with the state the entry point handed back, if it handed any.
    #[allow(deprecated)]
    fn run_handing_back(
        self,
        evm: &mut Evm,
        site: Site,
    ) -> Result<(ExecutionResult<MegaHaltReason>, Option<EvmState>), String> {
        let to = self.target(site);
        let tx = || call(SENDER, to, U256::ZERO, GAS_LIMIT);
        fn handing_back<E: core::fmt::Debug>(
            ran: Result<ExecResultAndState<ExecutionResult<MegaHaltReason>, EvmState>, E>,
        ) -> Result<(ExecutionResult<MegaHaltReason>, Option<EvmState>), String> {
            ran.map(|ran| (ran.result, Some(ran.state))).map_err(|error| format!("{error:?}"))
        }
        fn handing_back_nothing<E: core::fmt::Debug>(
            ran: Result<ExecutionResult<MegaHaltReason>, E>,
        ) -> Result<(ExecutionResult<MegaHaltReason>, Option<EvmState>), String> {
            ran.map(|result| (result, None)).map_err(|error| format!("{error:?}"))
        }
        let first = |results: Vec<ExecutionResult<MegaHaltReason>>| {
            results.into_iter().next().expect("one result")
        };
        match self {
            Self::ExecuteTransaction => {
                handing_back(evm.execute_transaction(tx()).map(|outcome| outcome.result_and_state))
            }
            Self::Transact => handing_back(ExecuteEvm::transact(evm, tx())),
            Self::TransactMany => {
                // The journal holds a success until the caller finalizes it.
                let results =
                    handing_back_nothing(evm.transact_many(core::iter::once(tx())).map(first))?;
                Ok((results.0, Some(evm.finalize())))
            }
            Self::TransactManyFinalize => handing_back(
                evm.transact_many_finalize(core::iter::once(tx()))
                    .map(|ran| ExecResultAndState::new(first(ran.result), ran.state)),
            ),
            Self::Replay => {
                evm.ctx_mut().set_tx(tx());
                handing_back(ExecuteEvm::replay(evm))
            }
            Self::TransactCommit => {
                handing_back_nothing(ExecuteCommitEvm::transact_commit(evm, tx()))
            }
            Self::TransactManyCommit => {
                handing_back_nothing(evm.transact_many_commit(core::iter::once(tx())).map(first))
            }
            Self::ReplayCommit => {
                evm.ctx_mut().set_tx(tx());
                handing_back_nothing(evm.replay_commit())
            }
            Self::InspectTx => handing_back(evm.inspect_tx(tx())),
            Self::Inspect => handing_back(InspectEvm::inspect(evm, tx(), NoOpInspector)),
            Self::InspectTxCommit => handing_back_nothing(evm.inspect_tx_commit(tx())),
            Self::InspectCommit => handing_back_nothing(evm.inspect_commit(tx(), NoOpInspector)),
            Self::AlloyTransact => handing_back(alloy_evm::Evm::transact(evm, tx())),
            Self::AlloyTransactRaw => handing_back(alloy_evm::Evm::transact_raw(evm, tx())),
            Self::AlloyTransactCommit => {
                handing_back_nothing(alloy_evm::Evm::transact_commit(evm, tx()))
            }
            Self::TransactOne => handing_back_nothing(evm.transact_one(tx())),
            Self::InspectOneTx => handing_back_nothing(evm.inspect_one_tx(tx())),
            Self::InspectOne => handing_back_nothing(evm.inspect_one(tx(), NoOpInspector)),
            Self::SystemCall => handing_back(evm.system_call(to, Bytes::new())),
            Self::SystemCallWithCaller => {
                handing_back(SystemCallEvm::system_call_with_caller(evm, SENDER, to, Bytes::new()))
            }
            Self::DeprecatedTransactSystemCallFinalize => {
                handing_back(evm.transact_system_call_finalize(to, Bytes::new()))
            }
            Self::DeprecatedTransactSystemCallWithCallerFinalize => handing_back(
                evm.transact_system_call_with_caller_finalize(SENDER, to, Bytes::new()),
            ),
            Self::AlloyTransactSystemCall => {
                handing_back(alloy_evm::Evm::transact_system_call(evm, SENDER, to, Bytes::new()))
            }
            Self::InspectSystemCall => handing_back(evm.inspect_system_call(to, Bytes::new())),
            Self::InspectSystemCallWithCaller => {
                handing_back(evm.inspect_system_call_with_caller(SENDER, to, Bytes::new()))
            }
            Self::InspectSystemCallWithInspector => handing_back(
                evm.inspect_system_call_with_inspector(to, Bytes::new(), NoOpInspector),
            ),
            Self::InspectSystemCallWithInspectorAndCaller => {
                handing_back(evm.inspect_system_call_with_inspector_and_caller(
                    SENDER,
                    to,
                    Bytes::new(),
                    NoOpInspector,
                ))
            }
            Self::BlockHashesPreBlockCall => handing_back(
                transact_blockhashes_contract_call(mainnet_hardforks(), B256::repeat_byte(1), evm)
                    .map(|ran| ran.expect("Prague is active")),
            ),
            Self::BeaconRootPreBlockCall => handing_back(
                transact_beacon_root_contract_call(
                    mainnet_hardforks(),
                    Some(B256::repeat_byte(2)),
                    evm,
                )
                .map(|ran| ran.expect("Cancun is active")),
            ),
            Self::ApplyPendingChangesPreBlockCall => {
                handing_back(transact_apply_pending_changes(evm))
            }
            Self::SystemCallOne => handing_back_nothing(evm.system_call_one(to, Bytes::new())),
            Self::SystemCallOneWithCaller => {
                handing_back_nothing(evm.system_call_one_with_caller(SENDER, to, Bytes::new()))
            }
            Self::DeprecatedTransactSystemCall => {
                handing_back_nothing(SystemCallEvm::transact_system_call(evm, to, Bytes::new()))
            }
            Self::DeprecatedTransactSystemCallWithCaller => {
                handing_back_nothing(evm.transact_system_call_with_caller(SENDER, to, Bytes::new()))
            }
            Self::InspectOneSystemCall => {
                handing_back_nothing(evm.inspect_one_system_call(to, Bytes::new()))
            }
            Self::InspectOneSystemCallWithCaller => handing_back_nothing(
                evm.inspect_one_system_call_with_caller(SENDER, to, Bytes::new()),
            ),
            Self::InspectOneSystemCallWithInspector => handing_back_nothing(
                evm.inspect_one_system_call_with_inspector(to, Bytes::new(), NoOpInspector),
            ),
            Self::InspectOneSystemCallWithInspectorAndCaller => {
                handing_back_nothing(evm.inspect_one_system_call_with_inspector_and_caller(
                    SENDER,
                    to,
                    Bytes::new(),
                    NoOpInspector,
                ))
            }
        }
    }
}

/// What the database holds, account by account: nonce, balance, code hash and storage.
fn committed(evm: &Evm) -> BTreeMap<Address, (u64, U256, B256, BTreeMap<U256, U256>)> {
    let accounts = &evm.ctx().db().cache.accounts;
    accounts
        .iter()
        .map(|(address, account)| {
            let storage = account.storage.iter().map(|(key, value)| (*key, *value)).collect();
            let info = &account.info;
            (*address, (info.nonce, info.balance, info.code_hash, storage))
        })
        .collect()
}

/// A call that fails with an error leaves nothing the next call depends on, whichever public entry
/// point ran it and wherever the error came from: the error slot is empty, and an entry point that
/// finalizes the journal on a success hands it back empty on an error too, while a `_one` entry
/// point leaves a journal that holds the call before the failed one and none of the failed call's
/// writes. The same call run again on the same EVM, the database serving again, has the result a
/// fresh EVM gives it and leaves the database as the fresh one leaves it. Each entry point runs on
/// an EVM with inspection enabled and on one without, which only alloy-evm's `transact` methods and
/// `execute_transaction` read; a system call has no post-execution, so the two post-execution sites
/// are the transactions'.
#[test]
fn test_every_entry_point_leaves_nothing_of_a_failed_call() {
    for inspect in [false, true] {
        for site in Site::ALL {
            for entry in EntryPoint::ALL {
                if entry.is_system_call() && !matches!(site, Site::Nested) {
                    continue;
                }
                assert_a_failed_call_leaves_nothing(entry, site, inspect);
            }
        }
    }
}

fn assert_a_failed_call_leaves_nothing(entry: EntryPoint, site: Site, inspect: bool) {
    let row = format!("{entry:?} / {site:?} / inspect={inspect}");
    // A system call before the failing one, for a `_one` entry point's journal to hold: it reads
    // neither the beneficiary nor a fee recipient, which the failing call's database fails.
    let earlier = |evm: &mut Evm| {
        if !entry.finalizes() {
            let earlier = evm
                .system_call_one_with_caller(SENDER, KEPT, Bytes::new())
                .expect("the earlier call runs");
            assert!(earlier.is_success(), "{row}: {earlier:?}");
        }
    };

    let mut evm = table_evm(ErrorInjectingDatabase::new(table_database()), inspect);
    earlier(&mut evm);
    site.arm(evm.ctx_mut().db_mut());
    let failed = entry.run(&mut evm, site);
    assert!(matches!(&failed, Err(error) if error.contains("injected")), "{row}: {failed:?}");
    Site::disarm(evm.ctx_mut().db_mut());
    assert!(evm.ctx_mut().error().is_ok(), "{row}: the error slot");
    if entry.finalizes() {
        assert!(evm.finalize().is_empty(), "{row}: the journal is handed back empty");
    } else {
        let target = entry.target(site);
        let journal = evm.ctx_mut().journal_mut();
        assert_eq!((journal.depth, journal.journal.len()), (0, 0), "{row}: no transaction open");
        assert_eq!(
            journal.state[&KEPT].storage[&U256::ZERO].present_value,
            U256::ONE,
            "{row}: the earlier call's write"
        );
        let sender = journal.state.get(&SENDER).map(|sender| sender.info.nonce);
        assert!(sender.is_none_or(|nonce| nonce == 0), "{row}: the failed call's nonce bump");
        let written = journal.state.get(&target).and_then(|target| target.storage.get(&U256::ZERO));
        assert!(
            written.is_none_or(|slot| slot.present_value == slot.original_value),
            "{row}: the failed call's write: {written:?}"
        );
    }
    let retried = entry.run(&mut evm, site);
    let mut fresh = table_evm(ErrorInjectingDatabase::new(table_database()), inspect);
    earlier(&mut fresh);
    let expected = entry.run(&mut fresh, site);
    assert!(matches!(&expected, Ok(result) if result.is_success()), "{row}: {expected:?}");
    assert_eq!(retried, expected, "{row}: the call run again");
    if !entry.finalizes() {
        for evm in [&mut evm, &mut fresh] {
            let state = evm.finalize();
            ExecuteCommitEvm::commit(evm, state);
        }
    }
    assert_eq!(committed(&evm), committed(&fresh), "{row}: what the database holds");
}
