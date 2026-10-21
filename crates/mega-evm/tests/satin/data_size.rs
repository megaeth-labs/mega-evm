//! The data-size limit: the 98% a child frame receives, the transaction body, and deployed code.
//!
//! A child frame's budget is 98% of what its parent has left. Crossing that budget, or the
//! transaction's own limit, reverts the frame that crossed it alone, and its parent resumes with
//! what the frame counted taken back.
//!
//! The body is counted before any frame and kept on every path; validation rejects a transaction
//! whose body, with a record for each authorization it carries, exceeds the limit. Deployed code is
//! counted on the creation's lane, one byte per byte, before the creation is committed, and only
//! when revm would deposit it.

use alloy_evm::Evm;
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy_sol_types::SolError;
use mega_evm::{
    constants::{MAX_CONTRACT_SIZE, TX_GAS_LIMIT_CAP},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm, MegaHaltReason,
    MegaLimitExceeded, MegaTransactionOutcome, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, TRANSFER_LOG_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        ADD, CALL, CREATE, CREATE2, GAS, ISZERO, LOG0, MSTORE, POP, PUSH0, RETURN, RETURNDATACOPY,
        RETURNDATASIZE, SELFDESTRUCT, SSTORE, STOP,
    },
    context::result::{ExecutionResult, HaltReason},
    context_interface::cfg::GasId,
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
        InstructionResult, Interpreter,
    },
    Database, Inspector,
};

use crate::common::{
    account_state_gas, body_history, call, call_with_data, constructor_returning, context, create,
    execute_under, history, history_is_free, history_rounds, share, slot_state_gas,
};

const CALLER: Address = address!("0000000000000000000000000000000000300000");
const A: Address = address!("0000000000000000000000000000000000300001");
const B: Address = address!("0000000000000000000000000000000000300002");
const C: Address = address!("0000000000000000000000000000000000300003");
const D: Address = address!("0000000000000000000000000000000000300004");

const GAS_LIMIT: u64 = 20_000_000;

/// [`GAS_LIMIT`] on top of twice the history of `bytes` at the byte prices in effect: enough for
/// the frame three calls down to keep `bytes`, after each caller kept a 64th of what it forwards.
fn gas_for_history(bytes: u64) -> u64 {
    GAS_LIMIT + 2 * history(bytes)
}

/// The least a parent must have left for the child's share of it to hold `bytes`: a limit that
/// fits a child's write by exactly its bytes still reverts the child on its own budget, which is
/// 98% of the parent's room, rounded down.
fn child_room(bytes: u64) -> u64 {
    let room = (bytes * FRAME_DATA_SHARE_DENOMINATOR).div_ceil(FRAME_DATA_SHARE_NUMERATOR);
    assert!(share(room) >= bytes && share(room - 1) < bytes, "the least room for {bytes} bytes");
    room
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
/// `D` alone: the three callers still write the slot after the call, and the transaction reports
/// no stop.
#[test]
fn test_depth_3_frame_budget_reverts_the_child_and_the_parents_resume() {
    const CAP: u64 = 20_000;
    let budget = budget_at_depth_3(CAP);
    let limits = EvmTxRuntimeLimits::no_limits().with_frame_data_size_limit(CAP);
    let run = |data_len: u64| {
        let db = chain(log0(data_len));
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
        let gas_limit = gas_for_history(budget);
        let result = evm.transact_raw(call(CALLER, A, U256::ZERO, gas_limit)).unwrap();
        (result, evm.ctx().additional_limit().limit_exceeded().copied(), evm.ctx().limit_usage())
    };

    // 32 bytes for the log's address, and the data. Exactly the budget.
    let (fits, stop, usage) = run(budget - 32);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(stop, None);
    assert!(stored(&fits.state, A) && stored(&fits.state, B) && stored(&fits.state, C));
    assert_eq!(fits.result.logs().len(), 1);
    assert_eq!(usage.write_records, 3, "one slot in each caller");
    assert_eq!(usage.data_size, mega_evm::TX_BODY_SIZE + budget + 3 * 40);

    let (over, stop, usage) = run(budget - 31);
    assert!(over.result.is_success(), "the callers resume: {:?}", over.result);
    assert_eq!(stop, None, "a frame budget below the transaction's own frame is no stop of it");
    assert!(stored(&over.state, A) && stored(&over.state, B) && stored(&over.state, C));
    assert!(over.result.logs().is_empty(), "the log died with D");
    assert_eq!(
        usage,
        mega_evm::LimitUsage { data_size: mega_evm::TX_BODY_SIZE + 3 * 40, write_records: 3 }
    );
}

/// The same depth, with the transaction's limit tighter than the frame budgets. `D`'s log crosses
/// the transaction limit, which reverts `D` alone, with the revert data naming the transaction
/// limit: `C`, `B` and `A` resume, each writes its slot within what the transaction has left, and
/// the transaction succeeds. What it keeps is what the same chain keeps without the log: the log
/// went with `D`, and the transaction never kept more than its limit.
#[test]
fn test_depth_3_transaction_limit_reverts_the_child_and_the_parents_resume() {
    const TX_LIMIT: u64 = 8_000;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(TX_LIMIT);
    let mut evm = MegaEvm::new(context(chain(log0(TX_LIMIT))).with_tx_runtime_limits(limits))
        .with_inspector(Probe::default());
    let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();

    assert!(outcome.result.is_success(), "the callers resume: {:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None, "a stop below the transaction's own frame is no stop");
    // `D` reverted with the transaction limit's stop: nothing was written before its log, so the
    // log crossed it alone, and the transaction limit is checked before `D`'s budget.
    let stop = MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: TX_LIMIT };
    assert_eq!(
        evm.inspector().calls.first(),
        Some(&(D, InstructionResult::Revert, Bytes::from(stop.abi_encode()))),
        "D returned the stop"
    );
    assert!(stored(&outcome.state, A) && stored(&outcome.state, B) && stored(&outcome.state, C));
    assert!(outcome.result.logs().is_empty(), "the log went with D");
    let three_records = mega_evm::TX_BODY_SIZE + 3 * WRITE_RECORD_SIZE;
    assert_eq!(outcome.usage, LimitUsage { data_size: three_records, write_records: 3 });
    assert!(outcome.usage.data_size <= TX_LIMIT);

    // The same chain without the log keeps exactly as much.
    let kept = MegaEvm::new(
        context(chain(BytecodeBuilder::default().append(STOP).build()))
            .with_tx_runtime_limits(limits),
    )
    .execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT))
    .unwrap();
    assert!(kept.result.is_success(), "{:?}", kept.result);
    assert_eq!(kept.limit_exceeded, None);
    assert_eq!(kept.usage, outcome.usage);
    assert!(stored(&kept.state, A) && stored(&kept.state, B) && stored(&kept.state, C));
}

/// Counts steps, so a frame answered without running is visible, and the result of each creation
/// and each call, the deepest first.
#[derive(Clone, Default)]
struct Probe {
    steps: u32,
    creates: Vec<(InstructionResult, Bytes)>,
    calls: Vec<(Address, InstructionResult, Bytes)>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Probe {
    fn step(&mut self, _: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        self.steps += 1;
    }

    fn call_end(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let result = &outcome.result;
        self.calls.push((inputs.target_address, result.result, result.output.clone()));
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

/// Asserts `error` is the refusal of a body whose bytes and authorization records exceed the
/// transaction's data-size limit.
fn assert_body_refused(error: &str) {
    assert!(error.contains(mega_evm::TX_BODY_OVER_DATA_SIZE_LIMIT), "{error}");
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
    // The transaction's frame is capped at `CAP` and has spent nothing but its nonce record,
    // counted on it as the creation starts, so the creation gets 98% of what the record leaves.
    // Its start records the created account before the code.
    let child_budget = share(CAP - WRITE_RECORD_SIZE);
    let at_start = WRITE_RECORD_SIZE;
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
            evm.ctx().limit_usage(),
            evm.ctx().additional_limit().limit_exceeded().copied(),
            evm.inspector().clone(),
        )
    };

    for inspect in [false, true] {
        let (kept, usage, stop, _) = run(fits, inspect);
        assert!(kept.result.is_success(), "inspect {inspect}: {:?}", kept.result);
        assert_eq!(stop, None);
        assert_eq!(
            usage,
            LimitUsage {
                data_size: mega_evm::TX_BODY_SIZE + WRITE_RECORD_SIZE + child_budget,
                write_records: 2,
            },
            "the creator's nonce record, and the creation's whole budget"
        );
        assert_eq!(kept.state[&created].info.code_hash, keccak256(vec![0_u8; fits as usize]));

        let (over, usage, stop, probe) = run(fits + 1, inspect);
        assert!(over.result.is_success(), "the caller resumes: {:?}", over.result);
        assert_eq!(stop, None, "a frame budget below the transaction's own frame is no stop of it");
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

/// A contract that runs `init` through `CREATE` with `value` wei, then returns what the creation
/// returned or reverted with: nothing once it succeeded, the stop's revert data once a limit
/// stopped it.
fn create_returning_its_output(init: &Bytes, value: u64) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, init)
        .push_number(init.len() as u64)
        .push_number(0_u64)
        .push_number(value)
        .append_many([CREATE, POP, RETURNDATASIZE, PUSH0, PUSH0, RETURNDATACOPY])
        .append_many([RETURNDATASIZE, PUSH0, RETURN])
        .build()
}

/// A creation's start counts against the creation's own budget — the share of what its creator
/// has left once the creator's own nonce record is counted on the creator — the created account's
/// record, and with an endowment the transfer log. The creator here is the transaction's own
/// frame, with `R` bytes left under the transaction's limit, of which its nonce record takes 40.
///
/// The largest `R` whose remainder's share falls short of the start stops the creation at its
/// start, which keeps its creator's nonce record: 80 without an endowment, a share of 39 against
/// 40 bytes. One byte more, a share of 40, starts it. With 100 left the share of the 60 left is
/// 58, so the creation may deposit the 18 bytes beyond its start, and the 19th stops it at its
/// deposit; with an endowment, 260 left leave room for 15. A stopped creation keeps nothing of
/// its own, and its creator resumes with the nonce record alone.
///
/// Before the creator's record moved to the creating frame the creation's budget was the share of
/// the whole `R`, against a start of 80 bytes: 81 stopped it and 82 started it, 100 left the same
/// 18 bytes, and 260 with an endowment left 14.
#[test]
fn test_a_creation_is_held_to_its_share_from_its_start() {
    let run = |left: u64, endowment: u64, deployed: u64| {
        let db = funded()
            .account_code(
                A,
                create_returning_its_output(&constructor_returning(deployed), endowment),
            )
            .account_balance(A, U256::from(endowment));
        run_at(db, call(CALLER, A, U256::ZERO, GAS_LIMIT), mega_evm::TX_BODY_SIZE + left)
    };
    let stop = |budget| MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: budget };
    let usage = |bytes, write_records| LimitUsage {
        data_size: mega_evm::TX_BODY_SIZE + bytes,
        write_records,
    };
    let nonce_record_alone = usage(WRITE_RECORD_SIZE, 1);
    let assert_stopped = |outcome: &MegaTransactionOutcome, budget: u64, case: &str| {
        assert!(outcome.result.is_success(), "{case}: A resumes: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{case}: a stop below the transaction's frame");
        assert_eq!(
            outcome.result.output().unwrap().as_ref(),
            stop(budget).abi_encode(),
            "{case}: the creation was stopped on its own budget",
        );
        assert_eq!(outcome.usage, nonce_record_alone, "{case}: A keeps its nonce record");
        assert_eq!(outcome.state[&A].info.nonce, 1, "{case}");
        assert!(
            outcome.state.get(&A.create(0)).is_none_or(|account| account.info.is_empty()),
            "{case}: nothing was created",
        );
    };

    // The creation's budget: the share of what the creator has left after its nonce record.
    let budget = |left: u64| share(left - WRITE_RECORD_SIZE);
    for endowment in [0, 1] {
        let log = if endowment == 0 { 0 } else { TRANSFER_LOG_SIZE };
        let at_start = WRITE_RECORD_SIZE + log;
        let starts = WRITE_RECORD_SIZE + child_room(at_start);

        let case = format!("endowment {endowment}, {} left", starts - 1);
        assert_stopped(&run(starts - 1, endowment, 0), budget(starts - 1), &case);

        let case = format!("endowment {endowment}, {starts} left");
        let started = run(starts, endowment, 0);
        assert!(started.result.is_success(), "{case}: {:?}", started.result);
        assert_eq!(started.result.output().unwrap(), &Bytes::new(), "{case}: it started");
        assert_eq!(
            started.usage,
            usage(WRITE_RECORD_SIZE + at_start, 2),
            "{case}: both records and the log"
        );

        let left = 100 + log;
        let room = budget(left) - at_start;
        let case = format!("endowment {endowment}, {left} left, {room} bytes deployed");
        let fits = run(left, endowment, room);
        assert!(fits.result.is_success(), "{case}: {:?}", fits.result);
        assert_eq!(fits.result.output().unwrap(), &Bytes::new(), "{case}: it deployed");
        assert_eq!(
            fits.usage,
            usage(WRITE_RECORD_SIZE + budget(left), 2),
            "{case}: the creator's record, and exactly the creation's share"
        );
        assert_eq!(
            fits.state[&A.create(0)].info.code_hash,
            keccak256(vec![0_u8; room as usize]),
            "{case}",
        );

        let case = format!("endowment {endowment}, {left} left, {} bytes deployed", room + 1);
        assert_stopped(&run(left, endowment, room + 1), budget(left), &case);

        if endowment == 0 {
            assert_eq!((starts - 1, budget(starts - 1), starts), (80, 39, 81));
            assert_eq!((budget(left), room), (58, 18));
        } else {
            assert_eq!((starts - 1, budget(starts - 1), starts), (244, 199, 245));
            assert_eq!((budget(left), room), (215, 15));
        }
    }
}

/// Init code that returns `len` bytes as the deployed contract, the first of them `first` and
/// the rest zeros.
fn constructor_returning_from(first: u8, len: u64) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, [first])
        .push_number(len)
        .push_number(0_u8)
        .append(RETURN)
        .build()
}

/// A contract that runs `init` through `CREATE`, then writes whether the creation failed to slot
/// 1, and stops.
fn factory_noting_failure(init: &Bytes) -> Bytes {
    BytecodeBuilder::default()
        .mstore(0, init)
        .push_number(init.len() as u64)
        .push_number(0_u64)
        .push_number(0_u64)
        .append(CREATE)
        .append(ISZERO)
        .push_number(1_u64)
        .append(SSTORE)
        .append(STOP)
        .build()
}

/// Only code `return_create` would deposit is counted. Code it refuses — starting with `0xEF`
/// (EIP-3541), or over the code-size limit — fails the creation alone, as it would with no limit:
/// the caller catches the failure and the transaction succeeds. The same number of bytes it would
/// deposit, with the gas to pay for them, is counted and crosses the transaction's limit, which
/// stops the creation with the limit's revert: the caller catches that failure too, and the code
/// goes with the creation. The reservoir of a gas limit far above the execution cap pays the state
/// gas and the history of a contract of the largest size.
///
/// `A` has 100 bytes left: the creation's start records the created account and `A`'s nonce, and
/// the output comes on top. Afterwards `A` writes a slot to note the failure: the transaction
/// keeps the nonce record and that slot's.
#[test]
fn test_code_return_create_refuses_is_not_counted() {
    let limit = mega_evm::TX_BODY_SIZE + 100;
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit);
    let at_start = 2 * WRITE_RECORD_SIZE;
    let max = MAX_CONTRACT_SIZE as u64;
    let cases = [
        ("0xEF first", 0xEF, 21, InstructionResult::CreateContractStartingWithEF),
        ("over the code-size limit", 0x00, max + 1, InstructionResult::CreateContractSizeLimit),
    ];
    let run = |init: &Bytes, inspect: bool, gas_limit: u64| {
        let db = funded().account_code(A, factory_noting_failure(init));
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .with_inspector(Probe::default());
        Evm::set_inspector_enabled(&mut evm, inspect);
        let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, gas_limit)).unwrap();
        (outcome, evm.inspector().clone())
    };
    let failure_noted = |outcome: &MegaTransactionOutcome| {
        outcome.state[&A].storage.get(&U256::from(1)).map(|slot| slot.present_value)
    };

    for (name, first, len, refusal) in cases {
        for inspect in [false, true] {
            let case = format!("{name}, inspect {inspect}");
            let (refused, probe) = run(&constructor_returning_from(first, len), inspect, GAS_LIMIT);
            assert!(refused.result.is_success(), "{case}: {:?}", refused.result);
            assert_eq!(refused.limit_exceeded, None, "{case}: the refused code is not counted");
            assert_eq!(failure_noted(&refused), Some(U256::from(1)), "{case}: A caught it");
            assert_eq!(
                refused.usage,
                LimitUsage { data_size: mega_evm::TX_BODY_SIZE + at_start, write_records: 2 },
                "{case}: the nonce record and the slot",
            );
            assert_eq!(
                refused.gas.history,
                mega_evm::history_gas(refused.usage.data_size).unwrap(),
                "{case}: history prices the same bytes",
            );
            assert_eq!(refused.state[&A].info.nonce, 1, "{case}");
            if inspect {
                assert_eq!(probe.creates.len(), 1, "{case}");
                assert_eq!(probe.creates[0].0, refusal, "{case}: revm's own refusal");
            }

            // The same length, deployable and paid for: counted, and it crosses.
            let deployable = constructor_returning_from(0x00, len.min(max));
            // The reservoir pays the state gas and the history of the largest contract, and a
            // billion more, at the byte prices in effect.
            let roomy = TX_GAS_LIMIT_CAP +
                max * mega_evm::satin_gas_params().get(GasId::code_deposit_state_gas()) +
                history(max) +
                1_000_000_000;
            let (caught, probe) = run(&deployable, inspect, roomy);
            assert!(caught.result.is_success(), "{case}: {:?}", caught.result);
            assert_eq!(caught.limit_exceeded, None, "{case}: A caught the creation's stop");
            assert_eq!(failure_noted(&caught), Some(U256::from(1)), "{case}");
            assert_eq!(
                caught.usage,
                LimitUsage { data_size: mega_evm::TX_BODY_SIZE + at_start, write_records: 2 },
                "{case}: the code went with the creation",
            );
            if inspect {
                let stop = MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit };
                assert_eq!(
                    probe.creates,
                    [(InstructionResult::Revert, Bytes::from(stop.abi_encode()))],
                    "{case}: the code was counted, and crossed the limit",
                );
            }
        }
    }
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

/// A value transfer's recipient is one 40-byte record, and the move leaves a transfer log; both are
/// counted when the transaction's frame starts. A limit either of them crosses stops the frame
/// there, before the recipient's code runs, and the value goes back with the stop.
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
    let start = record + TRANSFER_LOG_SIZE;

    let fits = run_at(db(), call(CALLER, B, U256::from(1), GAS_LIMIT), body + start + record);
    assert!(fits.result.is_success(), "{:?}", fits.result);
    assert_eq!(fits.state[&B].info.balance, U256::from(1));
    assert_eq!(fits.usage, LimitUsage { data_size: body + start + record, write_records: 2 });

    for limit in [body + record - 1, body + start - 1] {
        let over = run_at(db(), call(CALLER, B, U256::from(1), GAS_LIMIT), limit);
        assert_stopped(&over, limit, body + start);
        assert!(over.result.logs().is_empty(), "limit {limit}: no transfer log");
        assert_eq!(over.usage, LimitUsage { data_size: body, write_records: 0 });
        assert_eq!(
            over.state.get(&B).map(|account| account.info.balance).unwrap_or_default(),
            U256::ZERO
        );
        assert!(over.state.get(&B).is_none_or(|account| {
            account.storage.get(&U256::ZERO).is_none_or(|slot| !slot.is_changed())
        }));
    }
}

/// What one gas limit did with a transaction whose data size approaches the limit.
enum Bound {
    /// The gas limit did not cover the intrinsic cost, so the transaction was not included.
    Rejected,
    /// The transaction ran out of gas before the write that approaches the limit was counted.
    OutOfGas,
    /// The write was counted, and the data-size stop is what the frame that made it returned:
    /// the transaction's result when the frame is the transaction's own, a failed call that
    /// returned the stop to the frame that made it otherwise.
    DataSize,
    /// The write was counted inside the data-size limit and the transaction succeeded.
    Success,
}

/// `A`'s code for the store sweep: one fresh slot.
fn fresh_slot() -> Bytes {
    BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).stop().build()
}

/// `A`'s code for the frame-start sweep: a value `CALL` of one wei to [`FRESH`], which writes
/// two accounts, `A`'s and [`FRESH`]'s, answering as [`answer_the_call`] does.
fn value_call_to_fresh() -> Bytes {
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .push_number(1_u8)
        .push_address(FRESH)
        .append(GAS)
        .append(CALL);
    answer_the_call(code)
}

/// Appends the answer to the call just made: its flag, the size of what it returned and what it
/// returned, `PUSH0; MSTORE; RETURNDATASIZE; PUSH1 32; MSTORE; RETURNDATASIZE; PUSH0; PUSH1 64;
/// RETURNDATACOPY; RETURNDATASIZE; PUSH1 64; ADD; PUSH0; RETURN`, so a reader of the output tells
/// a halt (a flag of zero and no return data) from a stop (the stop's revert data) and from a
/// success.
fn answer_the_call(code: BytecodeBuilder) -> Bytes {
    code.append(PUSH0)
        .append(MSTORE)
        .append(RETURNDATASIZE)
        .push_number(32_u8)
        .append(MSTORE)
        .append_many([RETURNDATASIZE, PUSH0])
        .push_number(64_u8)
        .append_many([RETURNDATACOPY, RETURNDATASIZE])
        .push_number(64_u8)
        .append_many([ADD, PUSH0, RETURN])
        .build()
}

/// What a call [`answer_the_call`] answered for did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// The call succeeded.
    Success,
    /// The call failed with no return data: its frame halted.
    Halt,
    /// The call failed with a limit's stop: its dimension and limit.
    Stop(LimitKind, u64),
}

/// The [`Answer`] `answer` is, as [`answer_the_call`] wrote it: the call's flag, the size of what
/// it returned, and what it returned.
fn read_the_answer(answer: &[u8]) -> Answer {
    let flag = U256::from_be_slice(&answer[..32]);
    let returned = U256::from_be_slice(&answer[32..64]);
    if flag == U256::ONE {
        return Answer::Success;
    }
    assert!(flag.is_zero(), "a CALL's flag");
    assert_eq!(returned, U256::from(answer.len() - 64), "the return data follows");
    if returned.is_zero() {
        return Answer::Halt;
    }
    let (kind, limit) = mega_evm::decode_mega_limit_exceeded(&answer[64..])
        .unwrap_or_else(|| panic!("the call failed with something other than a stop: {answer:?}"));
    Answer::Stop(kind, limit)
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
            ExecutionResult::Success { output, .. } => {
                assert_eq!(outcome.limit_exceeded, None);
                // A frame start the limit stopped: `A` answers the call it returned the stop to;
                // code that answers nothing ran to its success.
                if output.data().is_empty() {
                    return Bound::Success;
                }
                match read_the_answer(output.data()) {
                    Answer::Stop(..) => Bound::DataSize,
                    Answer::Halt => panic!("the call halted: {output:?}"),
                    Answer::Success => Bound::Success,
                }
            }
        },
    }
}

/// A gas limit above what any write here needs to be counted, at the byte prices in effect:
/// 1,000,000 on top of a new account, a fresh slot and the body, and 64 times the history of two
/// records, which a value call that forwards all but a 64th of its gas pays from the 64th it keeps.
fn high_bound() -> u64 {
    1_000_000 +
        account_state_gas() +
        slot_state_gas() +
        body_history(0) +
        64 * history(2 * WRITE_RECORD_SIZE)
}

/// The smallest gas limit at which `code`'s write is counted rather than run out of gas.
fn gas_where_the_write_is_counted(code: &Bytes, data_limit: u64) -> u64 {
    smallest_gas_limit(high_bound(), |gas| {
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
/// | the `SSTORE`'s regular gas: 100 static, 2,000 cold, 10,000 set | 12,100 |
/// | the `SSTORE`'s state gas: a slot's 64 bytes at 1,530 | 97,920 |
/// | **total** | **152,306** |
///
/// Below it the transaction runs out of gas before the store completes, whatever the data-size
/// limit. The record's history — 40 bytes at 88, 3,520 — is not part of it: it is charged after
/// the record is counted, and only for a record the limit keeps.
const FRESH_SLOT_COUNTED_AT: u64 = 152_306;

/// A write meets two limits, and the one that binds first is the one reported.
///
/// The opcode has to finish before its record exists: short of that it is an out-of-gas, and the
/// record is not counted. Once it finishes the record is counted, and a data-size limit it
/// crosses stops the transaction; the record's history is not charged, because the record is not
/// kept. A limit the record fits costs that history on top, and the write succeeds only once the
/// history is paid. Short of it, the same write halts out of gas. So a crossing limit moves the
/// out-of-gas boundary down by exactly the record's history, at a storage write, a log and a
/// `SELFDESTRUCT` alike.
///
/// A `SELFDESTRUCT` that moves value counts its transfer log with its two records — the account
/// it clears and its beneficiary — in one check, so the three cross together; the log is data
/// size and not history, so the boundary moves by the records' history alone.
#[test]
fn test_whichever_of_gas_and_data_size_binds_first_is_reported() {
    let selfdestruct = BytecodeBuilder::default().push_address(B).append(SELFDESTRUCT).build();
    let sites = [
        ("a fresh slot", fresh_slot(), WRITE_RECORD_SIZE, WRITE_RECORD_SIZE),
        ("a log of one byte", log0(1), mega_evm::LOG_BASE_SIZE + 1, mega_evm::LOG_BASE_SIZE + 1),
        (
            "a SELFDESTRUCT that moves value",
            selfdestruct,
            2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE,
            2 * WRITE_RECORD_SIZE,
        ),
    ];
    for (name, code, bytes, history_bytes) in sites {
        // One byte under the record, the record itself crosses.
        let crosses = mega_evm::TX_BODY_SIZE + bytes - 1;
        let fits = mega_evm::TX_BODY_SIZE + bytes;
        let counted_at = gas_where_the_write_is_counted(&code, crosses);
        let kept_at = gas_where_the_write_is_counted(&code, fits);
        let history = mega_evm::history_gas(history_bytes).expect("the record has a price");

        assert_eq!(kept_at - counted_at, history, "{name}: a kept record costs its history");
        assert!(matches!(bound_at(&code, counted_at - 1, crosses), Bound::OutOfGas), "{name}");
        assert!(matches!(bound_at(&code, counted_at, crosses), Bound::DataSize), "{name}");
        assert!(matches!(bound_at(&code, kept_at, crosses), Bound::DataSize), "{name}");
        // A record whose history costs nothing, where a history byte is free, moves no boundary:
        // the write the limit counts is the write that succeeds.
        if history > 0 {
            assert!(matches!(bound_at(&code, counted_at, fits), Bound::OutOfGas), "{name}");
            assert!(matches!(bound_at(&code, kept_at - 1, fits), Bound::OutOfGas), "{name}");
        }
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
    assert_eq!((store_regular, store_state), (12_100, 97_920));
    assert_eq!(store_state, mega_evm::constants::SLOT_STATE_GAS);
    assert_eq!(record_history, WRITE_RECORD_SIZE * mega_evm::constants::COST_PER_HISTORY_BYTE);
}

/// The bytes a value call's start counts on the caller's lane, its own account's record, and on
/// the frame's lane, the recipient's record and the transfer log.
const START_ON_THE_CALLER: u64 = WRITE_RECORD_SIZE;
const START_ON_THE_FRAME: u64 = WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE;

/// A transaction limit the start of [`value_call_to_fresh`] fits: the caller's record, and room
/// for the frame's lane under the 98% share the new frame is given of what the caller has left.
fn start_fits() -> u64 {
    mega_evm::TX_BODY_SIZE + START_ON_THE_CALLER + child_room(START_ON_THE_FRAME)
}

/// At a frame start the order is the other way round, and so is the rule's outcome: the caller
/// pays for the records the frame's start makes at its opcode, before the frame is started and
/// its records are counted. So the data-size limit does not move the out-of-gas boundary of a
/// value `CALL`: the smallest gas limit that is not an out-of-gas is the same whether the two
/// records and the transfer log cross the limit or fit it, and one gas below it is an out-of-gas
/// under either. The log is charged nothing at all. Under the limit they cross, the frame is
/// answered with the stop, which `A` catches; under the limit they fit, the frame starts and the
/// records are kept.
///
/// Rules: [S7.28], [S10.10].
#[test]
fn test_a_frame_start_is_charged_before_its_records_are_counted() {
    let code = value_call_to_fresh();
    // `A`'s account and `FRESH`'s, and the transfer log: 240 bytes on top of the body.
    let start = START_ON_THE_CALLER + START_ON_THE_FRAME;
    let crosses = mega_evm::TX_BODY_SIZE + start - 1;
    let fits = start_fits();
    let charged_at = gas_where_the_write_is_counted(&code, crosses);

    assert_eq!(gas_where_the_write_is_counted(&code, fits), charged_at);
    assert!(matches!(bound_at(&code, charged_at - 1, crosses), Bound::OutOfGas));
    assert!(matches!(bound_at(&code, charged_at, crosses), Bound::DataSize));
    assert!(matches!(bound_at(&code, charged_at - 1, fits), Bound::OutOfGas));
    assert!(matches!(bound_at(&code, charged_at, fits), Bound::Success));
    let started = run_at(
        funded().account_code(A, code).account_balance(A, U256::from(1)),
        call(CALLER, A, U256::ZERO, charged_at),
        fits,
    );
    assert_eq!(
        started.usage,
        LimitUsage { data_size: mega_evm::TX_BODY_SIZE + start, write_records: 2 },
        "the frame started and its records were kept, not refused on the frame's budget",
    );
}

/// The frame-start order, one gas short by hand [S7.28]: a caller that forwards all but a 64th
/// keeps a 64th one gas short of the price of the two records its value call makes, under a
/// limit those records would cross. The caller pays for the records before they are counted, so
/// it runs out of gas first — a halt, no stop, nothing counted beyond the body — and the limit
/// never sees the start. One gas more, the 64th pays the records, they are counted, and the same
/// limit answers the start with the stop, which the caller catches; under a limit they fit, the
/// frame starts.
///
/// The gas limit is the call's intrinsic 15,000, the body's history, the caller's sixteen gas of
/// pushes, the `CALL`'s regular gas — the value transfer, the cold recipient and the new account,
/// 11,300, 3,000 and nothing at the schedule's prices — the recipient's state gas (charged before
/// the split), and what the caller must hold at the split for its 64th to be the records' price
/// less one: `64 (price − 1) + 63` (`constants`: the two byte prices in effect; 690,755 at the
/// spec's prices).
#[test]
fn test_a_caller_one_gas_short_of_its_records_runs_out_of_gas_under_a_limit_they_would_cross() {
    // Records that cost nothing leave no price to be short of, and records priced at a fraction
    // of a gas may round to nothing.
    if history_is_free() || history_rounds() {
        return;
    }
    let records = history(2 * WRITE_RECORD_SIZE);
    let held_at_the_split = 64 * (records - 1) + 63;
    let params = mega_evm::satin_gas_params();
    let value_call = params.get(GasId::transfer_value_cost()) +
        params.get(GasId::warm_storage_read_cost()) +
        params.get(GasId::cold_account_additional_cost()) +
        params.get(GasId::new_account_cost());
    let gas_limit =
        15_000 + body_history(0) + 16 + value_call + account_state_gas() + held_at_the_split;
    let start = START_ON_THE_CALLER + START_ON_THE_FRAME;
    let crosses = mega_evm::TX_BODY_SIZE + start - 1;
    let fits = start_fits();
    let db = || funded().account_code(A, value_call_to_fresh()).account_balance(A, U256::from(1));

    for limit in [crosses, fits] {
        let short = run_at(db(), call(CALLER, A, U256::ZERO, gas_limit), limit);
        assert!(
            matches!(
                short.result,
                ExecutionResult::Halt { reason: MegaHaltReason::Base(HaltReason::OutOfGas(_)), .. }
            ),
            "under a limit of {limit}: the caller runs out of gas paying for the records: {:?}",
            short.result,
        );
        assert_eq!(short.limit_exceeded, None, "under a limit of {limit}: no limit saw the start");
        assert_eq!(
            short.usage,
            LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
            "under a limit of {limit}: nothing was counted beyond the body",
        );
        assert_eq!(short.gas.history, body_history(0), "under a limit of {limit}");
        assert_eq!(short.gas.state, 0, "under a limit of {limit}: the new account came back");
        assert_eq!(short.gas.gas_used, gas_limit, "under a limit of {limit}: the halt consumes it");
    }

    let stopped = run_at(db(), call(CALLER, A, U256::ZERO, gas_limit + 1), crosses);
    assert!(stopped.result.is_success(), "the caller catches the stop: {:?}", stopped.result);
    assert_eq!(
        read_the_answer(stopped.result.output().expect("A answers")),
        Answer::Stop(LimitKind::DataSize, crosses),
        "the start was answered with the stop",
    );
    assert_eq!(stopped.usage, LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 });

    let started = run_at(db(), call(CALLER, A, U256::ZERO, gas_limit + 1), fits);
    assert!(started.result.is_success(), "one gas more, the frame starts: {:?}", started.result);
    assert_eq!(
        started.usage,
        LimitUsage { data_size: mega_evm::TX_BODY_SIZE + start, write_records: 2 }
    );
    assert_eq!(started.gas.history, body_history(0) + records);
    assert_eq!(started.gas.state, account_state_gas());

    if crate::common::runs_at_measurement_prices() {
        return;
    }
    assert_eq!(records, 2 * 40 * 88);
    assert_eq!(gas_limit, 690_755);
}

/* ---------- a child frame's write, and the order where the reservoir runs out ---------- */

/// `A`'s code when the write is a child's: a `CALL` to `B` with `forward` gas and no value,
/// `PUSH0 ×4; PUSH0; PUSH20 B; PUSH4 forward; CALL`, answering as [`answer_the_call`] does.
fn calls_b_with(forward: u64) -> Bytes {
    let code = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0])
        .push_address(B)
        .push_number(u32::try_from(forward).expect("the forward fits a PUSH4"))
        .append(CALL);
    answer_the_call(code)
}

/// A call from `CALLER` to `A`, which calls `B` running `code` with `forward` gas, at `gas_limit`,
/// under a transaction data-size limit of `data_limit`. `B` holds one wei, so a `SELFDESTRUCT`
/// of its moves value.
fn child_outcome(
    code: &Bytes,
    forward: u64,
    gas_limit: u64,
    data_limit: u64,
) -> MegaTransactionOutcome {
    let db = funded()
        .account_code(A, calls_b_with(forward))
        .account_code(B, code.clone())
        .account_balance(B, U256::from(1));
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(data_limit);
    execute_under(db, call(CALLER, A, U256::ZERO, gas_limit), limits)
}

/// What `B`'s write did, read off `A`'s answer: a stop of `B`, a halt of `B`, or a success. `A`
/// itself has gas to spare and room for nothing it keeps, so it always answers.
fn child_bound(outcome: &MegaTransactionOutcome) -> Bound {
    assert!(outcome.result.is_success(), "A answers: {:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None);
    match read_the_answer(outcome.result.output().expect("A answers")) {
        Answer::Stop(..) => Bound::DataSize,
        Answer::Halt => Bound::OutOfGas,
        Answer::Success => Bound::Success,
    }
}

/// The smallest forward at which `code`'s write in `B` is counted rather than run out of gas,
/// below the cap.
fn forward_where_the_write_is_counted(code: &Bytes, data_limit: u64) -> u64 {
    smallest_gas_limit(high_bound(), |forward| {
        !matches!(
            child_bound(&child_outcome(code, forward, GAS_LIMIT, data_limit)),
            Bound::OutOfGas
        )
    })
}

/// `B`'s code for a `SELFDESTRUCT` that moves its wei to `C`, which nobody has touched.
fn selfdestruct_to_c() -> Bytes {
    BytecodeBuilder::default().push_address(C).append(SELFDESTRUCT).build()
}

/// The same rule at a child frame's write [S7.27]: the record's history is the child's to pay,
/// out of the gas it was forwarded, so the forward where the write is counted and the forward
/// where it is kept are exactly the record's history apart, with the caller's gas out of the
/// picture. The fresh slot's forward is also the sum of its parts by hand: two pushes, the
/// store's regular gas and its state gas (`constants`).
#[test]
fn test_whichever_of_gas_and_data_size_binds_first_is_reported_at_a_childs_write() {
    let sites = [
        ("a fresh slot", fresh_slot(), WRITE_RECORD_SIZE, WRITE_RECORD_SIZE),
        ("a log of one byte", log0(1), mega_evm::LOG_BASE_SIZE + 1, mega_evm::LOG_BASE_SIZE + 1),
        (
            "a SELFDESTRUCT that moves value",
            selfdestruct_to_c(),
            2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE,
            2 * WRITE_RECORD_SIZE,
        ),
    ];
    for (name, code, bytes, history_bytes) in sites {
        let crosses = mega_evm::TX_BODY_SIZE + bytes - 1;
        let fits = mega_evm::TX_BODY_SIZE + child_room(bytes);
        let counted_at = forward_where_the_write_is_counted(&code, crosses);
        let kept_at = forward_where_the_write_is_counted(&code, fits);
        let history = history(history_bytes);
        let bound = |forward, limit| child_bound(&child_outcome(&code, forward, GAS_LIMIT, limit));

        assert_eq!(kept_at - counted_at, history, "{name}: a kept record costs its history");
        assert!(matches!(bound(counted_at - 1, crosses), Bound::OutOfGas), "{name}");
        assert!(matches!(bound(counted_at, crosses), Bound::DataSize), "{name}");
        assert!(matches!(bound(kept_at, crosses), Bound::DataSize), "{name}");
        if history > 0 {
            assert!(matches!(bound(counted_at, fits), Bound::OutOfGas), "{name}");
            assert!(matches!(bound(kept_at - 1, fits), Bound::OutOfGas), "{name}");
        }
        assert!(matches!(bound(kept_at, fits), Bound::Success), "{name}");
    }

    let params = mega_evm::satin_gas_params();
    let store_regular = params.get(GasId::sstore_static()) +
        params.get(GasId::cold_storage_cost()) +
        params.get(GasId::sstore_set_without_load_cost());
    assert_eq!(
        forward_where_the_write_is_counted(
            &fresh_slot(),
            mega_evm::TX_BODY_SIZE + WRITE_RECORD_SIZE - 1
        ),
        2 * 3 + store_regular + slot_state_gas(),
        "the fresh slot's forward is two pushes, the store's regular gas and its state gas",
    );
}

/// Above the cap, where a reservoir one gas short of a record's history makes the order visible
/// [S7.27]: the record's history draws the reservoir first and spills its last gas onto the
/// child, which was forwarded exactly the gas of its write and has none left. Counted under a
/// limit it crosses, the record is refused and nothing is charged — the transaction stops, and
/// the spill the child could not pay is never asked of it. A charge made before the check would
/// have run the child out of gas instead, and the limit would have reported nothing. Under a
/// limit the record fits, the spill is asked, the child runs out, and one gas more pays it with
/// the reservoir spent to the last gas.
///
/// Every forward is by hand — pushes, the opcode's regular gas, and the state gas it adds, drawn
/// from the reservoir (`constants`: the schedule in effect for the store, the byte prices for the
/// state gas and the record).
#[test]
fn test_a_record_a_limit_refuses_is_not_charged_where_the_reservoir_has_one_gas_too_few() {
    // A record that costs nothing leaves no gas for the reservoir to be short of, and one priced
    // at a fraction of a gas may round to nothing.
    if history_is_free() || history_rounds() {
        return;
    }
    let params = mega_evm::satin_gas_params();
    let store_regular = params.get(GasId::sstore_static()) +
        params.get(GasId::cold_storage_cost()) +
        params.get(GasId::sstore_set_without_load_cost());
    // (site, code, the child's regular gas up to and including the write, the state gas the write
    // adds, the bytes the limit counts, the record's history bytes, the records kept)
    let sites = [
        (
            "a fresh slot",
            fresh_slot(),
            2 * 3 + store_regular,
            slot_state_gas(),
            WRITE_RECORD_SIZE,
            WRITE_RECORD_SIZE,
            1,
        ),
        (
            "a log of one byte",
            log0(1),
            2 * 3 + (375 + 8 + 3),
            0,
            mega_evm::LOG_BASE_SIZE + 1,
            mega_evm::LOG_BASE_SIZE + 1,
            0,
        ),
        (
            "a SELFDESTRUCT that moves value",
            selfdestruct_to_c(),
            3 + (5_000 + params.selfdestruct_cost(true, true)),
            account_state_gas(),
            2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE,
            2 * WRITE_RECORD_SIZE,
            2,
        ),
    ];
    let body = body_history(0);
    for (name, code, counted_at, state, bytes, history_bytes, records) in sites {
        let record = history(history_bytes);
        let reservoir = body + state + record - 1;
        let gas_limit = TX_GAS_LIMIT_CAP + reservoir;
        let crosses = mega_evm::TX_BODY_SIZE + bytes - 1;
        let fits = mega_evm::TX_BODY_SIZE + child_room(bytes);
        let run = |forward, limit| child_outcome(&code, forward, gas_limit, limit);
        let body_only = LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 };

        // One gas short of the write itself: an out-of-gas before any record exists, under either
        // limit, with the reservoir untouched beyond the body.
        for limit in [crosses, fits] {
            let short = run(counted_at - 1, limit);
            assert!(matches!(child_bound(&short), Bound::OutOfGas), "{name} under {limit}");
            assert_eq!(short.usage, body_only, "{name} under {limit}");
            assert_eq!(short.gas.reservoir_remaining, reservoir - body, "{name} under {limit}");
        }

        // The record refused: counted, the limit crosses, the child stops. Nothing was charged
        // for it, so the one gas it would spill was never asked of the child.
        let refused = run(counted_at, crosses);
        assert!(matches!(child_bound(&refused), Bound::DataSize), "{name}: {:?}", refused.result);
        assert_eq!(
            read_the_answer(refused.result.output().expect("A answers")),
            Answer::Stop(LimitKind::DataSize, crosses),
            "{name}: the record crossed the transaction's limit",
        );
        assert_eq!(refused.gas.history, body, "{name}: the refused record was not charged");
        assert_eq!(refused.gas.state, 0, "{name}: the stop gave the state gas back");
        assert_eq!(refused.usage, body_only, "{name}: the record went with the child");
        assert_eq!(refused.gas.reservoir_remaining, reservoir - body, "{name}: the reservoir");

        // The record kept: its history draws the reservoir's last `record - 1` and spills one gas
        // the child does not have, so the child runs out, and its write comes back.
        let kept = run(counted_at, fits);
        assert!(matches!(child_bound(&kept), Bound::OutOfGas), "{name}: {:?}", kept.result);
        assert_eq!(kept.usage, body_only, "{name}: the child's write came back with it");
        assert_eq!(kept.gas.history, body, "{name}");
        assert_eq!(kept.gas.state, 0, "{name}");
        assert_eq!(kept.gas.reservoir_remaining, reservoir - body, "{name}");

        // One gas more pays the spill: the write is kept, and the reservoir is spent to the last
        // gas.
        let paid = run(counted_at + 1, fits);
        assert!(matches!(child_bound(&paid), Bound::Success), "{name}: {:?}", paid.result);
        assert_eq!(paid.gas.reservoir_remaining, 0, "{name}: the reservoir spent to the last gas");
        assert_eq!(paid.gas.history, body + record, "{name}: the body and the record");
        assert_eq!(paid.gas.state, state, "{name}: the state the write added");
        assert_eq!(
            paid.usage,
            LimitUsage { data_size: mega_evm::TX_BODY_SIZE + bytes, write_records: records },
            "{name}",
        );
    }
}

/* ---------- a body over the limit, at the smallest gas limit validation accepts ---------- */

/// A fresh account nobody has touched.
const FRESH: Address = address!("0000000000000000000000000000000000300005");

/// The account an EIP-7702 authorization of [`authorizing_call`] delegates.
const AUTHORITY: Address = address!("0000000000000000000000000000000000300006");

/// A type-4 call from `CALLER` to `A`, carrying one authorization of [`AUTHORITY`] to `B`.
fn authorizing_call(gas_limit: u64) -> mega_evm::MegaTransaction {
    crate::common::authorizing_call(CALLER, A, U256::ZERO, gas_limit, B, &[(AUTHORITY, 0)])
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

/// A body over the limit is rejected whatever the gas limit, before its gas is looked at: at a
/// gas limit validation would otherwise accept, below it, and above it. A body at the limit leaves
/// validation where it is without one: the smallest gas limit it accepts is the same.
#[test]
fn test_a_body_over_the_limit_is_rejected_at_every_gas_limit() {
    type Tx = fn(u64) -> mega_evm::MegaTransaction;
    let cases: [(&str, Tx, u64); 3] = [
        ("a value transfer to a fresh account", |gas| call(CALLER, FRESH, U256::from(1), gas), 0),
        ("a creation", |gas| create(CALLER, constructor_returning(1), gas), 12),
        // The authorization's record counts beside its tuple.
        ("an authorization", authorizing_call, mega_evm::AUTHORIZATION_SIZE + WRITE_RECORD_SIZE),
    ];
    assert_eq!(constructor_returning(1).len(), 12);
    for (name, tx, extra) in cases {
        let at = mega_evm::TX_BODY_SIZE + extra;
        let valid = smallest_gas_limit(GAS_LIMIT, |gas| outcome_at(tx(gas), u64::MAX).is_some());
        assert_eq!(
            smallest_gas_limit(GAS_LIMIT, |gas| outcome_at(tx(gas), at).is_some()),
            valid,
            "{name}: a limit the transaction fits does not move validation"
        );
        for gas in [0, valid - 1, valid, valid + 10_000] {
            let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(at - 1);
            let error = MegaEvm::new(context(funded()).with_tx_runtime_limits(limits))
                .execute_transaction(tx(gas))
                .expect_err("rejected");
            assert_body_refused(&format!("{name} at {gas}: {error:?}"));
        }
    }
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
        assert_eq!(outcome.limit_exceeded, None, "{name}: a frame budget below is no stop");
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

/// A body over the limit is rejected before an interceptor could answer the transaction's own
/// call, whatever the frame's target.
#[test]
fn test_a_body_over_the_limit_is_rejected_before_an_interceptor() {
    use alloy_sol_types::SolCall;
    use mega_evm::system::{IMegaLimitControl, LIMIT_CONTROL_ADDRESS};
    let selector = IMegaLimitControl::remainingComputeGasCall::SELECTOR;
    let body = mega_evm::TX_BODY_SIZE + selector.len() as u64;
    let tx =
        || call_with_data(CALLER, LIMIT_CONTROL_ADDRESS, Bytes::from(selector.to_vec()), GAS_LIMIT);
    let answered = run_at(funded(), tx(), body);
    assert!(answered.result.is_success(), "the interceptor answers: {:?}", answered.result);
    let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(body - 1);
    let error = MegaEvm::new(context(funded()).with_tx_runtime_limits(limits))
        .execute_transaction(tx())
        .expect_err("rejected");
    assert_body_refused(&format!("{error:?}"));
}

/* ---------- the nonce record a failed creation leaves its creator ---------- */

/// Logs `data_len` bytes, then attempts an empty creation — `CREATE2` when `create2` — and stops.
fn log_then_create(data_len: u64, create2: bool) -> Bytes {
    let builder = BytecodeBuilder::default().push_number(data_len).push_number(0_u64).append(LOG0);
    let builder = if create2 {
        builder.append_many([PUSH0, PUSH0, PUSH0, PUSH0]).append(CREATE2)
    } else {
        builder.append_many([PUSH0, PUSH0, PUSH0]).append(CREATE)
    };
    builder.append(POP).append(STOP).build()
}

/// Calls `target` with no value and returns whatever it returned or reverted with.
fn call_and_return_its_output(target: Address) -> Bytes {
    then_call(BytecodeBuilder::default(), target)
        .append(RETURNDATASIZE)
        .append_many([PUSH0, PUSH0])
        .append(RETURNDATACOPY)
        .append(RETURNDATASIZE)
        .append(PUSH0)
        .append(RETURN)
        .build()
}

/// A creation too big for its share still bumps its creator's nonce, and the creator's record of
/// that write outlives the creation: it lands on the creator's own lane. The creator is held to
/// its budget with it before it runs on.
///
/// `A` calls `B` with 4,000 bytes left, so `B` may keep 3,920. `B` logs, then creates: `B`'s nonce
/// record is counted on `B` first, and the creation's start records the created account, 40
/// bytes against a share of what the record left `B`, which is below 40, so it is stopped with
/// room to spare in the transaction. When the log left `B` 40 bytes, `B` keeps the nonce record at
/// exactly its budget and succeeds. When it left 39, the record puts `B` one byte over: `B`
/// reverts alone, its log and its nonce with it, and `A` resumes and returns `B`'s revert. Either
/// way the history the transaction pays is that of the bytes it keeps.
#[test]
fn test_a_failed_creations_nonce_record_holds_its_creator_to_its_budget() {
    const A_BUDGET: u64 = 4_000;
    let b_budget = share(A_BUDGET);
    let limit = mega_evm::TX_BODY_SIZE + A_BUDGET;
    let history_of = |usage: LimitUsage| mega_evm::history_gas(usage.data_size).unwrap();
    let run = |left: u64, create2: bool, inspect: bool| {
        let logged = b_budget - left - 32;
        let db = funded()
            .account_code(A, call_and_return_its_output(B))
            .account_code(B, log_then_create(logged, create2));
        let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit);
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .with_inspector(Probe::default());
        Evm::set_inspector_enabled(&mut evm, inspect);
        let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
        (outcome, evm.inspector().clone())
    };

    for create2 in [false, true] {
        for inspect in [false, true] {
            let case = format!("create2 {create2}, inspect {inspect}");
            let (fits, probe) = run(WRITE_RECORD_SIZE, create2, inspect);
            assert!(fits.result.is_success(), "{case}: {:?}", fits.result);
            assert_eq!(fits.limit_exceeded, None, "{case}");
            assert_eq!(
                fits.usage,
                LimitUsage { data_size: mega_evm::TX_BODY_SIZE + b_budget, write_records: 1 },
                "{case}: B keeps the nonce record at exactly its budget",
            );
            assert_eq!(fits.gas.history, history_of(fits.usage), "{case}");
            assert_eq!(fits.result.logs().len(), 1, "{case}");
            assert_eq!(fits.state[&B].info.nonce, 1, "{case}");
            assert_eq!(fits.result.output().unwrap(), &Bytes::new(), "{case}: B stopped");
            if inspect {
                assert_eq!(probe.creates.len(), 1, "{case}");
                assert_eq!(probe.creates[0].0, InstructionResult::Revert, "{case}");
            }

            let (over, _) = run(WRITE_RECORD_SIZE - 1, create2, inspect);
            assert!(over.result.is_success(), "{case}: A resumes: {:?}", over.result);
            assert_eq!(over.limit_exceeded, None, "{case}: a frame budget below is no stop");
            assert_eq!(
                over.usage,
                LimitUsage { data_size: mega_evm::TX_BODY_SIZE, write_records: 0 },
                "{case}: B's log and nonce record went with its revert",
            );
            assert_eq!(over.gas.history, history_of(over.usage), "{case}: and their history");
            assert!(over.result.logs().is_empty(), "{case}");
            assert!(over.state.get(&B).is_none_or(|b| b.info.nonce == 0), "{case}");
            assert_eq!(
                over.result.output().unwrap().as_ref(),
                MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit: b_budget }
                    .abi_encode(),
                "{case}: B reverted on its own budget",
            );
        }
    }
}

/// A creation's start is held to the transaction's limit even when it counts nothing: the record of
/// its creator's nonce lands on the creator unchecked, and the creation's start is where the
/// transaction first meets it.
///
/// `A` funds `X`, the address `B`'s empty `CREATE2` lands on, keeping two records and the transfer
/// log, and then calls `B`. `B`'s creation records nothing of its own — `X` is changed already, and
/// it carries no endowment — but the record of `B`'s nonce puts the transaction one byte over its
/// limit: the creation returns the stop before its first instruction, `B` returns it once the
/// creation returned into it, its nonce and its record going with its revert, and `A` resumes and
/// keeps what it kept.
#[test]
fn test_a_creation_counting_nothing_is_stopped_by_its_creators_record() {
    let x = B.create2(B256::ZERO, keccak256([]));
    let a = then_call(BytecodeBuilder::default().call(x, U256::from(1)), B).append(STOP).build();
    let b = BytecodeBuilder::default()
        .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
        .append(CREATE2)
        .append(POP)
        .append(STOP)
        .build();
    let kept = mega_evm::TX_BODY_SIZE + 2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE;
    let limit = kept + WRITE_RECORD_SIZE - 1;
    let stop =
        Bytes::from(MegaLimitExceeded { kind: LimitKind::DataSize.as_u8(), limit }.abi_encode());
    for inspect in [false, true] {
        let db = funded()
            .account_code(A, a.clone())
            .account_balance(A, U256::from(1))
            .account_code(B, b.clone());
        let limits = EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit);
        let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
            .with_inspector(Probe::default());
        Evm::set_inspector_enabled(&mut evm, inspect);
        let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, GAS_LIMIT)).unwrap();
        assert!(outcome.result.is_success(), "inspect {inspect}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "inspect {inspect}: A caught it");
        assert_eq!(
            outcome.usage,
            LimitUsage { data_size: kept, write_records: 2 },
            "inspect {inspect}: A's transfer alone",
        );
        assert!(outcome.state.get(&B).is_none_or(|b| b.info.nonce == 0), "inspect {inspect}");
        assert_eq!(outcome.state[&x].info.nonce, 0, "inspect {inspect}: nothing created at X");
        if inspect {
            let probe = evm.inspector();
            assert_eq!(
                probe.creates,
                vec![(InstructionResult::Revert, stop.clone())],
                "the creation returns the transaction's stop before its first instruction",
            );
            assert!(
                probe.calls.contains(&(B, InstructionResult::Revert, stop.clone())),
                "B returns it once the creation returned into it: {:?}",
                probe.calls,
            );
        }
    }
}
