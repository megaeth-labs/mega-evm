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
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm, MegaLimitExceeded,
    MegaTransactionOutcome, FRAME_DATA_SHARE_DENOMINATOR, FRAME_DATA_SHARE_NUMERATOR,
    WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{CALL, CREATE, GAS, LOG0, POP, PUSH0, RETURN, REVERT, STOP},
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
