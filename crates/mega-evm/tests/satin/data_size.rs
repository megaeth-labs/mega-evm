//! The data-size limit: the 98% a child frame receives, the transaction body, and deployed code.
//!
//! A child frame's budget is 98% of what its parent has left. Crossing that budget reverts the
//! child alone and its parent resumes. Crossing the transaction's own limit stops the transaction
//! through the same latch every other transaction-level limit uses.
//!
//! The body is counted before any frame and kept on every path. Deployed code is counted on the
//! creation's lane, one byte per byte, before the creation is committed.

use alloy_evm::Evm;
use alloy_primitives::{address, keccak256, Address, Bytes, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm, MegaHaltReason,
    MegaLimitExceeded, MegaTransactionOutcome, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, CREATE, GAS, LOG0, POP, PUSH0, RETURN, RETURNDATACOPY, RETURNDATASIZE, REVERT,
        SELFDESTRUCT, STOP,
    },
    context::result::{ExecutionResult, HaltReason},
    context_interface::cfg::GasId,
    interpreter::{
        interpreter::EthInterpreter, CreateInputs, CreateOutcome, InstructionResult, Interpreter,
    },
    Database, Inspector,
};

use crate::common::{call, call_with_data, context, create};

const CALLER: Address = address!("0000000000000000000000000000000000300000");
const A: Address = address!("0000000000000000000000000000000000300001");
const B: Address = address!("0000000000000000000000000000000000300002");
const C: Address = address!("0000000000000000000000000000000000300003");
const D: Address = address!("0000000000000000000000000000000000300004");

const GAS_LIMIT: u64 = 20_000_000;

/// 98% of `remaining`, the share a child frame is given.
fn share(remaining: u64) -> u64 {
    (u128::from(remaining) * u128::from(FRAME_DATA_SHARE_NUMERATOR) /
        u128::from(FRAME_DATA_SHARE_DENOMINATOR)) as u64
}

/// Calls `next` with no value, then writes slot 2: the write is what a resumed caller runs.
fn call_then_store(next: Address) -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(next)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .sstore(U256::from(2), U256::from(1))
        .stop()
        .build()
}

/// `LOG0` of `data_len` bytes at memory offset 0.
fn log0(data_len: u64) -> Bytes {
    BytecodeBuilder::default()
        .push_number(data_len)
        .push_number(0_u64)
        .append(LOG0)
        .append(STOP)
        .build()
}

/// `A -> B -> C -> D`. `D` is at depth 3.
fn chain(innermost: Bytes) -> mega_evm::test_utils::MemoryDatabase {
    mega_evm::test_utils::MemoryDatabase::default()
        .account_balance(CALLER, U256::from(1_000_000))
        .account_code(A, call_then_store(B))
        .account_code(B, call_then_store(C))
        .account_code(C, call_then_store(D))
        .account_code(D, innermost)
}

fn stored(state: &revm::state::EvmState, address: Address) -> bool {
    state.get(&address).is_some_and(|account| {
        account.storage.get(&U256::from(2)).is_some_and(|slot| slot.is_changed())
    })
}

/// The budget at depth 3 when every frame cap is `cap` and no frame has spent any of it before
/// the call that starts the next one.
fn budget_at_depth_3(cap: u64) -> u64 {
    share(share(share(cap)))
}

/// A log that fills `D`'s budget is kept, and `A`, `B` and `C` resume. One byte more reverts
/// `D` alone: the three callers still write the slot after the call, and the transaction is not
/// latched.
#[test]
fn test_depth_3_frame_budget_reverts_the_child_and_the_parents_resume() {
    const CAP: u64 = 20_000;
    let budget = budget_at_depth_3(CAP);
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(CAP);
    let run = |data_len: u64| {
        let db = chain(log0(data_len));
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
        let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
        (
            result,
            evm.ctx().additional_limit().latched().copied(),
            evm.ctx().additional_limit().usage(),
        )
    };

    // 32 bytes for the log's address, and the data. Exactly the budget.
    let (fits, latched, usage) = run(budget - 32);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(latched, None);
    assert!(stored(&fits.state, A) && stored(&fits.state, B) && stored(&fits.state, C));
    assert_eq!(fits.result.logs().len(), 1);
    assert_eq!(usage.write_records, 3, "one slot in each caller");
    assert_eq!(usage.data_size, mega_evm::TX_BODY_SIZE + budget + 3 * 40);

    let (over, latched, usage) = run(budget - 31);
    assert!(over.result.is_success(), "the callers resume: {:?}", over.result);
    assert_eq!(latched, None, "a frame budget does not latch the transaction");
    assert!(stored(&over.state, A) && stored(&over.state, B) && stored(&over.state, C));
    assert!(over.result.logs().is_empty(), "the log died with D");
    assert_eq!(
        usage,
        mega_evm::LimitUsage { data_size: mega_evm::TX_BODY_SIZE + 3 * 40, write_records: 3 }
    );
}

/// The same depth, with the transaction's limit tighter than the frame budgets. `D`'s log crosses
/// the transaction limit, so the latch stops every caller: the slots they wrote are not kept, and
/// the revert names the transaction limit.
#[test]
fn test_depth_3_transaction_limit_stops_the_transaction() {
    const TX_LIMIT: u64 = 8_000;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_LIMIT);
    let db = chain(log0(TX_LIMIT));
    let outcome = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT))
        .unwrap();

    assert!(!outcome.result.is_success());
    assert!(!outcome.result.is_halt(), "a data-size stop is a revert: {:?}", outcome.result);
    // Nothing was written before the log, so the crossing usage is the body plus the log: 32 for
    // its address and `TX_LIMIT` of data. The revert drops the log; the body stays.
    let crossed = mega_evm::TX_BODY_SIZE + 32 + TX_LIMIT;
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: TX_LIMIT,
            used: crossed,
            frame_local: false,
        })
    );
    assert_eq!(
        outcome.usage.data_size,
        mega_evm::TX_BODY_SIZE,
        "the revert drops the log and keeps the body"
    );
    match &outcome.result {
        revm::context::result::ExecutionResult::Revert { output, .. } => {
            assert_eq!(
                output.as_ref(),
                MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: TX_LIMIT }
                    .abi_encode()
            );
        }
        other => panic!("expected the latched revert, got {other:?}"),
    }
    assert!(!stored(&outcome.state, A) && !stored(&outcome.state, B) && !stored(&outcome.state, C));

    // The same chain without the log stays inside the limit and keeps every caller's slot.
    let kept = MegaEvm::new(
        context(chain(BytecodeBuilder::default().append(STOP).build()))
            .with_tx_runtime_limits(limits),
    )
    .execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT))
    .unwrap();
    assert!(kept.result.is_success(), "{:?}", kept.result);
    assert_eq!(kept.limit_exceeded, None);
    assert!(stored(&kept.state, A) && stored(&kept.state, B) && stored(&kept.state, C));
}

/// Counts steps, so a frame answered without running is visible, and the result of each creation.
#[derive(Clone, Default)]
struct Probe {
    steps: u32,
    creates: Vec<(InstructionResult, Bytes)>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Probe {
    fn step(&mut self, _: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.steps += 1;
    }

    fn create_end(
        &mut self,
        _: &mut MegaContext<DB>,
        _: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.creates.push((outcome.result.result, outcome.result.output.clone()));
    }
}

/// Init code that returns `size` zero bytes as the deployed contract.
fn constructor_returning(size: u64) -> Bytes {
    BytecodeBuilder::default().push_number(size).push_number(0_u8).append(RETURN).build()
}

/// Init code that reverts with `len` bytes and deploys nothing.
fn reverting_with(len: u8) -> Bytes {
    BytecodeBuilder::default().push_number(len).push_number(0_u8).append(REVERT).build()
}

/// A contract that `CREATE`s `deployed` bytes and stops.
fn factory_deploying(deployed: u64) -> Bytes {
    let init = constructor_returning(deployed);
    BytecodeBuilder::default()
        .mstore(0, &init)
        .push_number(init.len() as u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(CREATE)
        .append(POP)
        .append(STOP)
        .build()
}

fn funded() -> MemoryDatabase {
    MemoryDatabase::default().account_balance(CALLER, U256::from(10u64.pow(18)))
}

/// Runs a creation transaction of `init` under `limits`, with the inspector on when `inspect` is.
fn run_create(
    init: Bytes,
    limits: EvmTxRuntimeLimits,
    inspect: bool,
) -> (MegaTransactionOutcome, Probe) {
    let mut evm = MegaEvm::new(context(funded()).with_tx_runtime_limits(limits))
        .with_inspector(Probe::default());
    Evm::set_inspector_enabled(&mut evm, inspect);
    let outcome = evm.execute_transaction(create(CALLER, init, GAS_LIMIT)).unwrap();
    let probe = evm.inspector().clone();
    (outcome, probe)
}

/// Calldata is part of the body, and a zero byte counts the same as any other.
#[test]
fn test_calldata_counts_in_the_body() {
    let db = || funded().account_code(A, BytecodeBuilder::default().append(STOP).build());
    let run = |byte: u8| {
        let outcome = MegaEvm::new(context(db()))
            .execute_transaction(call_with_data(CALLER, A, Bytes::from(vec![byte; 50]), GAS_LIMIT))
            .unwrap();
        assert!(outcome.result.is_success(), "{:?}", outcome.result);
        outcome.usage
    };
    let expected = LimitUsage { data_size: mega_evm::TX_BODY_SIZE + 50, write_records: 0 };
    assert_eq!(run(0), expected, "a zero byte");
    assert_eq!(run(0xff), expected, "a non-zero byte");
}

/// A body that crosses the transaction limit stops the transaction before its code runs. The
/// body stays counted: it is what crossed.
#[test]
fn test_a_body_over_the_limit_stops_before_any_frame() {
    let db = funded().account_code(
        A,
        BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build(),
    );
    let limit = mega_evm::TX_BODY_SIZE - 1;
    let mut evm = MegaEvm::new(
        context(db)
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit)),
    )
    .with_inspector(Probe::default());
    let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();

    assert!(!result.result.is_success() && !result.result.is_halt(), "{:?}", result.result);
    assert_eq!(evm.inspector().steps, 0, "the callee never ran");
    assert_eq!(
        evm.ctx().additional_limit().latched().copied(),
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit,
            used: mega_evm::TX_BODY_SIZE,
            frame_local: false,
        })
    );
    assert_eq!(
        evm.ctx().additional_limit().usage(),
        LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
    );
    assert!(
        result.state.get(&A).is_none_or(|account| {
            account.storage.get(&U256::ZERO).is_none_or(|slot| !slot.is_changed())
        }),
        "the callee's write was never made",
    );
}

/// Deployed code is one byte per byte on the creation, on top of the body and the created
/// account. One byte over the transaction limit reverts the creation and leaves no code.
#[test]
fn test_deployed_code_one_byte_over_the_transaction_limit_is_not_left_deployed() {
    const DEPLOYED: u64 = 32;
    let init = constructor_returning(DEPLOYED);
    let body = mega_evm::TX_BODY_SIZE + init.len() as u64;
    let full = body + WRITE_RECORD_SIZE + DEPLOYED;
    let created = CALLER.create(0);
    let code_hash = keccak256(vec![0_u8; DEPLOYED as usize]);

    for inspect in [false, true] {
        let (kept, _) = run_create(
            init.clone(),
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(full),
            inspect,
        );
        assert!(kept.result.is_success(), "inspect {inspect}: {:?}", kept.result);
        assert_eq!(kept.limit_exceeded, None);
        assert_eq!(kept.usage, LimitUsage { data_size: full, write_records: 1 });
        assert_eq!(kept.state[&created].info.code_hash, code_hash, "inspect {inspect}");

        let (over, probe) = run_create(
            init.clone(),
            EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(full - 1),
            inspect,
        );
        assert!(!over.result.is_success() && !over.result.is_halt(), "{:?}", over.result);
        assert_eq!(
            over.limit_exceeded,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit: full - 1,
                used: full,
                frame_local: false,
            })
        );
        assert_eq!(
            over.usage,
            LimitUsage { data_size: body, write_records: 0 },
            "the stop drops the code and the created account; the body stays"
        );
        assert!(
            over.state.get(&created).is_none_or(|account| account.info.is_empty_code_hash()),
            "inspect {inspect}: the code was not deployed: {:?}",
            over.state.get(&created).map(|account| account.info.code_hash),
        );
        assert_eq!(over.state[&CALLER].info.nonce, 1, "the creator's nonce was bumped");
        if inspect {
            assert_eq!(probe.creates.len(), 1);
            assert_eq!(probe.creates[0].0, InstructionResult::Revert);
            assert_eq!(
                probe.creates[0].1.as_ref(),
                MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: full - 1 }
                    .abi_encode()
            );
        }
    }
}

/// A child creation that crosses its own budget reverts alone. The code is not deployed, the
/// creator's nonce record stays, and the caller resumes.
#[test]
fn test_deployed_code_over_the_frame_budget_reverts_the_creation_alone() {
    const CAP: u64 = 1_000;
    // The transaction's frame is capped at `CAP` and has spent nothing, so the creation gets 98%
    // of that. Its start records the created account and the creator's nonce before the code.
    let child_budget = share(CAP);
    let at_start = 2 * WRITE_RECORD_SIZE;
    let fits = child_budget - at_start;
    let db = |deployed| funded().account_code(A, factory_deploying(deployed));
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(CAP);
    let created = A.create(0);

    let run = |deployed, inspect| {
        let mut evm = MegaEvm::new(context(db(deployed)).with_tx_runtime_limits(limits))
            .with_inspector(Probe::default());
        Evm::set_inspector_enabled(&mut evm, inspect);
        let result = evm.transact_raw(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
        (
            result,
            evm.ctx().additional_limit().usage(),
            evm.ctx().additional_limit().latched().copied(),
            evm.inspector().clone(),
        )
    };

    for inspect in [false, true] {
        let (kept, usage, latched, _) = run(fits, inspect);
        assert!(kept.result.is_success(), "inspect {inspect}: {:?}", kept.result);
        assert_eq!(latched, None);
        assert_eq!(
            usage,
            LimitUsage { data_size: mega_evm::TX_BODY_SIZE + child_budget, write_records: 2 }
        );
        assert_eq!(kept.state[&created].info.code_hash, keccak256(vec![0_u8; fits as usize]));

        let (over, usage, latched, probe) = run(fits + 1, inspect);
        assert!(over.result.is_success(), "the caller resumes: {:?}", over.result);
        assert_eq!(latched, None, "a frame budget does not latch the transaction");
        assert_eq!(
            usage,
            LimitUsage { data_size: mega_evm::TX_BODY_SIZE + WRITE_RECORD_SIZE, write_records: 1 },
            "the creator's nonce stays; the code and the created account do not"
        );
        assert!(
            over.state.get(&created).is_none_or(|account| account.info.is_empty_code_hash()),
            "inspect {inspect}: {:?}",
            over.state.get(&created).map(|account| account.info.code_hash),
        );
        assert_eq!(over.state[&A].info.nonce, 1);
        if inspect {
            assert_eq!(probe.creates.len(), 1);
            assert_eq!(probe.creates[0].0, InstructionResult::Revert);
            assert_eq!(
                probe.creates[0].1.as_ref(),
                MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: child_budget }
                    .abi_encode()
            );
        }
    }
}

/// The output of a reverting creation is revert data, not deployed code, so it is not counted.
#[test]
fn test_a_reverting_creation_does_not_count_its_output() {
    let init = reverting_with(40);
    let (outcome, _) = run_create(init.clone(), EvmTxRuntimeLimits::no_limits(), false);
    assert!(!outcome.result.is_success() && !outcome.result.is_halt(), "{:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None, "the revert is the init code's, not a stop");
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: mega_evm::TX_BODY_SIZE + init.len() as u64, write_records: 0 },
    );
}

fn run_at(db: MemoryDatabase, tx: mega_evm::MegaTransaction, limit: u64) -> MegaTransactionOutcome {
    MegaEvm::new(
        context(db)
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit)),
    )
    .execute_transaction(tx)
    .unwrap()
}

fn assert_stopped(outcome: &MegaTransactionOutcome, limit: u64, used: u64) {
    assert!(!outcome.result.is_success() && !outcome.result.is_halt(), "{:?}", outcome.result);
    assert_eq!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit,
            used,
            frame_local: false,
        })
    );
}

/// Calldata is in the body. One byte over that count stops the transaction before the callee
/// runs, so a store the callee would have made is not part of the crossing figure.
#[test]
fn test_calldata_one_byte_over_stops_before_the_callee() {
    let data = Bytes::from(vec![0xab; 50]);
    let body = mega_evm::TX_BODY_SIZE + data.len() as u64;
    let db = || {
        funded().account_code(
            A,
            BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build(),
        )
    };

    let fits = run_at(db(), call_with_data(CALLER, A, data.clone(), GAS_LIMIT), body + 40);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage, LimitUsage { data_size: body + 40, write_records: 1 });

    let over = run_at(db(), call_with_data(CALLER, A, data, GAS_LIMIT), body - 1);
    assert_stopped(&over, body - 1, body);
    assert_eq!(over.usage, LimitUsage { data_size: body, write_records: 0 });
    assert!(over.result.logs().is_empty());
    assert!(over.state.get(&A).is_none_or(|account| {
        account.storage.get(&U256::ZERO).is_none_or(|slot| !slot.is_changed())
    }));
}

/// A log is 32 bytes for its address plus its data. One byte over that stops on the log: the
/// store after it is not in the crossing figure.
#[test]
fn test_a_log_one_byte_over_stops_on_the_log() {
    let data_len = 10u64;
    let log_bytes = 32 + data_len;
    let code = BytecodeBuilder::default()
        .push_number(data_len)
        .push_number(0_u64)
        .append(LOG0)
        .sstore(U256::from(1), U256::from(1))
        .stop()
        .build();
    let db = || funded().account_code(A, code.clone());
    let body = mega_evm::TX_BODY_SIZE;

    let fits = run_at(db(), call(CALLER, A, U256::ZERO, GAS_LIMIT), body + log_bytes + 40);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.result.logs().len(), 1);
    assert_eq!(fits.result.logs()[0].data.data.len(), data_len as usize);
    assert_eq!(fits.usage, LimitUsage { data_size: body + log_bytes + 40, write_records: 1 });

    let over = run_at(db(), call(CALLER, A, U256::ZERO, GAS_LIMIT), body + log_bytes - 1);
    assert_stopped(&over, body + log_bytes - 1, body + log_bytes);
    assert!(over.result.logs().is_empty(), "the log that crossed was dropped");
    assert_eq!(over.usage.data_size, body, "the store after the log never counted");
}

/// A storage write is one 40-byte record. One byte over that stops on the write: the log after
/// it is not in the crossing figure.
#[test]
fn test_a_storage_write_one_byte_over_stops_on_the_write() {
    let code = BytecodeBuilder::default()
        .sstore(U256::ZERO, U256::from(1))
        .push_number(10_u64)
        .push_number(0_u64)
        .append(LOG0)
        .stop()
        .build();
    let db = || funded().account_code(A, code.clone());
    let body = mega_evm::TX_BODY_SIZE;
    let record = 40;

    let fits = run_at(db(), call(CALLER, A, U256::ZERO, GAS_LIMIT), body + record + 32 + 10);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.result.logs().len(), 1);
    assert_eq!(fits.usage, LimitUsage { data_size: body + record + 42, write_records: 1 });

    let over = run_at(db(), call(CALLER, A, U256::ZERO, GAS_LIMIT), body + record - 1);
    assert_stopped(&over, body + record - 1, body + record);
    assert!(over.result.logs().is_empty(), "the log after the write never ran");
    assert_eq!(over.usage, LimitUsage { data_size: body, write_records: 0 });
}

/// A value transfer's recipient is one 40-byte record. One byte over that stops on the record,
/// before the recipient's code runs.
#[test]
fn test_an_account_write_one_byte_over_stops_on_the_recipient() {
    let db = || {
        funded().account_code(
            B,
            BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build(),
        )
    };
    let body = mega_evm::TX_BODY_SIZE;
    let record = 40;

    let fits = run_at(db(), call(CALLER, B, U256::from(1), GAS_LIMIT), body + record + record);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.state[&B].info.balance, U256::from(1));
    assert_eq!(fits.usage, LimitUsage { data_size: body + record + record, write_records: 2 });

    let over = run_at(db(), call(CALLER, B, U256::from(1), GAS_LIMIT), body + record - 1);
    assert_stopped(&over, body + record - 1, body + record);
    assert_eq!(over.usage, LimitUsage { data_size: body, write_records: 0 });
    assert_eq!(
        over.state.get(&B).map(|account| account.info.balance).unwrap_or_default(),
        U256::ZERO
    );
    assert!(over.state.get(&B).is_none_or(|account| {
        account.storage.get(&U256::ZERO).is_none_or(|slot| !slot.is_changed())
    }));
}

/// What one gas limit did with a transaction whose data size approaches the limit.
enum Bound {
    /// The gas limit did not cover the intrinsic cost, so the transaction was not included.
    Rejected,
    /// The transaction ran out of gas before the write that approaches the limit was counted.
    OutOfGas,
    /// The write was counted, and the data-size stop is what the transaction reports.
    DataSize,
    /// The write was counted inside the data-size limit and the transaction succeeded.
    Success,
}

/// `A`'s code for the store sweep: one fresh slot.
fn fresh_slot() -> Bytes {
    BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build()
}

/// `A`'s code for the frame-start sweep: a value `CALL` of one wei to [`FRESH`], which writes
/// two accounts, `A`'s and [`FRESH`]'s.
fn value_call_to_fresh() -> Bytes {
    BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(1_u8)
        .push_address(FRESH)
        .append(GAS)
        .append(CALL)
        .append(POP)
        .stop()
        .build()
}

/// A call from `CALLER` to `A` running `code`, at `gas_limit`, under a transaction data-size limit
/// of `data_limit`.
fn bound_at(code: &Bytes, gas_limit: u64, data_limit: u64) -> Bound {
    let db = funded().account_code(A, code.clone()).account_balance(A, U256::from(1));
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(data_limit);
    match MegaEvm::new(context(db).with_tx_runtime_limits(limits)).execute_transaction(call(
        CALLER,
        A,
        U256::ZERO,
        gas_limit,
    )) {
        Err(_) => Bound::Rejected,
        Ok(outcome) => match &outcome.result {
            ExecutionResult::Halt { reason, .. } => {
                assert!(
                    matches!(reason, MegaHaltReason::Base(HaltReason::OutOfGas(_))),
                    "a halt below the write is out of gas, got {reason:?}"
                );
                assert_eq!(outcome.limit_exceeded, None, "an out-of-gas is not a data-size stop");
                Bound::OutOfGas
            }
            ExecutionResult::Revert { .. } => {
                assert!(outcome.limit_exceeded.is_some(), "{:?}", outcome.result);
                Bound::DataSize
            }
            ExecutionResult::Success { .. } => {
                assert_eq!(outcome.limit_exceeded, None);
                Bound::Success
            }
        },
    }
}

/// The smallest gas limit at which `code`'s write is counted rather than run out of gas.
fn gas_where_the_write_is_counted(code: &Bytes, data_limit: u64) -> u64 {
    smallest_gas_limit(1_000_000, |gas| {
        !matches!(bound_at(code, gas, data_limit), Bound::Rejected | Bound::OutOfGas)
    })
}

/// The smallest gas limit at which a top-level fresh `SSTORE` is counted at the spec's byte
/// prices.
///
/// It is what the transaction pays up to and including the store, and nothing after it:
///
/// | Part | Gas |
/// |---|---:|
/// | the call's intrinsic regular gas | 15,000 |
/// | the body's history: 310 bytes at 88 | 27,280 |
/// | two `PUSH32` | 6 |
/// | the `SSTORE`'s regular gas: 100 static, 2,100 cold, 19,900 set | 22,100 |
/// | the `SSTORE`'s state gas: a slot's 64 bytes at 1,530 | 97,920 |
/// | **total** | **162,306** |
///
/// Below it the transaction runs out of gas before the store completes, whatever the data-size
/// limit. The record's history — 40 bytes at 88, 3,520 — is not part of it: it is charged after
/// the record is counted, and only for a record the limit keeps.
const FRESH_SLOT_COUNTED_AT: u64 = 162_306;

/// A write meets two limits, and the one that binds first is the one reported.
///
/// The opcode has to finish before its record exists: short of that it is an out-of-gas, and the
/// record is not counted. Once it finishes the record is counted, and a data-size limit it
/// crosses stops the transaction; the record's history is not charged, because the record is not
/// kept. A limit the record fits costs that history on top, and the write succeeds only once the
/// history is paid. Short of it, the same write halts out of gas. So a crossing limit moves the
/// out-of-gas boundary down by exactly the record's history, at a storage write, a log and a
/// `SELFDESTRUCT` alike.
#[test]
fn test_whichever_of_gas_and_data_size_binds_first_is_reported() {
    let selfdestruct = BytecodeBuilder::default().push_address(B).append(SELFDESTRUCT).build();
    let sites = [
        ("a fresh slot", fresh_slot(), WRITE_RECORD_SIZE),
        ("a log of one byte", log0(1), mega_evm::LOG_BASE_SIZE + 1),
        ("a SELFDESTRUCT that moves value", selfdestruct, WRITE_RECORD_SIZE),
    ];
    for (name, code, bytes) in sites {
        // One byte under the record, the record itself crosses.
        let crosses = mega_evm::TX_BODY_SIZE + bytes - 1;
        let fits = mega_evm::TX_BODY_SIZE + bytes;
        let counted_at = gas_where_the_write_is_counted(&code, crosses);
        let kept_at = gas_where_the_write_is_counted(&code, fits);
        let history = mega_evm::history_gas(bytes).expect("the record has a price");

        assert_eq!(kept_at - counted_at, history, "{name}: a kept record costs its history");
        assert!(matches!(bound_at(&code, counted_at - 1, crosses), Bound::OutOfGas), "{name}");
        assert!(matches!(bound_at(&code, counted_at, crosses), Bound::DataSize), "{name}");
        assert!(matches!(bound_at(&code, kept_at, crosses), Bound::DataSize), "{name}");
        assert!(matches!(bound_at(&code, counted_at, fits), Bound::OutOfGas), "{name}");
        assert!(matches!(bound_at(&code, kept_at - 1, fits), Bound::OutOfGas), "{name}");
        assert!(matches!(bound_at(&code, kept_at, fits), Bound::Success), "{name}");
    }
}

/// The fresh slot's figure, [`FRESH_SLOT_COUNTED_AT`], taken apart: what the call pays before its
/// code runs — the gas an empty call spends — then the two pushes and the store's regular and
/// state gas, all read from the schedule in force. The record's history comes on top only when
/// the record is kept.
#[test]
fn test_the_fresh_slot_boundary_is_the_sum_of_its_parts() {
    let code = fresh_slot();
    let counted_at =
        gas_where_the_write_is_counted(&code, mega_evm::TX_BODY_SIZE + WRITE_RECORD_SIZE - 1);
    let kept_at = gas_where_the_write_is_counted(&code, mega_evm::TX_BODY_SIZE + WRITE_RECORD_SIZE);
    let record_history = mega_evm::write_record_history_gas(1).expect("one record has a price");

    let empty_call = MegaEvm::new(context(funded().account_code(A, Bytes::from_static(&[STOP]))))
        .execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT))
        .unwrap();
    assert!(empty_call.result.is_success());
    let params = mega_evm::satin_gas_params();
    let pushes = 2 * 3;
    let store_regular = params.get(GasId::sstore_static()) +
        params.get(GasId::cold_storage_cost()) +
        params.get(GasId::sstore_set_without_load_cost());
    let store_state = params.get(GasId::sstore_set_state_gas());
    assert_eq!(counted_at, empty_call.gas.gas_used + pushes + store_regular + store_state);
    assert_eq!(kept_at, counted_at + record_history);

    if crate::common::runs_at_measurement_prices() {
        return;
    }
    assert_eq!(counted_at, FRESH_SLOT_COUNTED_AT);
    assert_eq!(empty_call.gas.gas_used, 15_000 + 310 * 88);
    assert_eq!((store_regular, store_state), (22_100, 97_920));
    assert_eq!(store_state, mega_evm::constants::SLOT_STATE_GAS);
    assert_eq!(record_history, WRITE_RECORD_SIZE * mega_evm::constants::COST_PER_HISTORY_BYTE);
}

/// At a frame start the order is the other way round, and so is the rule's outcome: the caller
/// pays for the records the frame's start makes at its opcode, before the frame is started and
/// its records are counted. So the data-size limit does not move the out-of-gas boundary of a
/// value `CALL`: the smallest gas limit that is not an out-of-gas is the same whether the two
/// records cross the limit or fit it, and one gas below it is an out-of-gas under either.
#[test]
fn test_a_frame_start_is_charged_before_its_records_are_counted() {
    let code = value_call_to_fresh();
    // `A`'s account and `FRESH`'s: 80 bytes on top of the body.
    let records = 2 * WRITE_RECORD_SIZE;
    let crosses = mega_evm::TX_BODY_SIZE + records - 1;
    let fits = mega_evm::TX_BODY_SIZE + records;
    let charged_at = gas_where_the_write_is_counted(&code, crosses);

    assert_eq!(gas_where_the_write_is_counted(&code, fits), charged_at);
    assert!(matches!(bound_at(&code, charged_at - 1, crosses), Bound::OutOfGas));
    assert!(matches!(bound_at(&code, charged_at, crosses), Bound::DataSize));
    assert!(matches!(bound_at(&code, charged_at - 1, fits), Bound::OutOfGas));
    assert!(matches!(bound_at(&code, charged_at, fits), Bound::Success));
}

/* ---------- a body over the limit, at the smallest gas limit validation accepts ---------- */

/// A fresh account nobody has touched.
const FRESH: Address = address!("0000000000000000000000000000000000300005");

/// The account an EIP-7702 authorization of [`authorizing_call`] delegates.
const AUTHORITY: Address = address!("0000000000000000000000000000000000300006");

/// A type-4 call from `CALLER` to `A`, carrying one authorization of [`AUTHORITY`] to `B`.
fn authorizing_call(gas_limit: u64) -> mega_evm::MegaTransaction {
    use revm::{
        context::{transaction::TransactionType, TxEnv},
        context_interface::{
            either::Either,
            transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
        },
    };
    let authorization = Either::Right(RecoveredAuthorization::new_unchecked(
        Authorization { chain_id: U256::ZERO, address: B, nonce: 0 },
        RecoveredAuthority::Valid(AUTHORITY),
    ));
    alloy_op_evm::OpTx(mega_evm::test_utils::op_transaction(TxEnv {
        tx_type: TransactionType::Eip7702 as u8,
        caller: CALLER,
        kind: alloy_primitives::TxKind::Call(A),
        gas_limit,
        gas_priority_fee: Some(0),
        authorization_list: vec![authorization],
        ..Default::default()
    }))
}

/// Runs `tx` over [`funded`] under a transaction data-size limit of `limit`; `None` when
/// validation rejects it.
fn outcome_at(tx: mega_evm::MegaTransaction, limit: u64) -> Option<MegaTransactionOutcome> {
    MegaEvm::new(
        context(funded())
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit)),
    )
    .execute_transaction(tx)
    .ok()
}

/// The smallest gas limit at which `accepted` holds, knowing it holds at `high` and at every
/// limit above the smallest.
fn smallest_gas_limit(high: u64, accepted: impl Fn(u64) -> bool) -> u64 {
    assert!(accepted(high), "the high bound must be accepted");
    let (mut low, mut high) = (0_u64, high);
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        if accepted(middle) {
            high = middle;
        } else {
            low = middle;
        }
    }
    high
}

/// A body over the limit is the stop at every gas limit validation accepts.
///
/// A gas limit that covers the intrinsic cost and no more cannot pay what the transaction's start
/// costs past it: the account a value transfer or a creation adds, the write record of it, and an
/// authorization's delegation. Without a limit, each of these transactions runs out of gas there,
/// before its first frame. With the body over the limit none of those writes is made — the first
/// frame is answered with the stop — so none is charged, and the transaction reports the stop that
/// bound first. It pays its intrinsic cost and nothing else.
#[test]
fn test_a_body_over_the_limit_is_the_stop_at_the_smallest_valid_gas_limit() {
    type Tx = fn(u64) -> mega_evm::MegaTransaction;
    let cases: [(&str, Tx, u64); 3] = [
        ("a value transfer to a fresh account", |gas| call(CALLER, FRESH, U256::from(1), gas), 0),
        ("a creation", |gas| create(CALLER, constructor_returning(1), gas), 12),
        ("an authorization", authorizing_call, mega_evm::AUTHORIZATION_SIZE),
    ];
    assert_eq!(constructor_returning(1).len(), 12);
    for (name, tx, extra) in cases {
        let body = mega_evm::TX_BODY_SIZE + extra;
        let limit = body - 1;
        let valid = smallest_gas_limit(GAS_LIMIT, |gas| outcome_at(tx(gas), u64::MAX).is_some());
        assert_eq!(
            smallest_gas_limit(GAS_LIMIT, |gas| outcome_at(tx(gas), limit).is_some()),
            valid,
            "{name}: the limit does not move validation"
        );

        let unlimited = outcome_at(tx(valid), u64::MAX).unwrap();
        assert!(
            matches!(
                unlimited.result,
                ExecutionResult::Halt { reason: MegaHaltReason::Base(HaltReason::OutOfGas(_)), .. }
            ),
            "{name}: without a limit the start runs out of gas, got {:?}",
            unlimited.result
        );

        let stopped = outcome_at(tx(valid), limit).unwrap();
        assert_stopped(&stopped, limit, body);
        assert_eq!(stopped.usage, LimitUsage { data_size: body, write_records: 0 }, "{name}");
        assert_eq!(stopped.result.gas().tx_gas_used(), valid, "{name}: the intrinsic cost only");

        // A gas limit above the smallest one pays the same: nothing past the intrinsic cost is
        // charged, so nothing is burnt either.
        let above = outcome_at(tx(valid + 10_000), limit).unwrap();
        assert_stopped(&above, limit, body);
        assert_eq!(above.result.gas().tx_gas_used(), valid, "{name}: the stop burns nothing");
    }
}

/// A creation transaction stopped at the smallest gas limit validation accepts still bumps its
/// sender's nonce, and an authorization its body carried is not applied.
#[test]
fn test_a_stopped_start_bumps_a_creators_nonce_and_applies_no_authorization() {
    let init = constructor_returning(1);
    let limit = mega_evm::TX_BODY_SIZE + init.len() as u64 - 1;
    let valid = smallest_gas_limit(GAS_LIMIT, |gas| {
        outcome_at(create(CALLER, init.clone(), gas), limit).is_some()
    });
    let stopped = outcome_at(create(CALLER, init, valid), limit).unwrap();
    assert!(stopped.limit_exceeded.is_some(), "{:?}", stopped.result);
    assert_eq!(stopped.state[&CALLER].info.nonce, 1, "the sender's nonce is bumped");
    assert!(stopped
        .state
        .get(&CALLER.create(0))
        .is_none_or(|account| account.info.is_empty_code_hash() && account.info.nonce == 0));

    let limit = mega_evm::TX_BODY_SIZE + mega_evm::AUTHORIZATION_SIZE - 1;
    let valid =
        smallest_gas_limit(GAS_LIMIT, |gas| outcome_at(authorizing_call(gas), limit).is_some());
    let stopped = outcome_at(authorizing_call(valid), limit).unwrap();
    assert!(stopped.limit_exceeded.is_some(), "{:?}", stopped.result);
    assert!(
        stopped
            .state
            .get(&AUTHORITY)
            .is_none_or(|a| a.info.nonce == 0 && a.info.is_empty_code_hash()),
        "the delegation was not applied"
    );
}

/* ---------- the 98% share, measured in slots ---------- */

/// Enough gas for a hundred fresh slots and a call or two around them.
const SLOTS_GAS_LIMIT: u64 = 100_000_000;

/// Appends writes of the fresh slots `0..n`.
fn write_slots(mut builder: BytecodeBuilder, n: u64) -> BytecodeBuilder {
    for slot in 0..n {
        builder = builder.sstore(U256::from(slot), U256::from(slot + 1));
    }
    builder
}

/// Appends a valueless `CALL` to `target` with all the gas, dropping its success flag.
fn then_call(builder: BytecodeBuilder, target: Address) -> BytecodeBuilder {
    builder
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(target)
        .append(GAS)
        .append(CALL)
        .append(POP)
}

/// A transaction limit that leaves the transaction's own frame room for `slots` fresh slots.
const fn room_for(slots: u64) -> u64 {
    mega_evm::TX_BODY_SIZE + slots * WRITE_RECORD_SIZE
}

/// Runs a call from `CALLER` to `A` over `db` under a transaction limit of `limit`.
fn run_slots(db: MemoryDatabase, limit: u64) -> MegaTransactionOutcome {
    MegaEvm::new(
        context(db)
            .with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit)),
    )
    .execute_transaction(call(CALLER, A, U256::ZERO, SLOTS_GAS_LIMIT))
    .unwrap()
}

/// With room for 100 slots, a child may keep 98 and a grandchild 96; a parent that kept 20 leaves
/// its child 78. A frame that crosses its share reverts alone: the transaction succeeds with what
/// the other frames kept, and a sibling started after it gets its full share.
#[test]
fn test_a_child_gets_98_percent_of_what_its_parent_has_left_in_slots() {
    assert_eq!(share(100 * WRITE_RECORD_SIZE), 98 * WRITE_RECORD_SIZE);
    assert_eq!(share(98 * WRITE_RECORD_SIZE) / WRITE_RECORD_SIZE, 96);
    assert_eq!(share(80 * WRITE_RECORD_SIZE) / WRITE_RECORD_SIZE, 78);
    let stop = || BytecodeBuilder::default().stop().build();
    let slots = |n| write_slots(BytecodeBuilder::default(), n).stop().build();
    let calls = |target| then_call(BytecodeBuilder::default(), target).stop().build();
    let cases: [(&str, Bytes, Bytes, Bytes, u64); 8] = [
        ("a child that fills its share", calls(B), slots(98), stop(), 98),
        ("a child one slot over its share", calls(B), slots(99), stop(), 0),
        (
            "a child that fills what a parent of 20 slots left",
            then_call(write_slots(BytecodeBuilder::default(), 20), B).stop().build(),
            slots(78),
            stop(),
            20 + 78,
        ),
        (
            "a child one slot over what a parent of 20 slots left",
            then_call(write_slots(BytecodeBuilder::default(), 20), B).stop().build(),
            slots(79),
            stop(),
            20,
        ),
        ("a grandchild that fills its share", calls(B), calls(C), slots(96), 96),
        ("a grandchild one slot over its share", calls(B), calls(C), slots(97), 0),
        (
            "a sibling after a child that crossed its share",
            then_call(then_call(BytecodeBuilder::default(), B), C).stop().build(),
            slots(99),
            slots(98),
            98,
        ),
        (
            "a parent that writes after its child crossed",
            then_call(BytecodeBuilder::default(), B)
                .sstore(U256::ZERO, U256::from(42))
                .stop()
                .build(),
            slots(99),
            stop(),
            1,
        ),
    ];
    for (name, a, b, c, kept) in cases {
        let db = funded().account_code(A, a).account_code(B, b).account_code(C, c);
        let outcome = run_slots(db, room_for(100));
        assert!(outcome.result.is_success(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{name}: a frame budget does not latch");
        assert_eq!(outcome.usage, LimitUsage { data_size: room_for(kept), write_records: kept });
        if name == "a parent that writes after its child crossed" {
            let written = |address: Address| {
                outcome.state.get(&address).is_some_and(|account| {
                    account.storage.get(&U256::ZERO).is_some_and(|slot| slot.is_changed())
                })
            };
            assert!(written(A), "the parent's write is kept");
            assert!(!written(B), "the child's writes went with its revert");
        }
    }
}

/// The child that crosses its share reverts with `MegaLimitExceeded` naming the data size and
/// the share it crossed, which is what its caller reads.
#[test]
fn test_a_child_that_crosses_its_share_reverts_with_the_share() {
    let returns_revert_data = then_call(BytecodeBuilder::default(), B)
        .append(RETURNDATASIZE)
        .append_many([PUSH0, PUSH0])
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .append(PUSH0)
        .append(RETURN)
        .build();
    let db = funded()
        .account_code(A, returns_revert_data)
        .account_code(B, write_slots(BytecodeBuilder::default(), 99).stop().build());
    let outcome = run_slots(db, room_for(100));
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    let reverted = MegaLimitExceeded::abi_decode(outcome.result.output().unwrap()).unwrap();
    assert_eq!(reverted.kind, LimitKind::DataSize.as_u8());
    assert_eq!(reverted.limit, 98 * WRITE_RECORD_SIZE);
}

/// The transaction's own frame gets what the body leaves: with room for 5 slots, 5 are kept and a
/// sixth stops the transaction. Its frame's budget is the transaction's own limit, so the
/// crossing is the transaction's, not a frame-local revert.
#[test]
fn test_the_first_frame_gets_what_the_body_leaves() {
    let run = |n| {
        run_slots(
            funded().account_code(A, write_slots(BytecodeBuilder::default(), n).stop().build()),
            room_for(5),
        )
    };
    let fits = run(5);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.usage.data_size, room_for(5));
    assert_stopped(&run(6), room_for(5), room_for(6));
}

/// A body and the execution after it add up: a limit the body fits stops the transaction at the
/// write that crosses it, calldata or no calldata.
#[test]
fn test_the_body_and_the_execution_add_up_to_the_stop() {
    for calldata in [0_u64, 200] {
        let limit = mega_evm::TX_BODY_SIZE + calldata + WRITE_RECORD_SIZE;
        let db =
            funded().account_code(A, write_slots(BytecodeBuilder::default(), 3).stop().build());
        let outcome = run_at(
            db,
            call_with_data(CALLER, A, Bytes::from(vec![0xab; calldata as usize]), SLOTS_GAS_LIMIT),
            limit,
        );
        assert_stopped(&outcome, limit, limit + WRITE_RECORD_SIZE);
    }
}

/// A body over the limit is stopped before an interceptor could answer the transaction's own
/// call: the stop is the first frame's answer, whatever the frame's target.
#[test]
fn test_a_body_over_the_limit_is_stopped_before_an_interceptor() {
    use alloy_sol_types::SolCall;
    use mega_evm::system::{IMegaLimitControl, LIMIT_CONTROL_ADDRESS};
    let selector = IMegaLimitControl::remainingComputeGasCall::SELECTOR;
    let body = mega_evm::TX_BODY_SIZE + selector.len() as u64;
    let tx =
        || call_with_data(CALLER, LIMIT_CONTROL_ADDRESS, Bytes::from(selector.to_vec()), GAS_LIMIT);
    let answered = run_at(funded(), tx(), body);
    assert!(answered.result.is_success(), "the interceptor answers: {:?}", answered.result);
    let stopped = run_at(funded(), tx(), body - 1);
    assert_stopped(&stopped, body - 1, body);
    assert_eq!(
        stopped.result.output().unwrap(),
        &LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: body - 1,
            used: body,
            frame_local: false,
        }
        .revert_data(),
    );
}
