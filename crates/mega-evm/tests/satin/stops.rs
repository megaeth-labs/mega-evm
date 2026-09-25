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
    constants::{
        BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS, TX_DATA_LIMIT, TX_GAS_LIMIT_CAP,
    },
    system::ORACLE_CONTRACT_ADDRESS,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm, MegaHaltReason,
    MegaTransactionOutcome, FRAME_DATA_SHARE_DENOMINATOR, FRAME_DATA_SHARE_NUMERATOR,
    LOG_BASE_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        ADD, BALANCE, CALL, GAS, JUMP, JUMPDEST, LOG0, MLOAD, POP, PUSH0, RETURNDATACOPY,
        RETURNDATASIZE, REVERT, SLOAD, SSTORE, TIMESTAMP,
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
    detention::{context, work, BENEFICIARY},
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

/// Rewrites every frame result it sees end into `into`, and leaves its gas as it is.
struct Halter {
    into: InstructionResult,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Halter {
    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        _inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        outcome.result.result = self.into;
    }
}

/// An inspector that rewrites every frame result into a halt cannot turn a stop into a halt either:
/// under the latch every result is the stop, whatever produced it, so the transaction reports the
/// stop and bills what ran, as it does without the inspector, rather than burning its gas.
#[test]
fn test_an_inspector_cannot_turn_a_stop_into_a_halt() {
    let slot = one_slot();
    let halts = [
        InstructionResult::OutOfGas,
        InstructionResult::PrecompileOOG,
        InstructionResult::InvalidFEOpcode,
    ];
    for limit in Limit::ALL {
        for crossing in [0, 3] {
            for gas_limit in TIERS {
                let (configured, _) = limits_of(limit, crossing, slot);
                let plain = execute(chain(limit, crossing, false), configured, gas_limit);
                for into in halts {
                    let case =
                        format!("{limit:?} at depth {crossing}, gas limit {gas_limit}, {into:?}");
                    let mut evm = MegaEvm::new(
                        context(chain(limit, crossing, false)).with_tx_runtime_limits(configured),
                    )
                    .with_inspector(Halter { into });
                    let halted =
                        evm.execute_transaction(call(CALLER, A, U256::ZERO, gas_limit)).unwrap();
                    assert!(
                        matches!(&halted.result, ExecutionResult::Revert { .. }),
                        "{case}: {:?}",
                        halted.result
                    );
                    assert_eq!(halted.result.output(), plain.result.output(), "{case}");
                    assert_eq!(halted.limit_exceeded, plain.limit_exceeded, "{case}");
                    assert_eq!(halted.gas, plain.gas, "{case}");
                    assert_eq!(halted.usage, plain.usage, "{case}");
                }
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

/* ---------- a detained callee does not burn its callers' gas ---------- */

/// A callee at `depth` that reads the block's timestamp and then computes past the cap, below
/// callers that forward it all their gas and would write [`MARKER`] once it returned.
fn detained_callee_at(depth: usize) -> MemoryDatabase {
    let callee = work(log(BytecodeBuilder::default()).append_many([TIMESTAMP, POP]), ROUNDS);
    let callee = callee.sstore(MARKER, U256::from(1)).stop().build();
    (0..depth)
        .fold(MemoryDatabase::default(), |db, caller| {
            let code = call_all(BytecodeBuilder::default(), CHAIN[caller + 1]).append(POP);
            db.account_code(CHAIN[caller], code.sstore(MARKER, U256::from(1)).stop().build())
        })
        .account_code(CHAIN[depth], callee)
}

/// A callee that reads the block's timestamp and then computes past the cap does not make its
/// callers burn their gas: the stop bills the transaction its intrinsic gas plus the limit the read
/// set — its compute at the read plus the cap — and nothing of what the callers held, whatever the
/// gas limit, one call down or three, below the execution cap and above it.
#[test]
fn test_a_callee_that_reads_the_timestamp_does_not_make_its_callers_burn_their_gas() {
    for depth in [1, 3] {
        let bills: Vec<_> = [BELOW, ABOVE, 1_000_000_000]
            .into_iter()
            .map(|gas_limit| {
                let case = format!("depth {depth}, gas limit {gas_limit}");
                let intrinsic = intrinsic(gas_limit);
                let mut evm = MegaEvm::new(context(detained_callee_at(depth)));
                let outcome =
                    evm.execute_transaction(call(CALLER, A, U256::ZERO, gas_limit)).unwrap();
                let limit = evm.ctx().detention().compute_limit().expect("the read set a limit");
                let stop = LimitCheck::ExceedsLimit {
                    kind: LimitKind::ComputeGas,
                    limit,
                    used: limit,
                    frame_local: false,
                };
                assert_eq!(outcome.result.output(), Some(&stop.revert_data()), "{case}");
                assert!(!outcome.result.is_halt(), "{case}");
                assert_eq!(outcome.limit_exceeded, Some(stop), "{case}");
                // The read came after the callers' calls and the callee's log: its compute then is
                // theirs alone, a sliver of the cap.
                assert!(limit - CAP < 50_000, "{case}: compute at the read {}", limit - CAP);
                assert_eq!(outcome.gas.regular, intrinsic.gas.regular + limit, "{case}");
                assert_eq!(outcome.gas.gas_used, intrinsic.gas.gas_used + limit, "{case}");
                assert_eq!(
                    outcome.gas.reservoir_remaining, intrinsic.gas.reservoir_remaining,
                    "{case}: the reservoir comes back"
                );
                assert!(outcome.result.logs().is_empty(), "{case}");
                (outcome.gas.gas_used, outcome.gas.regular, outcome.gas.state, outcome.gas.history)
            })
            .collect();
        assert!(bills.windows(2).all(|pair| pair[0] == pair[1]), "depth {depth}: {bills:?}");
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

/* ---------- the legacy engine's rows ---------- */

// The legacy engine halted a transaction a limit stopped, and gave back what detention withheld
// through a rescue of the remaining gas. Here the stop is a revert that settles like any EIP-8037
// revert, and detention never moves gas out of a frame's tracker, so there is nothing to rescue.
// These rows keep the legacy scenarios and take their expectation from that rule.

/// A gas limit whose reservoir pays the state and history gas of a thousand fresh slots, so the
/// writes spend nothing but compute from the frame's regular gas.
const ROOMY: u64 = 1_000_000_000;

/// Appends writes of the fresh slots `1..=1,000`: 22,100,000 of compute, past the cap.
fn thousand_writes(code: BytecodeBuilder) -> BytecodeBuilder {
    (1..=1_000_u64).fold(code, |code, slot| code.sstore(U256::from(slot), U256::from(slot)))
}

/// Appends a call to the Oracle with `gas`, dropping its status. The Oracle's code in these rows
/// reads its slot zero, which is the read of its storage detention caps.
fn call_oracle(code: BytecodeBuilder, gas: Option<u64>) -> BytecodeBuilder {
    let code =
        code.append_many([PUSH0, PUSH0, PUSH0, PUSH0, PUSH0]).push_address(ORACLE_CONTRACT_ADDRESS);
    let code = match gas {
        Some(gas) => code.push_number(gas),
        None => code.append(GAS),
    };
    code.append_many([CALL, POP])
}

/// `db` with the Oracle's code a read of its slot zero.
fn with_oracle_read(db: MemoryDatabase) -> MemoryDatabase {
    db.account_code(
        ORACLE_CONTRACT_ADDRESS,
        BytecodeBuilder::default().append_many([PUSH0, SLOAD, POP]).stop().build(),
    )
}

/// Runs `db`'s `A` under `limits` with [`ROOMY`] gas and asserts the detention stop: a revert
/// carrying `MegaLimitExceeded(2, limit)` for the limit the reads set, billed the intrinsic gas
/// plus that limit and nothing of the gas the transaction had left. Returns the limit and what the
/// transaction read.
fn assert_detention_stop(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
) -> (u64, mega_evm::VolatileDataAccess) {
    let intrinsic = intrinsic(ROOMY);
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
    let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, ROOMY)).unwrap();
    let detention = evm.ctx().detention();
    let limit = detention.compute_limit().expect("a read set a limit");
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::ComputeGas,
        limit,
        used: limit,
        frame_local: false,
    };
    assert!(!outcome.result.is_halt(), "a revert, not a halt: {:?}", outcome.result);
    assert_eq!(outcome.result.output(), Some(&stop.revert_data()));
    assert_eq!(outcome.limit_exceeded, Some(stop));
    assert_eq!(outcome.gas.regular, intrinsic.gas.regular + limit);
    assert_eq!(outcome.gas.gas_used, intrinsic.gas.gas_used + limit);
    assert_eq!(outcome.gas.reservoir_remaining, intrinsic.gas.reservoir_remaining);
    assert!(outcome.result.logs().is_empty());
    (limit, detention.accessed())
}

/// A frame that reads the block's timestamp and then writes a thousand fresh slots stops at the
/// limit the read set, and bills that limit: the transaction's own frame, a child whose caller
/// never runs on, and a caller whose child did its work after the read.
#[test]
fn test_volatile_data_access_oog_does_not_consume_all_gas() {
    let code = thousand_writes(BytecodeBuilder::default().append_many([TIMESTAMP, POP]));
    let db = MemoryDatabase::default().account_code(A, code.stop().build());
    let (limit, accessed) = assert_detention_stop(db, EvmTxRuntimeLimits::default());
    assert!(limit - CAP < 1_000, "the read came first: {}", limit - CAP);
    assert_eq!(accessed, mega_evm::VolatileDataAccess::TIMESTAMP);
}

/// A child that reads the block's timestamp and then writes a thousand fresh slots stops the
/// transaction: its caller's code after the call never runs.
#[test]
fn test_nested_call_block_env_access_child_oog() {
    let child = thousand_writes(BytecodeBuilder::default().append_many([TIMESTAMP, POP]));
    let parent = call_all(BytecodeBuilder::default(), B).append(POP).sstore(MARKER, U256::from(1));
    let db = MemoryDatabase::default()
        .account_code(A, parent.stop().build())
        .account_code(B, child.stop().build());
    let (limit, _) = assert_detention_stop(db, EvmTxRuntimeLimits::default());
    assert!(limit - CAP < 10_000, "the child read near the start: {}", limit - CAP);
}

/// A frame that reads the block's timestamp, calls a child that does some work, then writes a
/// thousand fresh slots stops at the limit its read set: the child's work counts towards it.
#[test]
fn test_parent_block_env_access_oog_after_nested_call() {
    let child =
        BytecodeBuilder::default().push_number(1_u8).push_number(2_u8).append(ADD).append(POP);
    let parent = call_all(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), B).append(POP);
    let db = MemoryDatabase::default()
        .account_code(A, thousand_writes(parent).stop().build())
        .account_code(B, child.stop().build());
    let (limit, _) = assert_detention_stop(db, EvmTxRuntimeLimits::default());
    assert!(limit - CAP < 1_000, "the read came first: {}", limit - CAP);
}

/// A call to the Oracle that reads its storage, then a thousand fresh slots: the transaction stops
/// at the compute at the read plus the Oracle's cap, the spec's 20,000,000, and bills it.
#[test]
fn test_an_oracle_read_holds_the_transaction_to_its_cap() {
    let code = thousand_writes(call_oracle(BytecodeBuilder::default(), None));
    let db = with_oracle_read(MemoryDatabase::default().account_code(A, code.stop().build()));
    let (limit, accessed) = assert_detention_stop(db, EvmTxRuntimeLimits::default());
    assert!(limit - ORACLE_ACCESS_COMPUTE_GAS < 10_000, "{}", limit - ORACLE_ACCESS_COMPUTE_GAS);
    assert_eq!(accessed, mega_evm::VolatileDataAccess::ORACLE);
}

/// The same with the Oracle called on 65,535 gas: the stop bills the limit, not the gas limit.
#[test]
fn test_oracle_volatile_data_access_oog_does_not_consume_all_gas() {
    let code = thousand_writes(call_oracle(BytecodeBuilder::default(), Some(0xffff)));
    let db = with_oracle_read(MemoryDatabase::default().account_code(A, code.stop().build()));
    assert_detention_stop(db, EvmTxRuntimeLimits::default());
}

/// A contract that calls one that reads the Oracle's storage and then writes a thousand fresh
/// slots: the stop reaches the outer contract, whose reads after the call never run.
#[test]
fn test_parent_runs_out_of_gas_after_oracle_access() {
    let middle = thousand_writes(call_oracle(BytecodeBuilder::default(), None));
    let outer =
        call_all(BytecodeBuilder::default(), B).append(POP).append_many([PUSH0, SLOAD, POP]);
    let db = MemoryDatabase::default()
        .account_code(A, outer.stop().build())
        .account_code(B, middle.stop().build());
    let (_, accessed) = assert_detention_stop(with_oracle_read(db), EvmTxRuntimeLimits::default());
    assert_eq!(accessed, mega_evm::VolatileDataAccess::ORACLE);
}

/// A read of the block's timestamp, then of the Oracle's storage under a lower cap: the most
/// restrictive limit binds, the Oracle's, and the stop bills it.
#[test]
fn test_both_volatile_data_access_oog_does_not_consume_all_gas() {
    const ORACLE_CAP: u64 = 1_000_000;
    let code = call_oracle(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), Some(0xffff));
    let db = MemoryDatabase::default().account_code(A, thousand_writes(code).stop().build());
    let limits = EvmTxRuntimeLimits::default().with_oracle_access_compute_gas_limit(ORACLE_CAP);
    let (limit, accessed) = assert_detention_stop(with_oracle_read(db), limits);
    assert!(limit - ORACLE_CAP < 10_000, "the Oracle's read binds: {limit}");
    assert_eq!(
        accessed,
        mega_evm::VolatileDataAccess::TIMESTAMP | mega_evm::VolatileDataAccess::ORACLE
    );
}

/// A read of the block's timestamp, then compute past the cap: the stop comes at the compute at the
/// read plus the cap.
#[test]
fn test_volatile_access_post_access_cap_enforced() {
    let code = work(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), ROUNDS);
    let db = MemoryDatabase::default().account_code(A, code.stop().build());
    let (limit, _) = assert_detention_stop(db, EvmTxRuntimeLimits::default());
    assert!(limit - CAP < 1_000, "{}", limit - CAP);
}

/// A read whose limit is past what the transaction's gas reaches does not bind: the frame runs out
/// of its own gas first, and that is an ordinary out-of-gas, which halts and burns the gas, not the
/// stop. Ten million of compute, the read, then sixteen million more, on 25,000,000 of gas.
#[test]
fn test_non_binding_detention_reports_a_normal_out_of_gas() {
    use revm::context::result::HaltReason;

    let code = work(BytecodeBuilder::default(), 3_300).append_many([TIMESTAMP, POP]);
    let db = MemoryDatabase::default().account_code(A, work(code, 5_200).stop().build());
    let gas_limit = 25_000_000;
    let mut evm = MegaEvm::new(context(db));
    let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, gas_limit)).unwrap();
    let limit = evm.ctx().detention().compute_limit().expect("the read set a limit");
    assert!(limit > gas_limit, "the read's limit is past the gas: {limit}");
    assert!(
        matches!(
            &outcome.result,
            ExecutionResult::Halt { reason: MegaHaltReason::Base(HaltReason::OutOfGas(_)), .. }
        ),
        "{:?}",
        outcome.result
    );
    assert_eq!(outcome.limit_exceeded, None);
    assert_eq!(outcome.gas.gas_used, gas_limit, "an out-of-gas burns the gas");
}

/// A transaction that reads the beneficiary's balance with far more gas than the cap succeeds and
/// pays for what it ran, the same as with detention off: while it ran, its frame could spend only
/// the cap, and the rest was withheld, never spent.
#[test]
fn test_detained_gas_is_restored() {
    /// The spendable and withheld parts of the frame's gas after each step.
    #[derive(Default)]
    struct Parts(Vec<(u64, u64)>);
    impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Parts {
        fn step_end(
            &mut self,
            interp: &mut Interpreter<EthInterpreter>,
            _context: &mut MegaContext<DB>,
        ) {
            self.0.push((interp.gas.spendable(), interp.gas.withheld()));
        }
    }

    let code = BytecodeBuilder::default().push_address(BENEFICIARY).append_many([BALANCE, POP]);
    let db = MemoryDatabase::default().account_code(A, code.stop().build());
    let tx = || call(CALLER, A, U256::ZERO, ABOVE);
    let mut evm = MegaEvm::new(context(db.clone())).with_inspector(Parts::default());
    let detained = evm.execute_transaction(tx()).unwrap();
    assert_eq!(evm.ctx().detention().accessed(), mega_evm::VolatileDataAccess::BENEFICIARY_BALANCE);
    let parts = &evm.inspector().0;
    let (spendable, withheld) = parts[1];
    assert!(spendable <= CAP && withheld > TX_GAS_LIMIT_CAP - 2 * CAP, "after the read: {parts:?}");

    let undetained =
        MegaEvm::new(context(db).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits()))
            .execute_transaction(tx())
            .unwrap();
    assert!(detained.result.is_success(), "{:?}", detained.result);
    assert_eq!(detained.gas, undetained.gas, "the withheld gas was never spent");
    assert!(detained.gas.gas_used < 50_000, "{}", detained.gas.gas_used);
}

/// A body over the transaction's data-size limit stops the transaction before its first frame: a
/// revert carrying the stop, not a halt, billed the intrinsic gas alone.
#[test]
fn test_data_limit_just_exceed() {
    let limit = TX_BODY_SIZE - 1;
    let db = || MemoryDatabase::default().account_code(A, writer(1));
    let outcome =
        execute(db(), EvmTxRuntimeLimits::default().with_tx_data_size_limit(limit), BELOW);
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit,
        used: TX_BODY_SIZE,
        frame_local: false,
    };
    assert!(!outcome.result.is_halt(), "{:?}", outcome.result);
    assert_eq!(outcome.result.output(), Some(&stop.revert_data()));
    assert_eq!(outcome.limit_exceeded, Some(stop));
    assert_eq!(outcome.usage, LimitUsage { data_size: TX_BODY_SIZE, write_records: 0 });
    assert_eq!(outcome.gas.gas_used, intrinsic(BELOW).gas.gas_used, "nothing ran");
}

/// A library's write, one call down, crosses a limit one byte above the body: the stop is a
/// revert, and the transaction is billed what ran.
#[test]
fn test_data_limit_exceed_in_nested_call() {
    let limit = TX_BODY_SIZE + 1;
    let library = BytecodeBuilder::default().push_number(1_u8).append_many([PUSH0, SLOAD, SSTORE]);
    let db = MemoryDatabase::default()
        .account_code(A, call_all(BytecodeBuilder::default(), B).append(POP).stop().build())
        .account_code(B, library.stop().build());
    let outcome = execute(db, EvmTxRuntimeLimits::default().with_tx_data_size_limit(limit), BELOW);
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit,
        used: TX_BODY_SIZE + WRITE_RECORD_SIZE,
        frame_local: false,
    };
    assert!(!outcome.result.is_halt(), "{:?}", outcome.result);
    assert_eq!(outcome.result.output(), Some(&stop.revert_data()));
    assert_eq!(outcome.limit_exceeded, Some(stop));
    assert!(outcome.gas.gas_used < 200_000, "the stop burns nothing: {}", outcome.gas.gas_used);
}

/// A value transfer to a contract that writes a slot, under a limit one byte above the body: the
/// first frame's start crosses it, so the value never moves and the contract never runs.
#[test]
fn test_state_revert_when_exceeding_limit() {
    let db = MemoryDatabase::default()
        .account_code(A, BytecodeBuilder::default().sstore(U256::ZERO, U256::from(1)).build())
        .account_balance(CALLER, U256::from(10_000));
    let outcome = MegaEvm::new(context(db).with_tx_runtime_limits(
        EvmTxRuntimeLimits::default().with_tx_data_size_limit(TX_BODY_SIZE + 1),
    ))
    .execute_transaction(call(CALLER, A, U256::from(100), BELOW))
    .unwrap();
    assert!(!outcome.result.is_halt(), "{:?}", outcome.result);
    assert!(matches!(
        outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, frame_local: false, .. })
    ));
    assert_eq!(outcome.usage, LimitUsage { data_size: TX_BODY_SIZE, write_records: 0 });
    assert!(outcome.state.get(&A).is_none_or(|a| a.storage.is_empty() && a.info.balance.is_zero()));
    assert_eq!(outcome.state[&CALLER].info.balance, U256::from(10_000), "the value did not move");
}

/// Data size is held before write records: a body that crosses the data-size limit is its stop
/// whatever the KV limit, and so is a write that crosses both.
#[test]
fn test_check_limit_priority_data_size_before_kv_update() {
    let db = || MemoryDatabase::default().account_code(A, writer(1));
    let both = |data_size: u64| {
        EvmTxRuntimeLimits::default().with_tx_data_size_limit(data_size).with_tx_kv_update_limit(0)
    };
    for (data_size, used) in [
        (1, TX_BODY_SIZE),
        (TX_BODY_SIZE + LOG_BASE_SIZE, TX_BODY_SIZE + LOG_BASE_SIZE + WRITE_RECORD_SIZE),
    ] {
        let outcome = execute(db(), both(data_size), BELOW);
        assert_eq!(
            outcome.limit_exceeded,
            Some(LimitCheck::ExceedsLimit {
                kind: LimitKind::DataSize,
                limit: data_size,
                used,
                frame_local: false,
            }),
            "data size {data_size}"
        );
    }
}

/// A transaction detained from its start — its sender is the block's beneficiary — whose body
/// crosses the data-size limit is the data-size stop, billed its intrinsic gas: detention adds
/// nothing to what it pays.
#[test]
fn test_detention_plus_intrinsic_data_size_overflow() {
    let db =
        || MemoryDatabase::default().account_code(A, BytecodeBuilder::default().stop().build());
    let run = |limits| {
        MegaEvm::new(context(db()).with_tx_runtime_limits(limits))
            .execute_transaction(call(BENEFICIARY, A, U256::ZERO, ROOMY))
            .unwrap()
    };
    let unlimited = run(EvmTxRuntimeLimits::default());
    let stopped = run(EvmTxRuntimeLimits::default().with_tx_data_size_limit(100));
    assert!(!stopped.result.is_halt(), "{:?}", stopped.result);
    assert_eq!(
        stopped.limit_exceeded,
        Some(LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: 100,
            used: TX_BODY_SIZE,
            frame_local: false,
        })
    );
    assert_eq!(stopped.gas, unlimited.gas, "the intrinsic gas alone, the reservoir back");
}

/// The transaction's own frame crossing its frame budget — a frame cap of a hundred records' bytes,
/// crossed by the 101st write — reverts it alone: a revert, not a halt, and no latch. The body is
/// all the transaction keeps.
#[test]
fn test_data_size_top_level_exceed_is_frame_local_revert() {
    let cap = 100 * WRITE_RECORD_SIZE;
    let code = (1..=101_u64).fold(BytecodeBuilder::default(), |code, slot| {
        code.sstore(U256::from(slot), U256::from(1))
    });
    let db = MemoryDatabase::default().account_code(A, code.stop().build());
    let outcome = execute(db, EvmTxRuntimeLimits::default().with_frame_data_size_limit(cap), BELOW);
    let stop = LimitCheck::ExceedsLimit {
        kind: LimitKind::DataSize,
        limit: cap,
        used: 0,
        frame_local: true,
    };
    assert!(!outcome.result.is_halt(), "{:?}", outcome.result);
    assert_eq!(outcome.result.output(), Some(&stop.revert_data()));
    assert_eq!(outcome.limit_exceeded, None, "a frame budget latches nothing");
    assert_eq!(outcome.usage, LimitUsage { data_size: TX_BODY_SIZE, write_records: 0 });
}
