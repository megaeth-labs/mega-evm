//! The revert-class stop, pinned as a matrix.
//!
//! A transaction-level limit — data size, KV updates, state gas, or gas detention's compute limit —
//! stops the transaction with a revert. The frame that crosses it reverts with
//! `MegaLimitExceeded(kind, limit)`, the transaction is latched, and every frame above returns the
//! same revert without running another instruction, whatever produced its result. The transaction
//! settles like an EIP-8037 revert: the sender pays for what ran, and its unspent regular gas and
//! the reservoir come back. A frame budget reverts its frame alone, and its caller runs on. A real
//! out-of-gas and a precompile given less than its price still halt, and burn what their frame was
//! given.
//!
//! The matrix crosses each limit in the transaction's own frame and in a frame three calls below
//! it, without an inspector and under one that rewrites every frame result into a success, below
//! the execution cap and above it, where a reservoir pays the body first. What ran is read off a
//! twin of each transaction: the same frames, each reverting where the stopped one crossed its
//! limit or would have resumed. The twin runs without limits and revm settles its reverts itself,
//! so it bills exactly what the stop should, plus the two pushes each of its reverts costs.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{BLOCK_ENV_ACCESS_COMPUTE_GAS, TX_DATA_LIMIT, TX_GAS_LIMIT_CAP},
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm, MegaHaltReason,
    MegaTransactionOutcome, FRAME_DATA_SHARE_DENOMINATOR, FRAME_DATA_SHARE_NUMERATOR,
    LOG_BASE_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        CALL, GAS, JUMP, JUMPDEST, LOG0, MLOAD, POP, PUSH0, RETURNDATACOPY, RETURNDATASIZE, REVERT,
        SSTORE, TIMESTAMP,
    },
    context::result::ExecutionResult,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        InstructionResult, Interpreter,
    },
    Database, Inspector,
};

use crate::{
    common::{call, call_with_data},
    detention::{context, work},
};

const CALLER: Address = address!("0000000000000000000000000000000000500000");
const A: Address = address!("0000000000000000000000000000000000500001");
const B: Address = address!("0000000000000000000000000000000000500002");
const C: Address = address!("0000000000000000000000000000000000500003");
const D: Address = address!("0000000000000000000000000000000000500004");
/// The frames of a chain, by depth: `A` is the transaction's own frame, `D` is three calls below.
const CHAIN: [Address; 4] = [A, B, C, D];

/// Below the execution cap: no reservoir.
const BELOW: u64 = 100_000_000;
/// Above it: a reservoir of 100,000,000, which pays the body's history first.
const ABOVE: u64 = TX_GAS_LIMIT_CAP + 100_000_000;
const TIERS: [u64; 2] = [BELOW, ABOVE];

/// Gas detention's cap on a read of the block environment, the spec's.
const CAP: u64 = BLOCK_ENV_ACCESS_COMPUTE_GAS;

/// The fresh slots the transaction's own frame writes before it calls or crosses.
const OWN_SLOTS: u64 = 2;
/// The fresh slot whose write crosses a limit.
const CROSSING_SLOT: U256 = U256::from_limbs([0x10, 0, 0, 0]);
/// The slot a frame writes once it runs on after its call or its crossing: a write of it is an
/// instruction run after the stop.
const MARKER: U256 = U256::from_limbs([0xaa, 0, 0, 0]);
/// Rounds of the compute loop a frame runs after it read the block's timestamp: over 21,000,000
/// of compute, past the cap.
const ROUNDS: u32 = 7_000;
/// What the two pushes before each of the twin's reverts cost.
const TWIN_REVERT: u64 = 4;

/// A transaction-level limit the matrix crosses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Limit {
    DataSize,
    KvUpdates,
    StateGas,
    Compute,
}

impl Limit {
    const ALL: [Self; 4] = [Self::DataSize, Self::KvUpdates, Self::StateGas, Self::Compute];

    const fn kind(self) -> LimitKind {
        match self {
            Self::DataSize => LimitKind::DataSize,
            Self::KvUpdates => LimitKind::KVUpdate,
            Self::StateGas => LimitKind::StateGrowth,
            Self::Compute => LimitKind::ComputeGas,
        }
    }
}

/// Appends a log with no topic and no data: 32 bytes of data size, and a log a stop must not keep.
fn log(code: BytecodeBuilder) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, LOG0])
}

/// Appends a call to `to` with all the gas, leaving its status on the stack.
fn call_all(code: BytecodeBuilder, to: Address) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]).push_address(to).append(GAS).append(CALL)
}

/// Appends a revert with no data.
fn revert(code: BytecodeBuilder) -> BytecodeBuilder {
    code.append_many([PUSH0, PUSH0, REVERT])
}

/// The code of the frame at `depth` of a chain whose frame at `crossing` crosses `limit`.
///
/// Every frame logs first, and the transaction's own frame writes [`OWN_SLOTS`] fresh slots. The
/// frame at `crossing` then crosses the limit — a fresh slot's write, or a read of the block's
/// timestamp and more compute than the cap — and every frame above it calls the next. The stopped
/// transaction's frames then write [`MARKER`] and log again; the twin's frames revert instead.
fn frame_code(limit: Limit, depth: usize, crossing: usize, twin: bool) -> Bytes {
    let mut code = log(BytecodeBuilder::default());
    if depth == 0 {
        for slot in 1..=OWN_SLOTS {
            code = code.sstore(U256::from(slot), U256::from(1));
        }
    }
    code = if depth < crossing {
        let code = call_all(code, CHAIN[depth + 1]);
        if twin {
            code
        } else {
            code.append(POP)
        }
    } else {
        match limit {
            Limit::Compute if twin => code.append(TIMESTAMP),
            Limit::Compute => work(code.append_many([TIMESTAMP, POP]), ROUNDS),
            _ => code.sstore(CROSSING_SLOT, U256::from(1)),
        }
    };
    if twin {
        revert(code).build()
    } else {
        log(code.sstore(MARKER, U256::from(1))).stop().build()
    }
}

/// The chain whose frame at `crossing` crosses `limit`, stopped or its twin.
fn chain(limit: Limit, crossing: usize, twin: bool) -> MemoryDatabase {
    (0..=crossing).fold(MemoryDatabase::default(), |db, depth| {
        db.account_code(CHAIN[depth], frame_code(limit, depth, crossing, twin))
    })
}

/// What a transaction from `CALLER` to `A` with `gas_limit` spends before its first instruction: an
/// `A` that stops at once, without limits.
fn intrinsic(gas_limit: u64) -> MegaTransactionOutcome {
    let db = MemoryDatabase::default().account_code(A, BytecodeBuilder::default().stop().build());
    let outcome = execute(db, EvmTxRuntimeLimits::no_limits(), gas_limit);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    assert_eq!(outcome.gas.state, 0);
    // The body's history is paid before the first frame, from the reservoir when there is one.
    let reservoir = gas_limit.saturating_sub(TX_GAS_LIMIT_CAP).saturating_sub(outcome.gas.history);
    assert_eq!(outcome.gas.reservoir_remaining, reservoir);
    outcome
}

/// The state gas of one fresh slot of `A`, at the price the engine runs.
fn one_slot() -> u64 {
    let code = BytecodeBuilder::default().sstore(U256::from(1), U256::from(1)).stop().build();
    let db = MemoryDatabase::default().account_code(A, code);
    let outcome = execute(db, EvmTxRuntimeLimits::no_limits(), BELOW);
    assert!(outcome.result.is_success(), "{:?}", outcome.result);
    outcome.gas.state
}

/// Runs a call from `CALLER` to `A` under `limits`.
fn execute(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    gas_limit: u64,
) -> MegaTransactionOutcome {
    MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .execute_transaction(call(CALLER, A, U256::ZERO, gas_limit))
        .expect("the transaction is valid")
}

/// Records the steps and the result of every frame it sees end, then rewrites that result into a
/// success with no output: a tool's inspector the latch must see through.
#[derive(Clone, Default)]
struct Rewriter {
    /// The frame and opcode of the last step, and the slot it named when it was an `SSTORE`.
    last: Option<(Address, u8, Option<U256>)>,
    /// How many writes of [`MARKER`] ran.
    marker_writes: usize,
    /// The frames that ended, deepest first, with the result each had before the rewrite.
    ended: Vec<(Address, InstructionResult, Bytes)>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Rewriter {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        let opcode = interp.bytecode.opcode();
        let slot = (opcode == SSTORE).then(|| interp.stack.peek(0).expect("SSTORE takes a slot"));
        self.marker_writes += usize::from(slot == Some(MARKER));
        self.last = Some((interp.input.target_address, opcode, slot));
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let result = &mut outcome.result;
        self.ended.push((inputs.target_address, result.result, result.output.clone()));
        result.result = InstructionResult::Stop;
        result.output = Bytes::new();
    }
}

/// One row of the matrix, both columns: `limit` crossed by the frame at `crossing` of a
/// transaction with `gas_limit`, without an inspector and under a [`Rewriter`].
fn assert_cell(limit: Limit, crossing: usize, gas_limit: u64, slot: u64) {
    let row = format!("{limit:?} at depth {crossing}, gas limit {gas_limit}");
    let frames = crossing as u64 + 1;
    let intrinsic = intrinsic(gas_limit);

    // The twin: the same frames, reverting where the stopped ones crossed or would resume.
    let twin = execute(chain(limit, crossing, true), EvmTxRuntimeLimits::no_limits(), gas_limit);
    assert!(
        matches!(&twin.result, ExecutionResult::Revert { output, .. } if output.is_empty()),
        "{row}: the twin reverts: {:?}",
        twin.result
    );
    assert_eq!(twin.limit_exceeded, None, "{row}");
    assert_eq!(
        (twin.gas.state, twin.gas.history, twin.gas.reservoir_remaining),
        (0, intrinsic.gas.history, intrinsic.gas.reservoir_remaining),
        "{row}: a revert keeps nothing but the body",
    );
    // The regular gas the frames spent up to the crossing: for the compute limit, up to the read.
    let before_crossing = twin.gas.regular - intrinsic.gas.regular - TWIN_REVERT * frames;

    // The limit crossed, and the usage that crossed it: one more byte, record or unit of state gas
    // than the limit. The compute limit is set by the read, at the compute it ran to plus the cap,
    // and the stop reports it as what was used.
    let (configured, value) = limits_of(limit, crossing, slot);
    let (value, used) = match value {
        Some(value) => (value, value + 1),
        None => (before_crossing + CAP, before_crossing + CAP),
    };
    let stop =
        LimitCheck::ExceedsLimit { kind: limit.kind(), limit: value, used, frame_local: false };
    // What ran: up to the crossing, and for the compute limit the cap past the read.
    let ran = if limit == Limit::Compute { value } else { before_crossing };

    let plain = execute(chain(limit, crossing, false), configured, gas_limit);
    let mut evm =
        MegaEvm::new(context(chain(limit, crossing, false)).with_tx_runtime_limits(configured))
            .with_inspector(Rewriter::default());
    let rewritten = evm.execute_transaction(call(CALLER, A, U256::ZERO, gas_limit)).unwrap();
    let rewriter = evm.inspector();

    for (column, outcome) in [("plain", &plain), ("rewritten", &rewritten)] {
        let cell = format!("{row}, {column}");
        // The receipt: a revert carrying the stop, which the outcome names.
        match &outcome.result {
            ExecutionResult::Revert { output, .. } => {
                assert_eq!(output, &stop.revert_data(), "{cell}: the stop's revert data")
            }
            other => panic!("{cell}: expected the stop, got {other:?}"),
        }
        assert_eq!(outcome.limit_exceeded, Some(stop), "{cell}");

        // The bill: intrinsic gas plus what ran, on the regular ledger alone.
        assert_eq!(outcome.gas.regular, intrinsic.gas.regular + ran, "{cell}: regular");
        assert_eq!(outcome.gas.state, 0, "{cell}: a stop keeps no state gas");
        assert_eq!(outcome.gas.history, intrinsic.gas.history, "{cell}: the body's history alone");
        assert_eq!(outcome.gas.history_bytes, intrinsic.gas.history_bytes, "{cell}");
        assert_eq!(
            outcome.gas.reservoir_remaining, intrinsic.gas.reservoir_remaining,
            "{cell}: the reservoir comes back, less the body it paid"
        );
        assert_eq!(outcome.gas.gas_used, intrinsic.gas.gas_used + ran, "{cell}: gas used");

        // Nothing the stopped frames wrote or logged is kept; the body is all the transaction
        // keeps.
        assert!(outcome.result.logs().is_empty(), "{cell}: no log is kept");
        for account in &CHAIN[..=crossing] {
            let kept = outcome
                .state
                .get(account)
                .is_some_and(|account| account.storage.values().any(|slot| slot.is_changed()));
            assert!(!kept, "{cell}: {account} kept a write");
        }
        assert_eq!(
            outcome.usage,
            LimitUsage { data_size: TX_BODY_SIZE, write_records: 0 },
            "{cell}: the body stays"
        );
    }
    // No instruction ran after the crossing: the bill above has no marker's write in it, and under
    // the inspector none ran at all. Every frame returned the stop, and the rewrites into successes
    // did not reach a caller: the inspector changed nothing the transaction reports.
    assert_eq!(rewritten.gas, plain.gas, "{row}");
    assert_eq!(rewriter.marker_writes, 0, "{row}: no frame ran on after the crossing");
    if limit != Limit::Compute {
        assert_eq!(
            rewriter.last,
            Some((CHAIN[crossing], SSTORE, Some(CROSSING_SLOT))),
            "{row}: the crossing write was the last step"
        );
    }
    let ended: Vec<_> = CHAIN[..=crossing]
        .iter()
        .rev()
        .map(|frame| (*frame, InstructionResult::Revert, stop.revert_data()))
        .collect();
    assert_eq!(rewriter.ended, ended, "{row}: every frame returned the stop, deepest first");
}

/// Every transaction-level limit, crossed by the transaction's own frame and by a frame three calls
/// below it, stops the transaction the same way — below the execution cap and above it, with and
/// without an inspector that rewrites every frame result into a success.
#[test]
fn test_every_limit_stops_the_transaction_at_every_depth_and_tier() {
    let slot = one_slot();
    for limit in Limit::ALL {
        for crossing in [0, 3] {
            for gas_limit in TIERS {
                assert_cell(limit, crossing, gas_limit, slot);
            }
        }
    }
}

/// The limits a cell runs under, and the stop's limit where it is known before the run: every
/// dimension but the compute limit, which the read sets.
fn limits_of(limit: Limit, crossing: usize, slot: u64) -> (EvmTxRuntimeLimits, Option<u64>) {
    let frames = crossing as u64 + 1;
    match limit {
        Limit::DataSize => {
            let value =
                TX_BODY_SIZE + frames * LOG_BASE_SIZE + (OWN_SLOTS + 1) * WRITE_RECORD_SIZE - 1;
            (EvmTxRuntimeLimits::default().with_tx_data_size_limit(value), Some(value))
        }
        Limit::KvUpdates => {
            (EvmTxRuntimeLimits::default().with_tx_kv_update_limit(OWN_SLOTS), Some(OWN_SLOTS))
        }
        Limit::StateGas => {
            let value = (OWN_SLOTS + 1) * slot - 1;
            (EvmTxRuntimeLimits::default().with_tx_state_gas_limit(value), Some(value))
        }
        Limit::Compute => (EvmTxRuntimeLimits::default(), None),
    }
}

/* ---------- frame budgets ---------- */

/// The slot a frame stores its call's status in.
const STATUS: U256 = U256::from_limbs([0xa1, 0, 0, 0]);
/// The slot a frame stores the size of what its call returned in.
const RETURNED: U256 = U256::from_limbs([0xa2, 0, 0, 0]);
/// The slot a frame stores the kind its callee's stop named in.
const STOP_KIND: U256 = U256::from_limbs([0xa3, 0, 0, 0]);
/// The slot a frame stores the limit its callee's stop named in.
const STOP_LIMIT: U256 = U256::from_limbs([0xa4, 0, 0, 0]);

/// A frame that logs, calls `to` with `gas` (all of it when `None`), and runs on: it stores the
/// call's status and the size of what the call returned, with `decode` the kind and the limit of
/// the `MegaLimitExceeded` it returned, then writes [`MARKER`].
fn resuming(to: Address, gas: Option<u64>, decode: bool) -> Bytes {
    let code = log(BytecodeBuilder::default()).append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]);
    let code = match gas {
        Some(gas) => code.push_address(to).push_number(gas),
        None => code.push_address(to).append(GAS),
    };
    let mut code = code
        .append(CALL)
        .push_u256(STATUS)
        .append(SSTORE)
        .append(RETURNDATASIZE)
        .push_u256(RETURNED)
        .append(SSTORE);
    if decode {
        // The two words after the selector: the kind, then the limit.
        code = code
            .push_number(64_u8)
            .push_number(4_u8)
            .append_many([PUSH0, RETURNDATACOPY, PUSH0, MLOAD])
            .push_u256(STOP_KIND)
            .append(SSTORE)
            .push_number(32_u8)
            .append(MLOAD)
            .push_u256(STOP_LIMIT)
            .append(SSTORE);
    }
    code.sstore(MARKER, U256::from(1)).stop().build()
}

/// A frame that logs and writes the fresh slots `1..=slots`, then [`MARKER`].
fn writer(slots: u64) -> Bytes {
    (1..=slots)
        .fold(log(BytecodeBuilder::default()), |code, slot| {
            code.sstore(U256::from(slot), U256::from(1))
        })
        .sstore(MARKER, U256::from(1))
        .stop()
        .build()
}

/// `A -> B -> C -> D`, `D` running `d`: each of `A`, `B` and `C` runs on after its call, and `C`
/// decodes what `D` returned.
fn resuming_chain(d: Bytes) -> MemoryDatabase {
    MemoryDatabase::default()
        .account_code(A, resuming(B, None, false))
        .account_code(B, resuming(C, None, false))
        .account_code(C, resuming(D, None, true))
        .account_code(D, d)
}

/// What a child gets of what its parent has left.
const fn share(remaining: u64) -> u64 {
    remaining * FRAME_DATA_SHARE_NUMERATOR / FRAME_DATA_SHARE_DENOMINATOR
}

/// The value `outcome` left in `slot` of `account`.
fn slot_of(outcome: &MegaTransactionOutcome, account: Address, slot: U256) -> U256 {
    outcome
        .state
        .get(&account)
        .and_then(|account| account.storage.get(&slot))
        .map_or(U256::ZERO, |slot| slot.present_value())
}

/// A frame three calls below the transaction's own that crosses its budget — in data size or in
/// write records — reverts alone with `MegaLimitExceeded(kind, budget)`, where the budget is what
/// the shares left it. Its caller resumes and reads the stop, every frame above runs on, nothing is
/// latched and the transaction succeeds, below and above the execution cap.
#[test]
fn test_a_frame_budget_crossed_three_calls_down_reverts_that_frame_alone() {
    // The transaction's own frame gets the room above the body; each relay logs before it calls,
    // which counts 32 bytes and no write record.
    const ROOM: u64 = 1_000;
    let data_budget = share(share(share(ROOM - LOG_BASE_SIZE) - LOG_BASE_SIZE) - LOG_BASE_SIZE);
    const RECORDS: u64 = 100;
    let record_budget = share(share(share(RECORDS)));
    let rows = [
        (
            LimitKind::DataSize,
            EvmTxRuntimeLimits::default().with_tx_data_size_limit(TX_BODY_SIZE + ROOM),
            data_budget,
            // D's log and its writes cross its budget long before the transaction's limit.
            writer(data_budget / WRITE_RECORD_SIZE + 1),
        ),
        (
            LimitKind::KVUpdate,
            EvmTxRuntimeLimits::default().with_tx_kv_update_limit(RECORDS),
            record_budget,
            writer(record_budget + 1),
        ),
    ];
    for (kind, limits, budget, d) in rows {
        for gas_limit in TIERS {
            let case = format!("{kind:?}, gas limit {gas_limit}");
            let outcome = execute(resuming_chain(d.clone()), limits, gas_limit);
            assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
            assert_eq!(outcome.limit_exceeded, None, "{case}: a frame budget latches nothing");

            // C read D's stop: a failed call returning `MegaLimitExceeded(kind, budget)`.
            assert_eq!(slot_of(&outcome, C, STATUS), U256::ZERO, "{case}: D reverted");
            assert_eq!(slot_of(&outcome, C, RETURNED), U256::from(68), "{case}");
            assert_eq!(slot_of(&outcome, C, STOP_KIND), U256::from(kind.as_u8()), "{case}");
            assert_eq!(slot_of(&outcome, C, STOP_LIMIT), U256::from(budget), "{case}");
            // Every caller ran on, D kept nothing.
            for frame in [A, B] {
                assert_eq!(slot_of(&outcome, frame, STATUS), U256::from(1), "{case}: {frame}");
            }
            for frame in [A, B, C] {
                assert_eq!(slot_of(&outcome, frame, MARKER), U256::from(1), "{case}: {frame}");
            }
            let d_kept = outcome
                .state
                .get(&D)
                .is_some_and(|d| d.storage.values().any(|slot| slot.is_changed()));
            assert!(!d_kept, "{case}: D kept a write");
            let loggers: Vec<_> = outcome.result.logs().iter().map(|log| log.address).collect();
            assert_eq!(loggers, [A, B, C], "{case}: D's log went with its frame");
        }
    }
}

/* ---------- halts ---------- */

/// Every per-transaction limit, armed and never crossed: a halt must stay a halt under them.
fn armed() -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::default()
        .with_tx_data_size_limit(TX_DATA_LIMIT)
        .with_tx_kv_update_limit(1_000)
        .with_tx_state_gas_limit(TX_GAS_LIMIT_CAP)
}

/// A loop that never ends.
fn spin() -> Bytes {
    BytecodeBuilder::default().append(JUMPDEST).append_many([PUSH0, JUMP]).build()
}

/// The ECRECOVER precompile, whose price is 3,000 whatever its input.
const ECRECOVER: Address = address!("0000000000000000000000000000000000000001");
/// The MODEXP precompile.
const MODEXP: Address = address!("0000000000000000000000000000000000000005");

/// A frame three calls below the transaction's own that halts burns what its caller gave it: a
/// frame that runs out of its own gas, and a precompile given less than its price. Its caller
/// resumes and the transaction succeeds, with every limit armed, below and above the execution
/// cap. Two forwards that differ by some gas bill the transaction exactly that much apart.
#[test]
fn test_a_halt_three_calls_down_burns_its_frames_gas_and_its_caller_resumes() {
    // (what C calls, and two forwards it may give it, both short of what it needs)
    let rows = [(D, 50_000, 80_000), (ECRECOVER, 1_000, 2_999)];
    for (callee, small, large) in rows {
        for gas_limit in TIERS {
            let case = format!("{callee}, gas limit {gas_limit}");
            let run = |forward: u64| {
                let db = MemoryDatabase::default()
                    .account_code(A, resuming(B, None, false))
                    .account_code(B, resuming(C, None, false))
                    .account_code(C, resuming(callee, Some(forward), false))
                    .account_code(D, spin());
                let outcome = execute(db, armed(), gas_limit);
                assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
                assert_eq!(outcome.limit_exceeded, None, "{case}");
                assert_eq!(slot_of(&outcome, C, STATUS), U256::ZERO, "{case}: the callee halted");
                assert_eq!(
                    slot_of(&outcome, C, RETURNED),
                    U256::ZERO,
                    "{case}: a halt returns nothing"
                );
                for frame in [A, B, C] {
                    assert_eq!(slot_of(&outcome, frame, MARKER), U256::from(1), "{case}: {frame}");
                }
                outcome.gas
            };
            let (small_gas, large_gas) = (run(small), run(large));
            assert_eq!(
                large_gas.gas_used - small_gas.gas_used,
                large - small,
                "{case}: the halt burned the whole forward"
            );
            assert_eq!(large_gas.regular - small_gas.regular, large - small, "{case}");
            assert_eq!(
                (large_gas.state, large_gas.history, large_gas.reservoir_remaining),
                (small_gas.state, small_gas.history, small_gas.reservoir_remaining),
                "{case}: only regular gas burns"
            );
        }
    }
}

/// The transaction's own frame that halts burns all its regular gas, with every limit armed: a
/// frame that runs out of its own gas, and a call straight to a precompile priced above what the
/// transaction has — ECRECOVER one gas short below the execution cap, a MODEXP priced past the cap
/// above it. The reservoir comes back, less the body it paid.
#[test]
fn test_the_transactions_own_frame_that_halts_burns_its_regular_gas() {
    use revm::context::result::{HaltReason, OutOfGasError};

    for gas_limit in TIERS {
        let reservoir = intrinsic(gas_limit).gas.reservoir_remaining;
        let db = MemoryDatabase::default().account_code(A, spin());
        let outcome = execute(db, armed(), gas_limit);
        assert!(
            matches!(
                &outcome.result,
                ExecutionResult::Halt { reason: MegaHaltReason::Base(HaltReason::OutOfGas(_)), .. }
            ),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(outcome.gas.reservoir_remaining, reservoir, "gas limit {gas_limit}");
        assert_eq!(outcome.gas.gas_used + reservoir, gas_limit, "the regular gas burned");
    }

    // ECRECOVER below the cap, given one gas less than its price.
    let precompile_tx = |to: Address, input: Vec<u8>, gas_limit: u64| {
        MegaEvm::new(context(MemoryDatabase::default()).with_tx_runtime_limits(armed()))
            .execute_transaction(call_with_data(CALLER, to, input.into(), gas_limit))
            .expect("the transaction is valid")
    };
    let paid = precompile_tx(ECRECOVER, Vec::new(), BELOW);
    assert!(paid.result.is_success(), "{:?}", paid.result);
    let before_frame = paid.gas.gas_used - 3_000;
    let short = precompile_tx(ECRECOVER, Vec::new(), before_frame + 2_999);
    let modexp_input = crate::withheld_gas::costly_modexp_input(512, 0);
    let past_the_cap = precompile_tx(MODEXP, modexp_input, ABOVE);
    for (outcome, gas_limit) in [(short, before_frame + 2_999), (past_the_cap, ABOVE)] {
        assert!(
            matches!(
                &outcome.result,
                ExecutionResult::Halt {
                    reason: MegaHaltReason::Base(HaltReason::OutOfGas(OutOfGasError::Precompile)),
                    ..
                }
            ),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.limit_exceeded, None);
        assert_eq!(
            outcome.gas.gas_used + outcome.gas.reservoir_remaining,
            gas_limit,
            "the regular gas burned, the reservoir back"
        );
    }
}
