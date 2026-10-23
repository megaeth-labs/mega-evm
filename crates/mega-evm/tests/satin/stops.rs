//! The revert-class stop, pinned as a matrix.
//!
//! A limit — data size, KV updates, state growth or gas detention's compute limit — stops the
//! frame that crosses it with a revert: the frame reverts with `MegaLimitExceeded(kind, limit)` and
//! keeps nothing it did, and its caller resumes as after any revert. A crossing the transaction's
//! own frame makes is the transaction's result, which settles like an EIP-8037 revert: the sender
//! pays for what ran, and its unspent regular gas and the reservoir come back. A frame budget stops
//! its frame the same way. A real out-of-gas and a precompile given less than its price still halt,
//! and burn what their frame was given.
//!
//! The matrix crosses each limit in the transaction's own frame and in a frame three calls below
//! it, below the execution cap and above it, where a reservoir pays the body first, without an
//! inspector and under one that records every frame's end. The callers of a frame three calls
//! down catch its stop and end at no cost, which is how a caller finishes within a compute limit
//! its callee crossed: the transaction succeeds, keeping what the callers did and nothing of the
//! frame that crossed. What ran is read off a twin of each transaction: the same frames without
//! limits, the one that crossed reverting where it crossed. The twin settles its revert itself, so
//! it bills exactly what the stop should, plus the two pushes of its revert, and keeps what the
//! stopped transaction keeps.
//!
//! A column of creations revived by an inspector that rewrites every frame result into a success
//! shows that a revived stop is taken as given — a success that keeps what it counted before the
//! stop — and is no stop.
//!
//! The last cells cross a limit three calls down with a frame answered without running — a
//! precompile run on what the compute limit leaves it, and a start whose records cross the
//! data-size limit before the frame is built — and the callers catch the stop the same way. An
//! inspector's answer past that allowance is taken as given, and is no stop.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{
        BLOCK_ENV_ACCESS_COMPUTE_GAS, ORACLE_ACCESS_COMPUTE_GAS, TX_DATA_LIMIT, TX_GAS_LIMIT_CAP,
    },
    system::ORACLE_CONTRACT_ADDRESS,
    test_utils::{BytecodeBuilder, MemoryDatabase},
    EvmTxRuntimeLimits, LimitCheck, LimitKind, LimitStop, LimitUsage, MegaContext, MegaEvm,
    MegaHaltReason, MegaTransactionOutcome, FRAME_DATA_SHARE_DENOMINATOR,
    FRAME_DATA_SHARE_NUMERATOR, LOG_BASE_SIZE, TRANSFER_LOG_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        ADD, BALANCE, CALL, GAS, LOG0, MLOAD, POP, PUSH0, RETURNDATACOPY, RETURNDATASIZE, REVERT,
        SLOAD, SSTORE, TIMESTAMP,
    },
    context::result::ExecutionResult,
    context_interface::cfg::GasId,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, Gas, InstructionResult, Interpreter, InterpreterResult,
    },
    primitives::HashMap,
    Database, Inspector,
};

use crate::{
    common::{body_history, call, call_with_data, create},
    detention::{cold_access, context, fresh_write, fresh_write_spill, work, Charges, BENEFICIARY},
    withheld_gas::{priced, Runs, PRICED},
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
/// The slot a frame writes once it runs on after its crossing: a write of it is an instruction
/// run after the stop.
const MARKER: U256 = U256::from_limbs([0xaa, 0, 0, 0]);
/// Rounds of the compute loop a frame runs after it read the block's timestamp: over 21,000,000
/// of compute, past the cap.
const ROUNDS: u32 = 7_000;
/// What the two pushes before the twin's revert cost.
const TWIN_REVERT: u64 = 4;

/// A limit the matrix crosses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Limit {
    DataSize,
    KvUpdate,
    StateGrowth,
    Compute,
}

impl Limit {
    const ALL: [Self; 4] = [Self::DataSize, Self::KvUpdate, Self::StateGrowth, Self::Compute];

    const fn kind(self) -> LimitKind {
        match self {
            Self::DataSize => LimitKind::DataSize,
            Self::KvUpdate => LimitKind::KvUpdate,
            Self::StateGrowth => LimitKind::StateGrowth,
            Self::Compute => LimitKind::ComputeGas,
        }
    }

    /// Whether the limit is one the layer counts, crossed by a write, rather than gas detention's.
    const fn is_counted(self) -> bool {
        !matches!(self, Self::Compute)
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
/// Every frame logs first, and the transaction's own frame writes [`OWN_SLOTS`] fresh slots. Every
/// frame above the one at `crossing` then calls the next and stops, which costs nothing: it
/// catches what the call returned and ends within any limit. The frame at `crossing` crosses the
/// limit — a fresh slot's write, or a read of the block's timestamp and more compute than the cap
/// — and would then write [`MARKER`] and log again, which a stopped frame never runs; its twin
/// reverts where it crossed, right after the read for the compute limit.
fn frame_code(limit: Limit, depth: usize, crossing: usize, twin: bool) -> Bytes {
    let mut code = log(BytecodeBuilder::default());
    if depth == 0 {
        for slot in 1..=OWN_SLOTS {
            code = code.sstore(U256::from(slot), U256::from(1));
        }
    }
    if depth < crossing {
        return call_all(code, CHAIN[depth + 1]).stop().build();
    }
    let code = match limit {
        Limit::Compute if twin => code.append(TIMESTAMP),
        Limit::Compute => work(code.append_many([TIMESTAMP, POP]), ROUNDS),
        Limit::DataSize | Limit::KvUpdate | Limit::StateGrowth => {
            code.sstore(CROSSING_SLOT, U256::from(1))
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

/// Records every step and the result of every frame it sees end, and, made to rewrite
/// ([`rewriting`](Self::rewriting)), then rewrites that result into a success with no output — a
/// creation's too, which revives a creation that failed: a tool's inspector.
#[derive(Clone, Default)]
struct FrameRecorder {
    /// Whether it rewrites the results it records.
    rewrites: bool,
    /// Every step: the frame it ran in, its opcode, and the slot it named when it was an `SSTORE`.
    steps: Vec<(Address, u8, Option<U256>)>,
    /// The calls that ended, deepest first, with the result each had before any rewrite.
    ended: Vec<(Address, InstructionResult, Bytes)>,
    /// The creations that ended, with the result each had before any rewrite.
    created: Vec<(InstructionResult, Bytes)>,
}

impl FrameRecorder {
    /// One that records and rewrites nothing.
    fn recording() -> Self {
        Self::default()
    }

    /// One that records, then rewrites every result into a success.
    fn rewriting() -> Self {
        Self { rewrites: true, ..Self::default() }
    }

    /// How many writes of [`MARKER`] ran.
    fn marker_writes(&self) -> usize {
        self.steps.iter().filter(|(_, _, slot)| *slot == Some(MARKER)).count()
    }

    /// The last step that ran in `frame`.
    fn last_in(&self, frame: Address) -> Option<(u8, Option<U256>)> {
        self.steps.iter().rev().find(|step| step.0 == frame).map(|step| (step.1, step.2))
    }
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for FrameRecorder {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        let opcode = interp.bytecode.opcode();
        let slot = (opcode == SSTORE).then(|| interp.stack.peek(0).expect("SSTORE takes a slot"));
        self.steps.push((interp.input.target_address, opcode, slot));
    }

    fn call_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let result = &mut outcome.result;
        self.ended.push((inputs.target_address, result.result, result.output.clone()));
        if self.rewrites {
            result.result = InstructionResult::Stop;
            result.output = Bytes::new();
        }
    }

    fn create_end(
        &mut self,
        _context: &mut MegaContext<DB>,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        let result = &mut outcome.result;
        self.created.push((result.result, result.output.clone()));
        if self.rewrites {
            result.result = InstructionResult::Return;
            result.output = Bytes::new();
        }
    }
}

/// The slots `account` changed in `outcome`, with the values it left in them.
fn changed(outcome: &MegaTransactionOutcome, account: Address) -> Vec<(U256, U256)> {
    let mut changed: Vec<_> = outcome
        .state
        .get(&account)
        .map(|account| {
            account
                .storage
                .iter()
                .filter(|(_, slot)| slot.is_changed())
                .map(|(key, slot)| (*key, slot.present_value()))
                .collect()
        })
        .unwrap_or_default();
    changed.sort();
    changed
}

/// One row of the matrix, both columns: `limit` crossed by the frame at `crossing` of a
/// transaction with `gas_limit`, without an inspector and under one that records every step and
/// frame end.
fn assert_cell(limit: Limit, crossing: usize, gas_limit: u64) {
    let row = format!("{limit:?} at depth {crossing}, gas limit {gas_limit}");
    let caught = crossing > 0;
    let intrinsic = intrinsic(gas_limit);

    // The twin: the same frames, the one that crossed reverting where it crossed, its callers
    // catching the revert and ending.
    let twin = execute(chain(limit, crossing, true), EvmTxRuntimeLimits::no_limits(), gas_limit);
    if caught {
        assert!(twin.result.is_success(), "{row}: the twin's callers end: {:?}", twin.result);
    } else {
        assert!(
            matches!(&twin.result, ExecutionResult::Revert { output, .. } if output.is_empty()),
            "{row}: the twin reverts: {:?}",
            twin.result
        );
    }
    assert_eq!(twin.limit_exceeded, None, "{row}");
    // The regular gas the frames spent up to the crossing: for the compute limit, up to the read.
    let before_crossing = twin.gas.regular - intrinsic.gas.regular - TWIN_REVERT;

    // The limit crossed, and the usage that crossed it. A counted limit is crossed by the crossing
    // frame's write, which takes the transaction one over its limit and the frame over its own
    // budget, which the stop names. The compute limit is set by the read, at the compute it ran
    // to plus the cap, and the stop reports the compute up to the charge that crosses it: after
    // the read's `POP`, one of the work's.
    let (configured, crossing_of) = limits_of(limit, crossing);
    let (value, used, frame_local, tx_limit) = match crossing_of {
        Some(Crossing { tx_limit, budget, used }) => (budget, used, true, tx_limit),
        None => {
            let left = Charges::default().then(&[2]).work(ROUNDS, 0).left(CAP);
            let value = before_crossing + CAP;
            (value, value - left, false, value)
        }
    };
    let stop = LimitCheck::ExceedsLimit { kind: limit.kind(), limit: value, used, frame_local };
    // What ran: up to the crossing, and for the compute limit up to the charge that crosses it.
    // The callers that catch the stop end at no cost.
    let ran = if limit == Limit::Compute { used } else { before_crossing };

    let plain = execute(chain(limit, crossing, false), configured, gas_limit);
    let mut evm =
        MegaEvm::new(context(chain(limit, crossing, false)).with_tx_runtime_limits(configured))
            .with_inspector(FrameRecorder::recording());
    let recorded = evm.execute_transaction(call(CALLER, A, U256::ZERO, gas_limit)).unwrap();
    let recorder = evm.inspector();

    for (column, outcome) in [("plain", &plain), ("recorded", &recorded)] {
        let cell = format!("{row}, {column}");
        if caught {
            // The receipt: the callers caught the stop and ended.
            assert!(outcome.result.is_success(), "{cell}: {:?}", outcome.result);
            assert_eq!(outcome.limit_exceeded, None, "{cell}: a stop below is no stop of it");
            assert_eq!(outcome.limit_stop(), None, "{cell}: nothing for an RPC to map");
        } else {
            // The receipt: a revert carrying the stop, which the outcome names.
            match &outcome.result {
                ExecutionResult::Revert { output, .. } => {
                    assert_eq!(output, &stop.revert_data(), "{cell}: the stop's revert data")
                }
                other => panic!("{cell}: expected the stop, got {other:?}"),
            }
            assert_eq!(outcome.limit_exceeded, Some(stop), "{cell}");
            assert_eq!(
                outcome.limit_stop(),
                Some(LimitStop { kind: limit.kind(), limit: value, used }),
                "{cell}: what an RPC maps the stop to"
            );
        }

        // The bill: intrinsic gas plus what ran on the regular ledger; on every other ledger what
        // the twin keeps, which is what the callers did before the crossing and nothing of the
        // frame that crossed: for the transaction's own frame, the body alone.
        assert_eq!(outcome.gas.regular, intrinsic.gas.regular + ran, "{cell}: regular");
        assert_eq!(outcome.gas.state, twin.gas.state, "{cell}: state");
        assert_eq!(outcome.gas.history, twin.gas.history, "{cell}: history");
        assert_eq!(outcome.gas.history_bytes, twin.gas.history_bytes, "{cell}");
        assert_eq!(
            outcome.gas.reservoir_remaining, twin.gas.reservoir_remaining,
            "{cell}: the reservoir comes back, less what was kept"
        );
        assert_eq!(
            outcome.gas.gas_used,
            twin.gas.gas_used - TWIN_REVERT - before_crossing + ran,
            "{cell}: gas used"
        );
        if !caught {
            assert_eq!(outcome.gas.state, 0, "{cell}: a stop keeps no state gas");
            assert_eq!(outcome.gas.history, intrinsic.gas.history, "{cell}: the body's history");
        }

        // What the frame that crossed did is not kept; what its callers did is.
        assert_eq!(outcome.result.logs(), twin.result.logs(), "{cell}: the callers' logs alone");
        for account in &CHAIN[..=crossing] {
            assert_eq!(changed(outcome, *account), changed(&twin, *account), "{cell}: {account}");
        }
        assert_eq!(outcome.usage, twin.usage, "{cell}: what is kept");
        match limit {
            Limit::DataSize => assert!(outcome.usage.data_size <= tx_limit, "{cell}: within it"),
            Limit::KvUpdate => {
                assert!(outcome.usage.write_records <= tx_limit, "{cell}: within it")
            }
            Limit::StateGrowth => {
                assert!(outcome.usage.state_growth <= tx_limit, "{cell}: within it")
            }
            Limit::Compute => {
                let compute = outcome.gas.regular - intrinsic.gas.regular;
                assert!(compute <= value, "{cell}: within the limit, {compute} of {value}");
            }
        }
        if !caught {
            assert_eq!(
                outcome.usage,
                LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 },
                "{cell}: the body stays"
            );
        }
    }
    // The recorder changes nothing. No instruction ran after the crossing in the frame that
    // crossed, and every caller resumed and ended.
    assert_eq!(recorded.gas, plain.gas, "{row}");
    assert_eq!(recorder.marker_writes(), 0, "{row}: no frame ran on after the crossing");
    if limit.is_counted() {
        assert_eq!(
            recorder.last_in(CHAIN[crossing]),
            Some((SSTORE, Some(CROSSING_SLOT))),
            "{row}: the crossing write was the crossing frame's last step"
        );
    }
    let ended: Vec<_> = CHAIN[..=crossing]
        .iter()
        .rev()
        .map(|frame| {
            if *frame == CHAIN[crossing] {
                (*frame, InstructionResult::Revert, stop.revert_data())
            } else {
                (*frame, InstructionResult::Stop, Bytes::new())
            }
        })
        .collect();
    assert_eq!(
        recorder.ended, ended,
        "{row}: the frame that crossed returned the stop, and every caller ended, deepest first"
    );
}

/// Every limit, crossed by the transaction's own frame and by a frame three calls below it, stops
/// the frame that crosses it — below the execution cap and above it, with and without an
/// inspector that records every step. Three calls down, the callers catch the stop and end, and
/// the transaction succeeds, keeping what they did and nothing of the frame that crossed; at the
/// transaction's own frame the stop is the transaction's result.
///
/// Rules: [S11.1], [S11.2], [S11.3], [S11.4], [S11.6], [S11.7], [S11.8]. Independence: constants
/// for every limit and the stop it reports, and engine-derived for the bill, which a twin run sets.
#[test]
fn test_every_limit_stops_the_frame_that_crosses_it_at_every_depth_and_tier() {
    for limit in Limit::ALL {
        for crossing in [0, 3] {
            for gas_limit in TIERS {
                assert_cell(limit, crossing, gas_limit);
            }
        }
    }
}

/// Rewrites every call result it sees end into `into`, and leaves its gas as it is.
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

/// An inspector that rewrites every frame result into a halt gets the halt: a stop is a revert of
/// the frame that crossed, which the rewrite turns into a halt like any other result, so the
/// transaction halts, counts nothing but its body, and reports no stop, whether the frame that
/// crossed was its own or three calls below, under every limit and at either tier. The stop is
/// in the inspector's hands as every result is. Each frame above the one that crossed succeeded,
/// and revm committed its journal before the inspector rewrote its result, so the log it made is
/// in the receipt, as revm leaves it.
///
/// Rules: [S11.1], [S11.6]. Independence: constants — the body is `TX_BODY_SIZE`.
#[test]
fn test_an_inspector_that_rewrites_a_stop_into_a_halt_gets_the_halt() {
    let halts = [
        InstructionResult::OutOfGas,
        InstructionResult::PrecompileOOG,
        InstructionResult::InvalidFEOpcode,
    ];
    for limit in Limit::ALL {
        for crossing in [0, 3] {
            for gas_limit in TIERS {
                let (configured, _) = limits_of(limit, crossing);
                for into in halts {
                    let case =
                        format!("{limit:?} at depth {crossing}, gas limit {gas_limit}, {into:?}");
                    let mut evm = MegaEvm::new(
                        context(chain(limit, crossing, false)).with_tx_runtime_limits(configured),
                    )
                    .with_inspector(Halter { into });
                    let halted =
                        evm.execute_transaction(call(CALLER, A, U256::ZERO, gas_limit)).unwrap();
                    assert!(halted.result.is_halt(), "{case}: {:?}", halted.result);
                    assert_eq!(halted.limit_exceeded, None, "{case}: the result is no stop");
                    assert_eq!(
                        halted.result.logs().len(),
                        crossing,
                        "{case}: the logs of the frames above the one that crossed"
                    );
                    assert_eq!(
                        halted.usage,
                        LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 },
                        "{case}: the body alone"
                    );
                }
            }
        }
    }
}

/// Where a counted limit is crossed: the transaction's limit, the budget of the frame that crosses
/// it, which the stop names, and what that frame used of it with the write that crossed.
#[derive(Clone, Copy, Debug)]
struct Crossing {
    tx_limit: u64,
    budget: u64,
    used: u64,
}

/// The limits a cell runs under, and where a counted limit is crossed, which is known before the
/// run: every dimension but the compute limit, which the read sets.
///
/// The transaction's limit is one short of what the chain keeps with the crossing write. The
/// transaction's own frame gets what the body leaves of it, and each frame below 98% of what its
/// caller has left at its call: `A` has logged and written its [`OWN_SLOTS`] by then, `B` and `C`
/// have logged. The crossing frame's write crosses its budget, the transaction's limit with it.
fn limits_of(limit: Limit, crossing: usize) -> (EvmTxRuntimeLimits, Option<Crossing>) {
    let frames = crossing as u64 + 1;
    // What the transaction's own frame keeps before it calls or crosses, what a frame below keeps
    // before it calls or crosses, and what the crossing write adds, in the limit's unit.
    let (own, below, write) = match limit {
        Limit::DataSize => (LOG_BASE_SIZE + OWN_SLOTS * WRITE_RECORD_SIZE, LOG_BASE_SIZE, 40),
        // The fresh slots are new slots, one each, as they are write records.
        Limit::KvUpdate | Limit::StateGrowth => (OWN_SLOTS, 0, 1),
        Limit::Compute => return (EvmTxRuntimeLimits::default(), None),
    };
    let (limits, tx_limit, outside) = match limit {
        Limit::DataSize => {
            let value = TX_BODY_SIZE + own + (frames - 1) * below + write - 1;
            (EvmTxRuntimeLimits::default().with_tx_data_size_limit(value), value, TX_BODY_SIZE)
        }
        Limit::KvUpdate => {
            let value = own + (frames - 1) * below + write - 1;
            (EvmTxRuntimeLimits::default().with_tx_kv_update_limit(value), value, 0)
        }
        _ => {
            let value = own + (frames - 1) * below + write - 1;
            (EvmTxRuntimeLimits::default().with_tx_state_growth_limit(value), value, 0)
        }
    };
    let share = |left: u64| left * FRAME_DATA_SHARE_NUMERATOR / FRAME_DATA_SHARE_DENOMINATOR;
    let mut budget = tx_limit - outside;
    let mut kept = own;
    for _ in 0..crossing {
        budget = share(budget - kept);
        kept = below;
    }
    (limits, Some(Crossing { tx_limit, budget, used: kept + write }))
}

/* ---------- creations under an inspector that revives them ---------- */

/// Where a creation meets the limit it crosses.
#[derive(Clone, Copy, Debug)]
enum Creation {
    /// The transaction is a creation, and its init code crosses the limit.
    Transaction,
    /// The transaction's own frame creates a contract whose init code crosses the limit.
    Nested,
    /// The transaction's own frame creates a contract whose start crosses the limit — its write
    /// records, or the new account it adds — so its init code never runs.
    NestedStart,
}

/// Init code that logs, then crosses `limit`: a fresh slot's write, or a read of the block's
/// timestamp and more compute than the cap.
fn crossing_init_code(limit: Limit) -> Vec<u8> {
    let code = log(BytecodeBuilder::default());
    let code = match limit {
        Limit::Compute => work(code.append_many([TIMESTAMP, POP]), ROUNDS),
        _ => code.sstore(CROSSING_SLOT, U256::from(1)),
    };
    code.stop().build_vec()
}

/// The database and the transaction of `creation`, for `limit`.
fn creation_run(
    creation: Creation,
    limit: Limit,
    gas_limit: u64,
) -> (MemoryDatabase, mega_evm::MegaTransaction) {
    let init_code = crossing_init_code(limit);
    match creation {
        Creation::Transaction => {
            (MemoryDatabase::default(), create(CALLER, init_code.into(), gas_limit))
        }
        Creation::Nested | Creation::NestedStart => {
            let code = BytecodeBuilder::default().create(U256::ZERO, init_code).append(POP);
            let db = MemoryDatabase::default().account_code(A, code.stop().build());
            (db, call(CALLER, A, U256::ZERO, gas_limit))
        }
    }
}

/// `creation` for `limit` run without limits, where it succeeds.
fn unlimited_creation_run(
    creation: Creation,
    limit: Limit,
    gas_limit: u64,
) -> MegaTransactionOutcome {
    let (db, tx) = creation_run(creation, limit, gas_limit);
    let unlimited =
        MegaEvm::new(context(db).with_tx_runtime_limits(EvmTxRuntimeLimits::no_limits()))
            .execute_transaction(tx)
            .expect("the transaction is valid");
    assert!(unlimited.result.is_success(), "{creation:?}, {limit:?}: {:?}", unlimited.result);
    unlimited
}

/// The limits under which `creation` crosses `limit` by one unit at the place it names.
///
/// Where the init code crosses, the limit is one unit below what the whole transaction uses without
/// limits, whose last write is the init code's; the compute limit is the spec's cap. For a nested
/// creation's data size it is one record below: a byte below, the creation's share would hold all
/// it writes, and the stop would be its creator's once its nonce record is counted; a record below,
/// the share falls short of the creation's last write by the part its creator keeps. Where the
/// start crosses, the limit is what the transaction used before it and the creator's nonce record:
/// the creation's share of that falls short of its start, and the creator keeps its record once
/// the creation returned.
fn creation_limits(creation: Creation, limit: Limit, gas_limit: u64) -> EvmTxRuntimeLimits {
    let limits = EvmTxRuntimeLimits::default();
    if matches!(creation, Creation::NestedStart) {
        return match limit {
            Limit::DataSize => limits.with_tx_data_size_limit(TX_BODY_SIZE + WRITE_RECORD_SIZE),
            Limit::KvUpdate => limits.with_tx_kv_update_limit(1),
            // `A` exists, so its nonce record is no new account.
            Limit::StateGrowth => limits.with_tx_state_growth_limit(0),
            Limit::Compute => unreachable!("a start charges no compute past the cap"),
        };
    }
    let unlimited = unlimited_creation_run(creation, limit, gas_limit);
    match (creation, limit) {
        (Creation::Nested, Limit::DataSize) => {
            limits.with_tx_data_size_limit(unlimited.usage.data_size - WRITE_RECORD_SIZE)
        }
        (_, Limit::DataSize) => limits.with_tx_data_size_limit(unlimited.usage.data_size - 1),
        (_, Limit::KvUpdate) => limits.with_tx_kv_update_limit(unlimited.usage.write_records - 1),
        // The created account and the init code's slot are the creation's new state, two of it:
        // a nested creation's budget, 98% of the whole count, is one short of them.
        (Creation::Nested, Limit::StateGrowth) => {
            limits.with_tx_state_growth_limit(unlimited.usage.state_growth)
        }
        (_, Limit::StateGrowth) => {
            limits.with_tx_state_growth_limit(unlimited.usage.state_growth - 1)
        }
        (_, Limit::Compute) => limits,
    }
}

/// A creation the limit stopped, revived by an inspector that rewrites every frame result into a
/// success, is taken as given, as a revived call is, and is no stop: the result of the
/// transaction's own frame is the inspector's success, so the outcome names no stop. A creation
/// transaction whose init code crosses a limit, a nested creation whose init code crosses one, and
/// a nested creation whose start crosses one each end with the stop before their revival and deploy
/// nothing: the stop reverted the creation's journal. The revival settles as a success, so what the
/// creation counted before it crossed is kept — all the creation counts without limits but the
/// write that crossed. Where its start crossed, it counted nothing of the start: its creator keeps
/// its own nonce record, counted on it once the creation returned, and catches the start's stop,
/// so the revival changes the creation's result alone. Below the execution cap and above it.
#[test]
fn test_a_revived_creation_keeps_what_it_counted_before_its_stop() {
    let cases = [
        (Creation::Transaction, Limit::ALL.as_slice()),
        (Creation::Nested, Limit::ALL.as_slice()),
        (Creation::NestedStart, [Limit::DataSize, Limit::KvUpdate, Limit::StateGrowth].as_slice()),
    ];
    for (creation, limits) in cases {
        for &limit in limits {
            for gas_limit in TIERS {
                let case = format!("{creation:?}, {limit:?}, gas limit {gas_limit}");
                let configured = creation_limits(creation, limit, gas_limit);
                let (db, tx) = creation_run(creation, limit, gas_limit);
                let mut evm = MegaEvm::new(context(db.clone()).with_tx_runtime_limits(configured))
                    .with_inspector(FrameRecorder::recording());
                let plain = evm.execute_transaction(tx.clone()).expect("the transaction is valid");
                let [(InstructionResult::Revert, stop)] = evm.inspector().created.as_slice() else {
                    panic!(
                        "{case}: the creation ended with a revert: {:?}",
                        evm.inspector().created
                    )
                };
                let (kind, _) = mega_evm::decode_mega_limit_exceeded(stop)
                    .unwrap_or_else(|| panic!("{case}: the creation was stopped: {stop}"));
                assert_eq!(kind, limit.kind(), "{case}");
                let stop = stop.clone();

                let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(configured))
                    .with_inspector(FrameRecorder::rewriting());
                let revived = evm.execute_transaction(tx).expect("the revival is not refused");
                let rewriter = evm.inspector();
                assert_eq!(
                    rewriter.created,
                    [(InstructionResult::Revert, stop)],
                    "{case}: the creation ended with the stop before its revival"
                );
                assert!(revived.result.is_success(), "{case}: {:?}", revived.result);
                assert_eq!(revived.limit_exceeded, None, "{case}: no stop is the result");
                let created = match creation {
                    Creation::Transaction => CALLER.create(0),
                    Creation::Nested | Creation::NestedStart => A.create(0),
                };
                assert!(
                    revived.state.get(&created).is_none_or(|account| account.info.is_empty()),
                    "{case}: nothing deployed"
                );
                let unlimited = unlimited_creation_run(creation, limit, gas_limit);
                let counted = match (creation, limit) {
                    // `A` exists, so its nonce record is no new account.
                    (Creation::NestedStart, _) => LimitUsage {
                        data_size: TX_BODY_SIZE + WRITE_RECORD_SIZE,
                        write_records: 1,
                        state_growth: 0,
                    },
                    // The crossing write, a fresh slot, is one record and one new slot.
                    (_, Limit::DataSize | Limit::KvUpdate | Limit::StateGrowth) => LimitUsage {
                        data_size: unlimited.usage.data_size - WRITE_RECORD_SIZE,
                        write_records: unlimited.usage.write_records - 1,
                        state_growth: unlimited.usage.state_growth - 1,
                    },
                    (_, Limit::Compute) => unlimited.usage,
                };
                assert_eq!(revived.usage, counted, "{case}: what it counted before its stop");
                if matches!(creation, Creation::NestedStart) {
                    // `A` catches the start's stop and keeps the same.
                    assert!(plain.result.is_success(), "{case}: {:?}", plain.result);
                    assert_eq!(plain.usage, counted, "{case}: the stop kept nothing of the start");
                } else {
                    assert_ne!(revived.usage, plain.usage, "{case}: which the stop takes back");
                }
            }
        }
    }
}

/* ---------- the frame the last step ran in ---------- */

/// Records the frame the last step ran in.
#[derive(Default)]
struct LastStep(Option<Address>);

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for LastStep {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut MegaContext<DB>) {
        self.0 = Some(interp.input.target_address);
    }
}

/// Runs `tx` on `db` under `limits` and a [`LastStep`], and returns the outcome and the frame the
/// last step ran in.
fn run_recording_steps(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    tx: mega_evm::MegaTransaction,
) -> (MegaTransactionOutcome, Option<Address>) {
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits))
        .with_inspector(LastStep::default());
    let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
    (outcome, evm.inspector().0)
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

/// A frame three calls below the transaction's own that crosses its data-size budget reverts alone
/// with `MegaLimitExceeded(0, budget)`, where the budget is what the shares left it. Its caller
/// resumes and reads the stop, every frame above runs on, the transaction reports no stop and
/// succeeds, below and above the execution cap.
#[test]
fn test_a_frame_budget_crossed_three_calls_down_reverts_that_frame_alone() {
    // The transaction's own frame gets the room above the body; each relay logs before it calls,
    // which counts 32 bytes and no write record.
    const ROOM: u64 = 1_000;
    let budget = share(share(share(ROOM - LOG_BASE_SIZE) - LOG_BASE_SIZE) - LOG_BASE_SIZE);
    let kind = LimitKind::DataSize;
    let limits = EvmTxRuntimeLimits::default().with_tx_data_size_limit(TX_BODY_SIZE + ROOM);
    // D's log and its writes cross its budget long before the transaction's limit.
    let d = writer(budget / WRITE_RECORD_SIZE + 1);
    for gas_limit in TIERS {
        let case = format!("gas limit {gas_limit}");
        let outcome = execute(resuming_chain(d.clone()), limits, gas_limit);
        assert!(outcome.result.is_success(), "{case}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{case}: a frame budget below is no stop");

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
        let d_kept =
            outcome.state.get(&D).is_some_and(|d| d.storage.values().any(|slot| slot.is_changed()));
        assert!(!d_kept, "{case}: D kept a write");
        let loggers: Vec<_> = outcome.result.logs().iter().map(|log| log.address).collect();
        assert_eq!(loggers, [A, B, C], "{case}: D's log went with its frame");
    }
}

/* ---------- halts ---------- */

/// Every per-transaction limit, armed and never crossed: a halt must stay a halt under them.
fn armed() -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::default().with_tx_data_size_limit(TX_DATA_LIMIT)
}

/// A loop that never ends. A round copies 32 KiB within memory, which costs 3,075 gas and the
/// interpreter one instruction, so the frame runs out of however much gas it has in few rounds.
fn spin() -> Bytes {
    crate::detention::spin(BytecodeBuilder::default())
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

/// A gas limit whose reservoir pays the state and history gas of [`writes_past_the_cap`]'s fresh
/// slots at the prices in effect, so the writes spend nothing but compute from the frame's regular
/// gas.
fn roomy() -> u64 {
    TX_GAS_LIMIT_CAP + slots_past_the_cap() * fresh_write_spill() + 100_000_000
}

/// The fresh slots these rows write after a read: one more than the cap's compute pays for, 1,653
/// at the schedule's prices, so their compute is past the cap.
fn slots_past_the_cap() -> u64 {
    CAP / fresh_write() + 1
}

/// The spec's limits with the state-growth limit lifted, which these rows run under: their writes
/// are [`slots_past_the_cap`] fresh slots, more than the 1,000 new slots the spec's limit lets a
/// transaction keep, so under it the 1,001st slot would stop the frame before its compute reached
/// the cap.
fn detention_limits() -> EvmTxRuntimeLimits {
    EvmTxRuntimeLimits::default().with_tx_state_growth_limit(u64::MAX)
}

/// Appends writes of the fresh slots `1..=`[`slots_past_the_cap`]: 20,011,218 of compute at the
/// schedule's prices, past the cap.
fn writes_past_the_cap(code: BytecodeBuilder) -> BytecodeBuilder {
    (1..=slots_past_the_cap())
        .fold(code, |code, slot| code.sstore(U256::from(slot), U256::from(slot)))
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

/// Runs `db`'s `A` under `limits` with [`roomy`] gas and asserts the detention stop of the
/// transaction's own frame: a revert carrying `MegaLimitExceeded(2, limit)` for the limit the reads
/// set, billed the intrinsic gas plus the compute up to the charge that crosses that limit, `left`
/// short of it, and nothing of the gas the transaction had left. `A` crossed it, and ran the last
/// step: itself, or after a callee that crossed first returned the stop into it, on what the limit
/// left it. Returns the limit and what the transaction read.
fn assert_detention_stop(
    db: MemoryDatabase,
    limits: EvmTxRuntimeLimits,
    left: u64,
) -> (u64, mega_evm::VolatileDataAccess) {
    let intrinsic = intrinsic(roomy());
    let (recorded, last) =
        run_recording_steps(db.clone(), limits, call(CALLER, A, U256::ZERO, roomy()));
    assert_eq!(last, Some(A), "the last step ran in the transaction's own frame");
    let mut evm = MegaEvm::new(context(db).with_tx_runtime_limits(limits));
    let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, roomy())).unwrap();
    assert_eq!(
        (&recorded.result, recorded.gas),
        (&outcome.result, outcome.gas),
        "the recorder changes nothing"
    );
    let detention = evm.ctx().detention();
    let limit = detention.compute_limit().expect("a read set a limit");
    let used = limit - left;
    let stop =
        LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, limit, used, frame_local: false };
    assert!(
        matches!(&outcome.result, ExecutionResult::Revert { output, .. }
            if output == &stop.revert_data()),
        "a revert carrying the stop: {:?}",
        outcome.result
    );
    assert_eq!(outcome.limit_exceeded, Some(stop));
    assert_eq!(outcome.gas.regular, intrinsic.gas.regular + used);
    assert_eq!(outcome.gas.gas_used, intrinsic.gas.gas_used + used);
    assert_eq!(outcome.gas.reservoir_remaining, intrinsic.gas.reservoir_remaining);
    assert!(outcome.result.logs().is_empty());
    (limit, detention.accessed())
}

/// The writes after a read: the read's `POP`, then the fresh slots, of which the reservoir pays the
/// state and history gas.
fn pop_then_writes() -> Charges {
    Charges::default().then(&[2]).fresh_writes(slots_past_the_cap())
}

/// After the Oracle's read: its `POP`, its caller's, then the fresh slots.
fn oracle_pops_then_writes() -> Charges {
    Charges::default().then(&[2, 2]).fresh_writes(slots_past_the_cap())
}

/// A frame that reads the block's timestamp and then writes the fresh slots stops at the
/// first write's charge past the limit the read set, and bills its compute before it: the
/// transaction's own frame, a child whose caller catches the stop and is held to what is left,
/// and a caller whose child did its work after the read.
#[test]
fn test_volatile_data_access_oog_does_not_consume_all_gas() {
    let code = writes_past_the_cap(BytecodeBuilder::default().append_many([TIMESTAMP, POP]));
    let db = MemoryDatabase::default().account_code(A, code.stop().build());
    let left = pop_then_writes().left(CAP);
    let (limit, accessed) = assert_detention_stop(db, detention_limits(), left);
    assert!(limit - CAP < 1_000, "the read came first: {}", limit - CAP);
    assert_eq!(accessed, mega_evm::VolatileDataAccess::TIMESTAMP);
}

/// A child that reads the block's timestamp and then writes the fresh slots is stopped at the
/// first write past the limit; its caller resumes on what the limit leaves it, and its own write
/// after the call crosses the limit again, which stops the transaction there.
#[test]
fn test_nested_call_block_env_access_child_oog() {
    let child = writes_past_the_cap(BytecodeBuilder::default().append_many([TIMESTAMP, POP]));
    let parent = call_all(BytecodeBuilder::default(), B).append(POP).sstore(MARKER, U256::from(1));
    let db = MemoryDatabase::default()
        .account_code(A, parent.stop().build())
        .account_code(B, child.stop().build());
    // The child's crossing, then the caller's `POP` and write, from the room the child's crossing
    // charge was not made from.
    let left = pop_then_writes().left(CAP);
    let left = Charges::default().then(&[2]).fresh_writes(1).left(left);
    let (limit, _) = assert_detention_stop(db, detention_limits(), left);
    assert!(limit - CAP < 10_000, "the child read near the start: {}", limit - CAP);
}

/// A frame that reads the block's timestamp, calls a child that does some work, then writes the
/// fresh slots stops at the limit its read set: the child's work counts towards it.
#[test]
fn test_parent_block_env_access_oog_after_nested_call() {
    let child =
        BytecodeBuilder::default().push_number(1_u8).push_number(2_u8).append(ADD).append(POP);
    let parent = call_all(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), B).append(POP);
    let db = MemoryDatabase::default()
        .account_code(A, writes_past_the_cap(parent).stop().build())
        .account_code(B, child.stop().build());
    // The read's `POP`, the call's five pushes, its address, `GAS` and cold access, the child's
    // two pushes, `ADD` and `POP`, the caller's `POP`, then the writes.
    let left = Charges::default()
        .then(&[2, 2, 2, 2, 2, 2, 3, 2, cold_access(), 3, 3, 3, 2, 2])
        .fresh_writes(slots_past_the_cap())
        .left(CAP);
    let (limit, _) = assert_detention_stop(db, detention_limits(), left);
    assert!(limit - CAP < 1_000, "the read came first: {}", limit - CAP);
}

/// A call to the Oracle that reads its storage, then the fresh slots: the transaction stops
/// at the first write's charge past its compute at the read plus the Oracle's cap, the spec's
/// 20,000,000, and bills its compute before it.
#[test]
fn test_an_oracle_read_holds_the_transaction_to_its_cap() {
    let code = writes_past_the_cap(call_oracle(BytecodeBuilder::default(), None));
    let db = with_oracle_read(MemoryDatabase::default().account_code(A, code.stop().build()));
    let left = oracle_pops_then_writes().left(ORACLE_ACCESS_COMPUTE_GAS);
    let (limit, accessed) = assert_detention_stop(db, detention_limits(), left);
    assert!(limit - ORACLE_ACCESS_COMPUTE_GAS < 10_000, "{}", limit - ORACLE_ACCESS_COMPUTE_GAS);
    assert_eq!(accessed, mega_evm::VolatileDataAccess::ORACLE);
}

/// The same with the Oracle called on 65,535 gas: the stop bills the compute, not the gas limit.
#[test]
fn test_oracle_volatile_data_access_oog_does_not_consume_all_gas() {
    let code = writes_past_the_cap(call_oracle(BytecodeBuilder::default(), Some(0xffff)));
    let db = with_oracle_read(MemoryDatabase::default().account_code(A, code.stop().build()));
    let left = oracle_pops_then_writes().left(ORACLE_ACCESS_COMPUTE_GAS);
    assert_detention_stop(db, detention_limits(), left);
}

/// A contract that calls one that reads the Oracle's storage and then writes the fresh slots: the
/// middle contract is stopped at the first write past the limit, and the outer contract resumes
/// on what the limit leaves it: its `POP`, then its read of its own slot, whose cold access crosses
/// the limit again and stops the transaction.
#[test]
fn test_parent_runs_out_of_gas_after_oracle_access() {
    let middle = writes_past_the_cap(call_oracle(BytecodeBuilder::default(), None));
    let outer =
        call_all(BytecodeBuilder::default(), B).append(POP).append_many([PUSH0, SLOAD, POP]);
    let db = MemoryDatabase::default()
        .account_code(A, outer.stop().build())
        .account_code(B, middle.stop().build());
    // The middle contract's crossing, then the outer one's `POP`, `PUSH0` and cold `SLOAD`: its
    // static gas, then the cold surcharge.
    let left = oracle_pops_then_writes().left(ORACLE_ACCESS_COMPUTE_GAS);
    let params = mega_evm::satin_gas_params();
    let sload = [
        2,
        2,
        params.get(GasId::warm_storage_read_cost()),
        params.get(GasId::cold_storage_additional_cost()),
        2,
    ];
    let left = Charges::default().then(&sload).left(left);
    let (_, accessed) = assert_detention_stop(with_oracle_read(db), detention_limits(), left);
    assert_eq!(accessed, mega_evm::VolatileDataAccess::ORACLE);
}

/// A read of the block's timestamp, then of the Oracle's storage under a lower cap: the most
/// restrictive limit binds, the Oracle's, and the stop bills the compute up to the charge that
/// crosses it.
#[test]
fn test_both_volatile_data_access_oog_does_not_consume_all_gas() {
    const ORACLE_CAP: u64 = 1_000_000;
    let code = call_oracle(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), Some(0xffff));
    let db = MemoryDatabase::default().account_code(A, writes_past_the_cap(code).stop().build());
    let limits = detention_limits().with_oracle_access_compute_gas_limit(ORACLE_CAP);
    let left = oracle_pops_then_writes().left(ORACLE_CAP);
    let (limit, accessed) = assert_detention_stop(with_oracle_read(db), limits, left);
    assert!(limit - ORACLE_CAP < 10_000, "the Oracle's read binds: {limit}");
    assert_eq!(
        accessed,
        mega_evm::VolatileDataAccess::TIMESTAMP | mega_evm::VolatileDataAccess::ORACLE
    );
}

/// A read of the block's timestamp, then compute past the cap: the stop comes at the work's first
/// charge past the compute at the read plus the cap.
#[test]
fn test_volatile_access_post_access_cap_enforced() {
    let code = work(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), ROUNDS);
    let db = MemoryDatabase::default().account_code(A, code.stop().build());
    let left = Charges::default().then(&[2]).work(ROUNDS, 0).left(CAP);
    let (limit, _) = assert_detention_stop(db, detention_limits(), left);
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
    assert!(
        detained.gas.gas_used < 50_000 + body_history(0),
        "what it ran and its body's history: {}",
        detained.gas.gas_used
    );
}

/// A library's write, one call down, crosses a limit one byte above the body: the library is
/// stopped with a revert, which its caller catches before it ends, and the transaction is billed
/// what ran — its intrinsic gas, its body's history, the call, the library up to and including the
/// write, and the caller's `POP` — and nothing more: the unspent regular gas comes back to the
/// sender, and so does the state gas the write spilled onto it, which a revert gives back.
///
/// What ran, below the execution cap, so the body's history is paid from the regular budget:
///
/// | Part | Gas |
/// |---|---:|
/// | the call's intrinsic regular gas: the EIP-2780 base and the recipient | 15,000 |
/// | `A`'s five `PUSH0`, its `PUSH20` and its `GAS` | 15 |
/// | `A`'s `CALL`: the warm access and the cold surcharge for `B` | 3,000 |
/// | `B`'s `PUSH1` and `PUSH0` | 5 |
/// | `B`'s cold `SLOAD` | 2,100 |
/// | `B`'s `SSTORE` of the slot it warmed: static and the storage write | 10,100 |
/// | `A`'s `POP` of the call's status | 2 |
///
/// Rules: [S11.8], [S11.9]. Independence: independent — the parts are written out; the body's
/// history is priced by the production helper.
#[test]
fn test_data_limit_exceed_in_nested_call() {
    let limit = TX_BODY_SIZE + 1;
    let library = BytecodeBuilder::default().push_number(1_u8).append_many([PUSH0, SLOAD, SSTORE]);
    let db = MemoryDatabase::default()
        .account_code(A, call_all(BytecodeBuilder::default(), B).append(POP).stop().build())
        .account_code(B, library.stop().build());
    let outcome = execute(db, EvmTxRuntimeLimits::default().with_tx_data_size_limit(limit), BELOW);
    assert!(outcome.result.is_success(), "the caller caught the stop: {:?}", outcome.result);
    assert_eq!(outcome.limit_exceeded, None);
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 }
    );
    let ran = 15_000 + 15 + 3_000 + 5 + 2_100 + 10_100 + 2;
    assert_eq!(outcome.gas.regular, ran, "what ran, on the regular ledger");
    assert_eq!(outcome.gas.history, body_history(0), "the body's history alone");
    assert_eq!(outcome.gas.state, 0, "the write's state gas came back");
    assert_eq!(outcome.gas.gas_used, ran + body_history(0), "and nothing more: the rest came back");
}

/// A value transfer to a contract that writes a slot, under a limit one byte above the body: the
/// first frame's start crosses it — its own budget of one byte, and the transaction's limit — so
/// the value never moves and the contract never runs.
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
    let stop = outcome.limit_exceeded.expect("the transaction is stopped");
    assert!(
        matches!(
            stop,
            LimitCheck::ExceedsLimit { kind: LimitKind::DataSize, limit: 1, frame_local: true, .. }
        ),
        "the frame's budget, the byte the body leaves: {stop:?}"
    );
    assert!(
        matches!(stop, LimitCheck::ExceedsLimit { used, .. } if used == WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE),
        "{stop:?}"
    );
    assert!(
        matches!(&outcome.result, ExecutionResult::Revert { output, .. }
            if output == &stop.revert_data()),
        "{:?}",
        outcome.result
    );
    assert_eq!(
        outcome.usage,
        LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 }
    );
    assert!(outcome.state.get(&A).is_none_or(|a| a.storage.is_empty() && a.info.balance.is_zero()));
    assert_eq!(outcome.state[&CALLER].info.balance, U256::from(10_000), "the value did not move");
}

/// A transaction detained from its start — its sender is the block's beneficiary — whose body
/// crosses the data-size limit is rejected at validation, as the same transaction undetained is:
/// detention changes nothing about what validation refuses.
#[test]
fn test_detention_plus_intrinsic_data_size_overflow() {
    let db =
        || MemoryDatabase::default().account_code(A, BytecodeBuilder::default().stop().build());
    let run = |limits, sender| {
        MegaEvm::new(context(db()).with_tx_runtime_limits(limits)).execute_transaction(call(
            sender,
            A,
            U256::ZERO,
            roomy(),
        ))
    };
    let mut evm = MegaEvm::new(context(db()));
    let within_limits = evm.execute_transaction(call(BENEFICIARY, A, U256::ZERO, roomy())).unwrap();
    assert_eq!(evm.ctx().detention().compute_limit(), Some(CAP), "detained from its start");
    assert!(within_limits.result.is_success(), "{:?}", within_limits.result);

    let over = EvmTxRuntimeLimits::default().with_tx_data_size_limit(100);
    let detained = run(over, BENEFICIARY).expect_err("rejected");
    let undetained = run(over, CALLER).expect_err("rejected");
    assert!(format!("{detained:?}").contains(mega_evm::TX_BODY_OVER_DATA_SIZE_LIMIT));
    assert_eq!(format!("{detained:?}"), format!("{undetained:?}"));
}

/// A call a data-size limit stopped — its own frame budget, or the transaction's limit — revived
/// into a success by an inspector, hands its caller nothing to count: its checkpoint was reverted
/// before the inspector saw its result, so its log is not kept, and the log was its one write,
/// which the crossing counted nothing of. Its caller runs on within the limits, and the transaction
/// is the one no inspector runs: the rewrite changed only the flag `A` drops.
///
/// Under a frame cap of 1,000 bytes `A` gives `B` 98% of it, and `B`'s log of 1,000 bytes counts
/// 1,032, which crosses `B`'s budget, and would cross `A`'s were it handed on. Under a transaction
/// limit of the body and 1,000 bytes the same log crosses the transaction's limit and `B`'s budget,
/// 98% of the 1,000 `A` is given, which is checked first and named.
#[test]
fn test_a_call_revived_past_a_limit_hands_its_caller_nothing() {
    const CAP: u64 = 1_000;
    const LOGGED: u64 = 1_000;
    const { assert!(LOG_BASE_SIZE + LOGGED > CAP, "the log would cross A's budget too") };
    let share = CAP * FRAME_DATA_SHARE_NUMERATOR / FRAME_DATA_SHARE_DENOMINATOR;
    let a = call_all(BytecodeBuilder::default(), B)
        .append(POP)
        .sstore(MARKER, U256::from(1))
        .stop()
        .build();
    let b =
        BytecodeBuilder::default().push_number(LOGGED).append_many([PUSH0, LOG0]).stop().build();
    let db = MemoryDatabase::default().account_code(A, a).account_code(B, b);
    let unlimited = EvmTxRuntimeLimits::no_limits();
    let rows = [
        ("its frame budget", unlimited.with_frame_data_size_limit(CAP), share),
        ("the transaction's limit", unlimited.with_tx_data_size_limit(TX_BODY_SIZE + CAP), share),
    ];
    for (row, limits, crossed) in rows {
        let plain = execute(db.clone(), limits, BELOW);
        let mut evm = MegaEvm::new(context(db.clone()).with_tx_runtime_limits(limits))
            .with_inspector(FrameRecorder::rewriting());
        let outcome = evm.execute_transaction(call(CALLER, A, U256::ZERO, BELOW)).unwrap();
        let rewriter = evm.inspector();

        // The revert data names the limit crossed, whatever the usage.
        let stop = LimitCheck::ExceedsLimit {
            kind: LimitKind::DataSize,
            limit: crossed,
            used: 0,
            frame_local: false,
        };
        assert_eq!(
            rewriter.ended,
            vec![
                (B, InstructionResult::Revert, stop.revert_data()),
                (A, InstructionResult::Stop, Bytes::new())
            ],
            "{row}: B crossed it; A did not"
        );
        assert_eq!(rewriter.marker_writes(), 1, "{row}: A ran on after its call returned");
        assert!(outcome.result.is_success(), "{row}: {:?}", outcome.result);
        assert_eq!(outcome.limit_exceeded, None, "{row}: a stop below is no stop");
        assert_eq!(
            outcome.usage,
            LimitUsage {
                data_size: TX_BODY_SIZE + WRITE_RECORD_SIZE,
                write_records: 1,
                state_growth: 1
            },
            "{row}: the body and A's marker, a new slot: none of B's log"
        );
        assert!(outcome.result.logs().is_empty(), "{row}: B's log was reverted with B");
        assert!(plain.result.is_success(), "{row}: {:?}", plain.result);
        assert_eq!((&outcome.result, &outcome.state), (&plain.result, &plain.state), "{row}");
        assert_eq!((outcome.gas, outcome.usage), (plain.gas, plain.usage), "{row}");
    }
}

/* ---------- a frame three calls down, answered without running ---------- */

/// An account with nothing at it, which a value call adds.
const EMPTY: Address = address!("0000000000000000000000000000000000500005");

/// A frame three calls down answered without running, whose answer crosses a limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answered {
    /// An inspector answers the call to `D` having spent all it was forwarded, past what the
    /// compute limit leaves the frame. An inspector's answer is taken as given: it is no stop
    /// (see [`test_an_inspectors_answer_past_the_allowance_is_taken_as_given`]).
    PastTheAllowance,
    /// `C` calls a precompile priced past the cap, which revm runs on the allowance: it runs out
    /// of it without computing.
    Precompile,
    /// `C` sends a wei to [`EMPTY`], whose start's records and transfer log cross the data-size
    /// limit once revm answered the call.
    StartRecords,
}

impl Answered {
    /// The address the answered call is made to.
    const fn target(self) -> Address {
        match self {
            Self::PastTheAllowance => D,
            Self::Precompile => PRICED,
            Self::StartRecords => EMPTY,
        }
    }
}

/// The chain `A` → `B` → `C` → the answered frame. `A` reads the block's timestamp first, which
/// detains the transaction under the default limits. Each frame calls the next with all its gas,
/// and `C` makes the answered call; then every frame drops the call's status and stops.
fn answered_chain(answered: Answered) -> MemoryDatabase {
    let end = |code: BytecodeBuilder| code.append(POP).stop().build();
    let c = match answered {
        Answered::PastTheAllowance => call_all(BytecodeBuilder::default(), D),
        Answered::Precompile => BytecodeBuilder::default()
            .mstore(0, U256::from(CAP + 1).to_be_bytes::<32>())
            .append_many([PUSH0, PUSH0])
            .push_number(32_u8)
            .append_many([PUSH0, PUSH0])
            .push_address(PRICED)
            .append_many([GAS, CALL]),
        Answered::StartRecords => BytecodeBuilder::default()
            .append_many([PUSH0, PUSH0, PUSH0, PUSH0])
            .push_number(1_u8)
            .push_address(EMPTY)
            .append_many([GAS, CALL]),
    };
    MemoryDatabase::default()
        .account_code(A, end(call_all(BytecodeBuilder::default().append_many([TIMESTAMP, POP]), B)))
        .account_code(B, end(call_all(BytecodeBuilder::default(), C)))
        .account_code(C, end(c))
        .account_code(D, BytecodeBuilder::default().stop().build())
        .account_balance(C, U256::from(1))
}

/// The compute `answered`'s chain spends after `A`'s read up to the answered call: `A`'s `POP`,
/// then the three calls down — five pushes, the address, `GAS` and the callee's cold access —
/// `C`'s own being, for the precompile, the price word's `MSTORE` — two pushes, the opcode and a
/// word of memory — the call's five pushes, its address, `GAS` and the precompile's warm access.
fn to_the_answer(answered: Answered) -> u64 {
    let call_all = 2 * 5 + 3 + 2 + cold_access();
    let c = match answered {
        Answered::Precompile => 3 + 3 + 3 + 3 + 2 * 2 + 3 + 2 * 2 + 3 + 2 + 100,
        _ => call_all,
    };
    2 + 2 * call_all + c
}

/// The limits `answered`'s chain crosses its limit under, and the stop the answered frame returns.
/// The compute limit is set by `A`'s read, at the read's own two gas plus the cap, and the answer
/// is the charge that crosses it: the stop reports the compute when the answered call was made.
fn answered_limits(answered: Answered) -> (EvmTxRuntimeLimits, LimitCheck) {
    let stop =
        |kind, limit, used| LimitCheck::ExceedsLimit { kind, limit, used, frame_local: false };
    match answered {
        Answered::PastTheAllowance | Answered::Precompile => (
            EvmTxRuntimeLimits::default(),
            stop(LimitKind::ComputeGas, 2 + CAP, 2 + to_the_answer(answered)),
        ),
        Answered::StartRecords => {
            // The body, and one byte short of the start's two records and its transfer log. The
            // start three calls down crosses the budget it would start with too, 98% of 98% of
            // 98% of what the body leaves, which the stop names.
            let start = 2 * WRITE_RECORD_SIZE + TRANSFER_LOG_SIZE;
            let limit = TX_BODY_SIZE + start - 1;
            let share =
                |left: u64| left * FRAME_DATA_SHARE_NUMERATOR / FRAME_DATA_SHARE_DENOMINATOR;
            let budget = share(share(share(start - 1)));
            (
                EvmTxRuntimeLimits::no_limits().with_tx_data_size_limit(limit),
                LimitCheck::ExceedsLimit {
                    kind: LimitKind::DataSize,
                    limit: budget,
                    used: start,
                    frame_local: true,
                },
            )
        }
    }
}

/// Answers every call to `D` itself, having spent all the gas it was forwarded, and records every
/// frame it sees end, deepest first, with the result and output it had, and the gas the call to `D`
/// was forwarded; with `rewrite`, it then rewrites that result into a success, as
/// [`FrameRecorder`] does.
struct AnswersD {
    rewrite: bool,
    ended: Vec<(Address, InstructionResult, Bytes)>,
    forward: Option<u64>,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for AnswersD {
    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        (inputs.target_address == D).then(|| {
            self.forward = Some(inputs.gas_limit);
            let mut gas =
                Gas::new_with_regular_gas_and_reservoir(inputs.gas_limit, inputs.reservoir);
            gas.spend_all();
            CallOutcome::new(
                InterpreterResult::new(InstructionResult::Stop, Bytes::new(), gas),
                inputs.return_memory_offset.clone(),
            )
        })
    }

    fn call_end(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        let result = &mut outcome.result;
        self.ended.push((inputs.target_address, result.result, result.output.clone()));
        if self.rewrite {
            result.result = InstructionResult::Stop;
            result.output = Bytes::new();
        }
    }
}

/// A run of an answered chain: its outcome, the frames an inspector saw end, what the precompile
/// at [`PRICED`] ran on, each with whether its price was within it, and the gas an inspector's
/// answer to `D` was forwarded.
struct AnsweredRun {
    outcome: MegaTransactionOutcome,
    ended: Vec<(Address, InstructionResult, Bytes)>,
    ran_on: Vec<(u64, bool)>,
    forward: Option<u64>,
}

/// Runs the stopped chain of `answered` under `limits` at `gas_limit`, with the precompile at
/// [`PRICED`] installed. Without `rewrite` it runs without an inspector — an answer past the
/// allowance under one that answers `D` and rewrites nothing — and with `rewrite` under one that
/// rewrites every frame result into a success.
fn run_answered(
    answered: Answered,
    limits: EvmTxRuntimeLimits,
    gas_limit: u64,
    rewrite: bool,
) -> AnsweredRun {
    let runs = Runs::default();
    let evm = MegaEvm::new(context(answered_chain(answered)).with_tx_runtime_limits(limits))
        .with_dyn_precompiles(HashMap::from_iter([(PRICED, priced(&runs))]));
    let tx = call(CALLER, A, U256::ZERO, gas_limit);
    let (outcome, ended, forward) = if answered == Answered::PastTheAllowance {
        let mut evm = evm.with_inspector(AnswersD { rewrite, ended: Vec::new(), forward: None });
        let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
        (outcome, evm.inspector().ended.clone(), evm.inspector().forward)
    } else if rewrite {
        let mut evm = evm.with_inspector(FrameRecorder::rewriting());
        let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
        (outcome, evm.inspector().ended.clone(), None)
    } else {
        let mut evm = evm;
        (evm.execute_transaction(tx).expect("the transaction is valid"), Vec::new(), None)
    };
    let ran_on = runs.lock().unwrap().clone();
    AnsweredRun { outcome, ended, ran_on, forward }
}

/// One answered cell, both columns: `answered`'s chain at `gas_limit`, without the rewrite and
/// with it.
///
/// The answered frame is the stop, which `C` catches, and every frame then drops the status and
/// stops: the transaction succeeds. A compute stop bills its compute up to the answer past the
/// intrinsic gas, then the three callers' `POP`s. A start's stop bills what the same chain bills
/// without limits, where the start is made: the callers pay the call the same either way, and the
/// start's records and the account it would add are history and state gas, which its stop gives
/// back. That run is on the same engine, so its bill pins that a stop bills what ran and no more;
/// it pins no price, and a charge both runs make, such as `C`'s own `CALL`, would be wrong in both
/// alike.
///
/// A start's stop the inspector rewrites into a success is taken as given: the success gives back
/// neither the account the start's `CALL` was charged for nor the history of the start's records,
/// so that column bills the state and history gas of the run without limits, where the start is
/// kept, while it counts nothing and moves nothing, as the stop left it.
fn assert_answered_cell(answered: Answered, gas_limit: u64) {
    let row = format!("{answered:?}, gas limit {gas_limit}");
    let intrinsic = intrinsic(gas_limit);
    let (limits, stop) = answered_limits(answered);
    let LimitCheck::ExceedsLimit { kind, used, .. } = stop else { unreachable!() };
    let free = (kind != LimitKind::ComputeGas).then(|| {
        let free = execute(answered_chain(answered), EvmTxRuntimeLimits::no_limits(), gas_limit);
        assert!(free.result.is_success(), "{row}: without limits: {:?}", free.result);
        assert_eq!(balance_of(&free, EMPTY), U256::from(1), "{row}: the start moves its wei");
        free
    });
    let ran = free.as_ref().map_or(used + 3 * 2, |free| free.gas.regular - intrinsic.gas.regular);

    let plain = run_answered(answered, limits, gas_limit, false);
    let rewritten = run_answered(answered, limits, gas_limit, true);
    for (column, run) in [("not rewritten", &plain), ("rewritten", &rewritten)] {
        let cell = format!("{row}, {column}");
        let outcome = &run.outcome;
        let (state, history) = match (&free, column) {
            (Some(free), "rewritten") => (free.gas.state, free.gas.history),
            _ => (0, intrinsic.gas.history),
        };
        let kept = state + history - intrinsic.gas.history;
        assert!(outcome.result.is_success(), "{cell}: the callers caught the stop");
        assert_eq!(outcome.limit_exceeded, None, "{cell}: a stop below is no stop of it");
        assert_eq!(outcome.gas.regular, intrinsic.gas.regular + ran, "{cell}: regular");
        assert_eq!(outcome.gas.state, state, "{cell}: the state gas");
        assert_eq!(outcome.gas.history, history, "{cell}: the history gas");
        assert_eq!(outcome.gas.history_bytes, intrinsic.gas.history_bytes, "{cell}");
        assert_eq!(
            outcome.gas.reservoir_remaining,
            intrinsic.gas.reservoir_remaining.saturating_sub(kept),
            "{cell}: the reservoir comes back, less the body it paid and what is kept"
        );
        assert_eq!(outcome.gas.gas_used, intrinsic.gas.gas_used + ran + kept, "{cell}: gas used");
        assert!(outcome.result.logs().is_empty(), "{cell}: no log is kept");
        assert_eq!(
            outcome.usage,
            LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 },
            "{cell}: the body stays"
        );
        assert_eq!(balance_of(outcome, EMPTY), U256::ZERO, "{cell}: no value moved");
        if answered == Answered::Precompile {
            let allowance = CAP - to_the_answer(answered);
            assert_eq!(run.ran_on, [(allowance, false)], "{cell}: run on the allowance");
        }
    }
    if free.is_none() {
        assert_eq!(rewritten.outcome.result, plain.outcome.result, "{row}");
        assert_eq!(rewritten.outcome.gas, plain.outcome.gas, "{row}");
    }
    let ended = vec![
        (answered.target(), InstructionResult::Revert, stop.revert_data()),
        (C, InstructionResult::Stop, Bytes::new()),
        (B, InstructionResult::Stop, Bytes::new()),
        (A, InstructionResult::Stop, Bytes::new()),
    ];
    assert_eq!(
        rewritten.ended, ended,
        "{row}: the answered frame ended stopped, then every caller ended"
    );
}

/// The balance `address` holds after `outcome`: zero for an account it did not touch.
fn balance_of(outcome: &MegaTransactionOutcome, address: Address) -> U256 {
    outcome.state.get(&address).map(|account| account.info.balance).unwrap_or_default()
}

/// A frame three calls down that is answered without running, and whose answer crosses a limit, is
/// the stop as a frame that ran there would be: a precompile run on what the compute limit leaves
/// the frame and priced past it. Below the execution cap and above it, with and without an
/// inspector that rewrites every frame result into a success, the answer is the stop, its caller
/// catches it, every frame ends, and the transaction bills what ran, keeps the body's history and
/// nothing else, and gets its reservoir back.
#[test]
fn test_an_answer_three_calls_down_is_the_stop_its_caller_catches_at_either_tier() {
    for gas_limit in TIERS {
        assert_answered_cell(Answered::Precompile, gas_limit);
    }
}

/// An inspector's answer is taken as given, as revm's inspected frame start takes it: gas detention
/// holds the engine's answers to the compute limit, and a frame the inspector answers never
/// reaches the engine's frame start. An inspector's answer to `D`, three calls down, that spent all
/// it was forwarded, past what the compute limit leaves the frame, is no stop: `D` ends as the
/// inspector answered it, and what it spent is `C`'s compute. Every caller then resumes on what the
/// limit leaves the transaction, nothing, and crosses it at its next charge, its `POP`: the
/// inspector sees each end with the stop, and the stop of the transaction's own frame is the
/// transaction's result, which reports the compute at that crossing — the read, the calls down and
/// the answer's forward, past the limit — and bills it.
#[test]
fn test_an_inspectors_answer_past_the_allowance_is_taken_as_given() {
    for gas_limit in TIERS {
        let row = format!("gas limit {gas_limit}");
        let intrinsic = intrinsic(gas_limit);
        let (limits, _) = answered_limits(Answered::PastTheAllowance);
        let run = run_answered(Answered::PastTheAllowance, limits, gas_limit, false);
        let forward = run.forward.expect("the inspector answered the call to D");
        let used = 2 + to_the_answer(Answered::PastTheAllowance) + forward;
        let stop = LimitCheck::ExceedsLimit {
            kind: LimitKind::ComputeGas,
            limit: 2 + CAP,
            used,
            frame_local: false,
        };
        assert_eq!(run.outcome.limit_exceeded, Some(stop), "{row}: the transaction's own stop");
        match &run.outcome.result {
            ExecutionResult::Revert { output, .. } => {
                assert_eq!(output, &stop.revert_data(), "{row}: the stop's revert data")
            }
            other => panic!("{row}: expected the stop, got {other:?}"),
        }
        assert_eq!(run.outcome.gas.regular, intrinsic.gas.regular + used, "{row}: regular");
        let stopped = |frame| (frame, InstructionResult::Revert, stop.revert_data());
        assert_eq!(
            run.ended,
            [(D, InstructionResult::Stop, Bytes::new()), stopped(C), stopped(B), stopped(A)],
            "{row}: D ended as answered, then every caller returned the stop"
        );
    }
}

/// A frame three calls down whose start's records and transfer log cross the data-size limit is
/// answered with the stop once revm answered it, and what its start moved goes back with it: no
/// value stays moved and no account is added. Its caller catches the stop, below the execution
/// cap and above it, with and without the rewriting inspector, whose success keeps what the start
/// was charged at its caller's opcode.
#[test]
fn test_a_start_three_calls_down_crossing_the_limit_is_stopped_at_its_start() {
    for gas_limit in TIERS {
        assert_answered_cell(Answered::StartRecords, gas_limit);
    }
}

/* ---------- a stopped answer gives its caller back its share ---------- */

/// A start revm answers without building a frame — a value call to an account with no code — and
/// whose records cross a budget gives way to the stop on the pools the frame would have started
/// with, which go back to its caller as a failed frame's do: the caller then holds what it held
/// before the call, and a write of its own past its budget is stopped by that budget.
///
/// `B`, given a budget of nine slots, records or growth units by `A`, fills it, sends one wei to an
/// account that does not exist, whose start's records and new account cross the share of nothing
/// it gets, and catches that stop; its next fresh slot crosses its own budget of nine, and `A`
/// returns that stop. In each of the three counted dimensions, below the execution cap and above
/// it.
#[test]
fn test_a_stopped_answer_gives_its_caller_back_its_share() {
    const OWN: u64 = 9;
    // The call from `CALLER` to `A` carries no value and no calldata: the transaction's own frame
    // gets what the limit leaves of the body, and `B` the share of that. The smallest data-size
    // limit that leaves `B` nine records is the body and nine records' bytes scaled back up by the
    // share, rounded up.
    let records = OWN * WRITE_RECORD_SIZE;
    let data_size_limit = TX_BODY_SIZE +
        (records * FRAME_DATA_SHARE_DENOMINATOR).div_ceil(FRAME_DATA_SHARE_NUMERATOR);
    let limits = [
        (LimitKind::DataSize, records, data_size_limit),
        (LimitKind::KvUpdate, OWN, 10),
        (LimitKind::StateGrowth, OWN, 10),
    ];
    let mut b = BytecodeBuilder::default();
    for slot in 1..=OWN {
        b = b.sstore(U256::from(slot), U256::from(1));
    }
    let b = b
        .call(C, U256::from(1))
        .append(POP)
        .sstore(U256::from(OWN + 1), U256::from(1))
        .stop()
        .build();
    let a = BytecodeBuilder::default().call(B, U256::ZERO).append(POP).return_returndata().build();
    for (kind, budget, limit) in limits {
        let mut under = EvmTxRuntimeLimits::default();
        under = match kind {
            LimitKind::DataSize => under.with_tx_data_size_limit(limit),
            LimitKind::KvUpdate => under.with_tx_kv_update_limit(limit),
            LimitKind::StateGrowth => under.with_tx_state_growth_limit(limit),
            LimitKind::ComputeGas => unreachable!(),
        };
        let held =
            |remaining: u64| remaining * FRAME_DATA_SHARE_NUMERATOR / FRAME_DATA_SHARE_DENOMINATOR;
        let own_frame = if kind == LimitKind::DataSize { limit - TX_BODY_SIZE } else { limit };
        assert_eq!(held(own_frame), budget, "{kind:?}: B's budget");
        for gas_limit in TIERS {
            let row = format!("{kind:?}, gas limit {gas_limit}");
            let db = MemoryDatabase::default()
                .account_code(A, a.clone())
                .account_code(B, b.clone())
                .account_balance(B, U256::from(1));
            let outcome = execute(db, under, gas_limit);
            assert!(outcome.result.is_success(), "{row}: {:?}", outcome.result);
            let stop = LimitCheck::ExceedsLimit { kind, limit: budget, used: 0, frame_local: true };
            assert_eq!(
                outcome.result.output(),
                Some(&stop.revert_data()),
                "{row}: B's own slot crossed its budget, not a pool the answer lifted"
            );
            assert_eq!(outcome.limit_exceeded, None, "{row}: A caught B's stop");
            assert_eq!(outcome.usage, LimitUsage { data_size: TX_BODY_SIZE, ..LimitUsage::ZERO });
        }
    }
}
