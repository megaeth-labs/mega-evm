//! How a callee's charges draw on the EIP-8037 reservoir it inherits from its caller.
//!
//! Under one gas pool, the forward bounds everything a callee spends. Under Satin a call forwards
//! regular gas, at most `remaining − remaining × 2 / 100` of what its caller has left, and hands
//! the callee the same share of its caller's reservoir whatever regular gas it asks for; the
//! callee's state and history charges draw that share before its regular gas. These tests measure
//! how far a callee's charges reach past the forward, and what that leaves its caller.
//!
//! The contracts:
//! - `A`, the transaction's target, reads `GAS`, calls `C` with an explicit forward (or all it may,
//!   asking for its `GAS`), reads `GAS` again, then writes `K` fresh slots of its own and returns.
//! - `C` is `STOP`; `INVALID`, which burns its forward and charges nothing else; or a loop of one
//!   charging opcode — `LOG0` with data, `LOG4` without data, or a fresh `SSTORE` — that reads
//!   `GAS` around the opcode and returns once an iteration's drop exceeds the opcode's regular cost
//!   (the reservoir no longer paid the whole charge), or once its regular gas is below what one
//!   more iteration costs with the whole charge spilled onto regular gas. The greedy variant never
//!   stops on the drop. One more `C` `CREATE`s a contract of zero bytes and stops.
//!
//! `K` is the work a single pool's forward rule would guarantee `A`: the most fresh slots `A`
//! completes after a callee that spent exactly its forward of regular gas and charged the
//! reservoir nothing, which is what the `INVALID` callee does: its halt hands its share of the
//! reservoir back whole. Each slot is a new slot, so the state-growth limit holds `K` to the 1,000
//! the transaction may add, and the slots `C` keeps come out of them; below that `A`'s gas binds
//! `K`, and the work fails once `C`'s charges, from both pools together, take more than the room
//! the work leaves.
//!
//! Every figure is predicted by a model of the two pools ([`Pools`]) and the three counted budgets
//! ([`Budget`]) built from the spec's rules and the static opcode costs — a regular charge draws
//! regular gas only, a state or history charge draws the reservoir first and spills onto regular
//! gas, `GAS` reads regular gas; a call forwards at most all but 2 per cent of its caller's regular
//! gas and hands the child all but 2 per cent of its caller's reservoir; the child hands back what
//! it leaves, its reservoir unwinding the caller's spill before the rest joins what the caller
//! kept, and a frame that reverts or halts gives its charges back, its share of the reservoir
//! with them; a child's data-size, KV and state-growth budgets are 98/100 of what its caller has
//! left, and a charge that crosses one reverts the frame with the stop — and each run is held to
//! it: the forward, the share, the charges `C` made, the pools it left, `A`'s `GAS` around the
//! call, whether `A`'s work completed, and the receipt with its ledgers and counts. The figures are
//! read by an inspector, and the same run without one settles the same. The tests print their
//! tables (`--nocapture`); a frame's end shows its gas before its caller settles it, so a reverted
//! `C`'s history column is what it charged before the revert gave it back.
//!
//! They pin the spec's numbers, and return early at other byte prices.

use alloy_primitives::{address, Address, Bytes, U256};
use mega_evm::{
    constants::{
        COST_PER_STATE_BYTE, MAX_CONTRACT_SIZE, TX_DATA_LIMIT, TX_GAS_LIMIT_CAP,
        TX_KV_UPDATE_LIMIT, TX_STATE_GROWTH_LIMIT,
    },
    decode_mega_limit_exceeded,
    test_utils::MemoryDatabase,
    LimitCheck, LimitKind, LimitUsage, MegaContext, MegaEvm, MegaTransaction,
    MegaTransactionOutcome, TX_BODY_SIZE, WRITE_RECORD_SIZE,
};
use revm::{
    bytecode::opcode::{
        ADD, CALL, CREATE, DUP1, DUP2, DUP3, GAS, GT, INVALID, ISZERO, JUMP, JUMPDEST, JUMPI, LOG0,
        LOG4, LT, MSTORE, POP, PUSH0, PUSH1, PUSH2, PUSH20, PUSH3, PUSH4, PUSH6, RETURN, SSTORE,
        STOP, SUB, SWAP1,
    },
    context::result::ExecutionResult,
    interpreter::{
        interpreter::EthInterpreter, interpreter_types::Jumps, CallInputs, CallOutcome,
        CreateInputs, CreateOutcome, InstructionResult, Interpreter,
    },
    Database, Inspector,
};

use crate::common::{
    account_state_gas, body_history, call, context, execute, forward_cap, history,
    reservoir_giving, reservoir_share, runs_at_measurement_prices, share, slot_state_gas,
};

const SENDER: Address = address!("0000000000000000000000000000000000710000");
const A: Address = address!("0000000000000000000000000000000000710001");
const C: Address = address!("0000000000000000000000000000000000710002");

/// The bytes of data each `LOG0` of the log-heavy callee carries.
const LOG_DATA: u64 = 512;

/* ---------- static costs ---------- */

// revm's Amsterdam table, which Satin keeps.

/// `GAS`, `PUSH0`, `POP`.
const G_BASE: u64 = 2;
/// `PUSHn`, `DUPn`, `SWAPn`, `ADD`, `SUB`, `LT`, `GT`, `ISZERO`, `MSTORE`.
const G_VERYLOW: u64 = 3;
/// `JUMPDEST`.
const G_JUMPDEST: u64 = 1;
/// `JUMP`.
const G_JUMP: u64 = 8;
/// `JUMPI`.
const G_JUMPI: u64 = 10;
/// A `LOG`'s base cost.
const G_LOG: u64 = 375;
/// A `LOG`'s cost per topic.
const G_LOG_TOPIC: u64 = 375;
/// A `LOG`'s cost per byte of data.
const G_LOG_DATA: u64 = 8;
/// A fresh `SSTORE` to a cold slot: `sstore_static` 100, `cold_storage_additional_cost` 2,000
/// and `sstore_set_without_load_cost` 10,000.
const G_SSTORE_SET_COLD: u64 = 100 + 2_000 + 10_000;
/// EIP-2200's sentry: an `SSTORE` needs more regular gas than the call stipend.
const SSTORE_SENTRY: u64 = 2_300;
/// A `CALL` without value to a cold account: the warm access 100 and the cold surcharge 2,900.
const G_CALL_COLD: u64 = 100 + 2_900;
/// The intrinsic regular gas of a call with no calldata: `TX_BASE_COST` 12,000 and 3,000 for the
/// recipient's access.
const INTRINSIC: u64 = 12_000 + 3_000;

/// The gas limits of the sweep.
const SWEEP: [u64; 5] = [400_000_000, 800_000_000, 1_200_000_000, 1_600_000_000, 2_000_000_000];

/* ---------- the pool model ---------- */

/// A frame ran out of gas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Halt;

/// A frame's two pools — its regular gas and the EIP-8037 reservoir — and the state and history
/// gas its charges spilled onto its regular gas, which a returning child's reservoir unwinds
/// first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pools {
    regular: u64,
    reservoir: u64,
    spill: u64,
}

impl Pools {
    const fn new(regular: u64, reservoir: u64) -> Self {
        Self { regular, reservoir, spill: 0 }
    }

    /// A regular charge draws regular gas only.
    fn regular(&mut self, cost: u64) -> Result<(), Halt> {
        self.regular = self.regular.checked_sub(cost).ok_or(Halt)?;
        Ok(())
    }

    /// A state or history charge draws the reservoir first and spills the rest onto regular gas.
    fn draw(&mut self, cost: u64) -> Result<(), Halt> {
        let from_reservoir = cost.min(self.reservoir);
        let spill = cost - from_reservoir;
        if spill > self.regular {
            return Err(Halt);
        }
        self.reservoir -= from_reservoir;
        self.regular -= spill;
        self.spill += spill;
        Ok(())
    }

    /// A call's start forwarding `gas` of regular gas: the child gets it and the reservoir's
    /// share, all but this frame's 2 per cent, whatever `gas` is, and starts with no spill.
    fn forward(&mut self, gas: u64) -> Self {
        let reservoir = reservoir_share(self.reservoir);
        self.regular -= gas;
        self.reservoir -= reservoir;
        Self::new(gas, reservoir)
    }

    /// A child's return, handing back `child`: its regular gas comes back, a kept child's spill
    /// joins this frame's, and the reservoir it hands back unwinds this frame's spill onto its
    /// regular gas before the rest joins what this frame kept.
    fn absorb(&mut self, child: Self, kept: bool) {
        self.regular += child.regular;
        if kept {
            self.spill += child.spill;
        }
        let unwound = child.reservoir.min(self.spill);
        self.regular += unwound;
        self.spill -= unwound;
        self.reservoir += child.reservoir - unwound;
    }
}

/// The pools the transaction's own frame starts with: the regular gas the execution cap leaves
/// once the intrinsic gas is paid, and the gas limit above the cap. The body's history is paid
/// before the frame, from the reservoir first, so none of it is a spill of the frame's.
fn first_frame(gas_limit: u64) -> Pools {
    let mut pools = Pools::new(
        gas_limit.min(TX_GAS_LIMIT_CAP) - INTRINSIC,
        gas_limit.saturating_sub(TX_GAS_LIMIT_CAP),
    );
    pools.draw(body_history(0)).expect("the body fits");
    Pools { spill: 0, ..pools }
}

/// A limit's stop: the dimension crossed, the budget the frame was given, which the stop names,
/// and the frame's count with the crossing charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stop {
    kind: LimitKind,
    limit: u64,
    used: u64,
}

/// A frame's data-size, KV and state-growth budgets: what it was given and what it has left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Budget {
    given: LimitUsage,
    left: LimitUsage,
}

impl Budget {
    /// The transaction's own frame's: what the transaction's limits leave once its body is
    /// counted.
    const fn first_frame() -> Self {
        let given = LimitUsage {
            data_size: TX_DATA_LIMIT - TX_BODY_SIZE,
            write_records: TX_KV_UPDATE_LIMIT,
            state_growth: TX_STATE_GROWTH_LIMIT,
        };
        Self { given, left: given }
    }

    /// No budget at all.
    const fn unlimited() -> Self {
        let given =
            LimitUsage { data_size: u64::MAX, write_records: u64::MAX, state_growth: u64::MAX };
        Self { given, left: given }
    }

    /// A child's: 98/100 of what this frame has left in each dimension, rounded down.
    fn child(&self) -> Self {
        let given = LimitUsage {
            data_size: share(self.left.data_size),
            write_records: share(self.left.write_records),
            state_growth: share(self.left.state_growth),
        };
        Self { given, left: given }
    }

    /// Counts `counted`, or stops on the first dimension it crosses, in the order data size, KV
    /// updates, state growth, counting nothing.
    fn count(&mut self, counted: LimitUsage) -> Result<(), Stop> {
        let dimensions = [
            (LimitKind::DataSize, counted.data_size, self.left.data_size, self.given.data_size),
            (
                LimitKind::KvUpdate,
                counted.write_records,
                self.left.write_records,
                self.given.write_records,
            ),
            (
                LimitKind::StateGrowth,
                counted.state_growth,
                self.left.state_growth,
                self.given.state_growth,
            ),
        ];
        for (kind, add, left, limit) in dimensions {
            if add > left {
                return Err(Stop { kind, limit, used: limit - left + add });
            }
        }
        self.merge(counted);
        Ok(())
    }

    /// Takes `kept` out of what this frame has left: what it counted, or what a child that
    /// returned kept.
    fn merge(&mut self, kept: LimitUsage) {
        self.left.data_size -= kept.data_size;
        self.left.write_records -= kept.write_records;
        self.left.state_growth -= kept.state_growth;
    }
}

/// The sum of two counts.
const fn plus(a: LimitUsage, b: LimitUsage) -> LimitUsage {
    LimitUsage {
        data_size: a.data_size + b.data_size,
        write_records: a.write_records + b.write_records,
        state_growth: a.state_growth + b.state_growth,
    }
}

/// `a` less `b`: what a frame counted of a budget `a` when it has `b` left.
const fn minus(a: LimitUsage, b: LimitUsage) -> LimitUsage {
    LimitUsage {
        data_size: a.data_size - b.data_size,
        write_records: a.write_records - b.write_records,
        state_growth: a.state_growth - b.state_growth,
    }
}

/// What a fresh slot counts: its write record's bytes, the record, and the new slot.
const SLOT_COUNTS: LimitUsage =
    LimitUsage { data_size: WRITE_RECORD_SIZE, write_records: 1, state_growth: 1 };

/// What the transaction counts before its frame: its body.
const BODY_COUNTS: LimitUsage =
    LimitUsage { data_size: TX_BODY_SIZE, write_records: 0, state_growth: 0 };

/* ---------- the callee ---------- */

/// The charging opcode the callee's loop repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Charge {
    /// `LOG0` of [`LOG_DATA`] bytes: history only.
    Log0Data,
    /// `LOG4` of no data: history only.
    Log4,
    /// A fresh `SSTORE`: state and history.
    FreshSlot,
}

impl Charge {
    fn name(self) -> &'static str {
        match self {
            Self::Log0Data => "LOG0 512 B",
            Self::Log4 => "LOG4 0 B",
            Self::FreshSlot => "fresh SSTORE",
        }
    }

    fn opcode(self) -> u8 {
        match self {
            Self::Log0Data => LOG0,
            Self::Log4 => LOG4,
            Self::FreshSlot => SSTORE,
        }
    }

    /// Code that runs once before the loop: `LOG0` reads its data from memory expanded here, so
    /// no iteration pays an expansion.
    fn prologue(self) -> Vec<u8> {
        match self {
            Self::Log0Data => {
                let [hi, lo] = ((LOG_DATA - 32) as u16).to_be_bytes();
                vec![PUSH0, PUSH2, hi, lo, MSTORE]
            }
            Self::Log4 | Self::FreshSlot => vec![],
        }
    }

    fn prologue_regular(self) -> u64 {
        match self {
            Self::Log0Data => {
                let words = LOG_DATA / 32;
                G_BASE + 2 * G_VERYLOW + 3 * words + words * words / 512
            }
            Self::Log4 | Self::FreshSlot => 0,
        }
    }

    /// The opcodes that push the charging opcode's operands, run with the loop's `[i, g0]` on the
    /// stack; the opcode follows and leaves the stack so.
    fn operands(self) -> Vec<u8> {
        match self {
            Self::Log0Data => {
                let [hi, lo] = (LOG_DATA as u16).to_be_bytes();
                vec![PUSH2, hi, lo, PUSH0]
            }
            Self::Log4 => vec![PUSH0; 6],
            // `SSTORE(key = i, value = 1)`: the counter is a fresh slot each iteration.
            Self::FreshSlot => vec![PUSH1, 1, DUP3],
        }
    }

    fn operands_regular(self) -> u64 {
        match self {
            Self::Log0Data => G_VERYLOW + G_BASE,
            Self::Log4 => 6 * G_BASE,
            Self::FreshSlot => 2 * G_VERYLOW,
        }
    }

    /// The charging opcode's regular gas.
    fn opcode_regular(self) -> u64 {
        match self {
            Self::Log0Data => G_LOG + G_LOG_DATA * LOG_DATA,
            Self::Log4 => G_LOG + 4 * G_LOG_TOPIC,
            Self::FreshSlot => G_SSTORE_SET_COLD,
        }
    }

    /// The regular gas of one charge, operands included: what an iteration's `GAS` drop is when
    /// the reservoir paid the whole charge, less the closing `GAS`.
    fn body_regular(self) -> u64 {
        self.operands_regular() + self.opcode_regular()
    }

    /// The bytes one charge appends: a log's 32 for its address and 32 per topic plus its data,
    /// a write record's 40.
    fn bytes(self) -> u64 {
        match self {
            Self::Log0Data => 32 + LOG_DATA,
            Self::Log4 => 32 + 4 * 32,
            Self::FreshSlot => WRITE_RECORD_SIZE,
        }
    }

    /// What one charge counts against the frame's budgets: a log its bytes, a fresh slot its
    /// record's bytes, the record and the new slot.
    fn counts(self) -> LimitUsage {
        match self {
            Self::Log0Data | Self::Log4 => {
                LimitUsage { data_size: self.bytes(), write_records: 0, state_growth: 0 }
            }
            Self::FreshSlot => SLOT_COUNTS,
        }
    }

    /// The state gas of one charge.
    fn state(self) -> u64 {
        match self {
            Self::Log0Data | Self::Log4 => 0,
            Self::FreshSlot => slot_state_gas(),
        }
    }

    /// The history gas of one charge.
    fn history(self) -> u64 {
        history(self.bytes())
    }

    /// The regular gas the callee keeps before another iteration: that iteration with its whole
    /// charge spilled onto regular gas, and the way out.
    fn reserve(self) -> u64 {
        self.body_regular() + LOOP_REGULAR + self.state() + self.history() + 64
    }
}

// The callee's loop: its regular gas outside the charge.

/// `JUMPDEST GAS`, which opens an iteration.
const LOOP_ENTRY: u64 = G_JUMPDEST + G_BASE;
/// `DUP1 PUSH4 GT PUSH2 JUMPI`: out when the regular gas left is below the reserve.
const RESERVE_CHECK: u64 = 4 * G_VERYLOW + G_JUMPI;
/// `SWAP1 SUB PUSH4 LT PUSH2 JUMPI`: out when the drop exceeds the charge's regular gas.
const DROP_CHECK: u64 = 5 * G_VERYLOW + G_JUMPI;
/// `PUSH1 ADD PUSH2 JUMP`: the counter, and back to the top.
const LOOP_NEXT: u64 = 3 * G_VERYLOW + G_JUMP;
/// One iteration's regular gas outside the charge, the closing `GAS` included.
const LOOP_REGULAR: u64 = LOOP_ENTRY + G_BASE + RESERVE_CHECK + DROP_CHECK + LOOP_NEXT;

/// What `C` runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Callee {
    /// `STOP`: returns at once, charging nothing.
    Stop,
    /// `INVALID`: halts, burning its whole forward and charging nothing else. It spends what a
    /// single pool's forward rule lets a callee spend, and no more.
    Burn,
    /// The loop of `charge`; `stop_on_spill` stops it on the first iteration the reservoir did
    /// not pay in full.
    Loop { charge: Charge, stop_on_spill: bool },
}

impl Callee {
    const fn stops_on_spill(charge: Charge) -> Self {
        Self::Loop { charge, stop_on_spill: true }
    }

    const fn greedy(charge: Charge) -> Self {
        Self::Loop { charge, stop_on_spill: false }
    }

    fn name(self) -> String {
        match self {
            Self::Stop => "STOP".into(),
            Self::Burn => "INVALID".into(),
            Self::Loop { charge, stop_on_spill: true } => charge.name().into(),
            Self::Loop { charge, stop_on_spill: false } => format!("{}, greedy", charge.name()),
        }
    }

    fn charge(self) -> Option<Charge> {
        match self {
            Self::Stop | Self::Burn => None,
            Self::Loop { charge, .. } => Some(charge),
        }
    }

    fn code(self) -> Bytes {
        let (charge, stop_on_spill) = match self {
            Self::Stop => return Bytes::from_static(&[STOP]),
            Self::Burn => return Bytes::from_static(&[INVALID]),
            Self::Loop { charge, stop_on_spill } => (charge, stop_on_spill),
        };
        // The drop that stops the loop: the charge's regular gas and the closing `GAS`.
        let stop_above =
            if stop_on_spill { charge.body_regular() + G_BASE } else { u64::from(u32::MAX) };
        let u32_bytes = |value: u64| u32::try_from(value).expect("fits a PUSH4").to_be_bytes();

        let mut code = charge.prologue();
        code.push(PUSH0);
        let top = code.len();
        code.extend([JUMPDEST, GAS]);
        code.extend(charge.operands());
        code.push(charge.opcode());
        // [i, g0, g1]: out when g1 < reserve.
        code.extend([GAS, DUP1, PUSH4]);
        code.extend(u32_bytes(charge.reserve()));
        code.extend([GT, PUSH2, 0, 0, JUMPI]);
        let out_on_reserve = code.len() - 3;
        // [i, g0 - g1]: out when the drop exceeds the charge's regular gas.
        code.extend([SWAP1, SUB, PUSH4]);
        code.extend(u32_bytes(stop_above));
        code.extend([LT, PUSH2, 0, 0, JUMPI]);
        let out_on_drop = code.len() - 3;
        code.extend([PUSH1, 1, ADD, PUSH2]);
        code.extend((top as u16).to_be_bytes());
        code.push(JUMP);
        let out = (code.len() as u16).to_be_bytes();
        code.extend([JUMPDEST, STOP]);
        code[out_on_reserve..out_on_reserve + 2].copy_from_slice(&out);
        code[out_on_drop..out_on_drop + 2].copy_from_slice(&out);
        code.into()
    }
}

/// How `C` ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalleeEnd {
    Returned,
    /// A charge crossed one of `C`'s budgets: `C` reverts with the stop.
    Crossed(Stop),
    Halted,
}

/// What the model says `C` does.
#[derive(Clone, Copy, Debug)]
struct CalleeModel {
    end: CalleeEnd,
    /// Charging opcodes started, the one that crossed or halted included.
    charges: u64,
    /// `C`'s pools at its end, before the revert of a crossing gives its charges back.
    at_end: Pools,
    /// The pools `C` hands back to `A`.
    handed_back: Pools,
    /// The state and history gas of the charges `C` kept.
    state: u64,
    history: u64,
    /// What `C` kept of its budgets.
    kept: LimitUsage,
}

/// `C` run on the pools `start` and the budgets `budget` its start handed it.
fn model_callee(callee: Callee, start: Pools, budget: Budget) -> CalleeModel {
    let (charge, stop_on_spill) = match callee {
        Callee::Stop | Callee::Burn => {
            // `INVALID` charged nothing, so its halt hands the share back whole.
            let (end, handed_back) = match callee {
                Callee::Stop => (CalleeEnd::Returned, start),
                _ => (CalleeEnd::Halted, Pools::new(0, start.reservoir)),
            };
            return CalleeModel {
                end,
                charges: 0,
                at_end: handed_back,
                handed_back,
                state: 0,
                history: 0,
                kept: LimitUsage::ZERO,
            };
        }
        Callee::Loop { charge, stop_on_spill } => (charge, stop_on_spill),
    };
    let mut pools = start;
    let mut budget = budget;
    let mut charges = 0;
    let (mut state, mut history) = (0, 0);
    let end = (|| -> Result<CalleeEnd, Halt> {
        pools.regular(charge.prologue_regular() + G_BASE)?;
        loop {
            pools.regular(LOOP_ENTRY)?;
            let g0 = pools.regular;
            pools.regular(charge.operands_regular())?;
            charges += 1;
            if charge == Charge::FreshSlot && pools.regular <= SSTORE_SENTRY {
                return Err(Halt);
            }
            pools.regular(charge.opcode_regular())?;
            pools.draw(charge.state())?;
            // The completed-write hook counts the write before it charges its history.
            if let Err(stop) = budget.count(charge.counts()) {
                return Ok(CalleeEnd::Crossed(stop));
            }
            pools.draw(charge.history())?;
            state += charge.state();
            history += charge.history();
            pools.regular(G_BASE)?;
            let g1 = pools.regular;
            pools.regular(RESERVE_CHECK)?;
            if g1 < charge.reserve() {
                pools.regular(G_JUMPDEST)?;
                return Ok(CalleeEnd::Returned);
            }
            pools.regular(DROP_CHECK)?;
            if stop_on_spill && g0 - g1 > charge.body_regular() + G_BASE {
                pools.regular(G_JUMPDEST)?;
                return Ok(CalleeEnd::Returned);
            }
            pools.regular(LOOP_NEXT)?;
        }
    })()
    .unwrap_or(CalleeEnd::Halted);
    let (handed_back, kept) = match end {
        CalleeEnd::Returned => (pools, minus(budget.given, budget.left)),
        // The revert gives the charges back: what the reservoir paid to the share, what spilled
        // to the regular gas it hands back.
        CalleeEnd::Crossed(_) => {
            (Pools::new(pools.regular + pools.spill, start.reservoir), LimitUsage::ZERO)
        }
        CalleeEnd::Halted => (Pools::new(0, start.reservoir), LimitUsage::ZERO),
    };
    let (state, history) = if end == CalleeEnd::Returned { (state, history) } else { (0, 0) };
    CalleeModel { end, charges, at_end: pools, handed_back, state, history, kept }
}

/* ---------- the caller ---------- */

/// The gas `A` forwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Forward {
    Explicit(u64),
    /// `GAS` as the call's gas, more than the cap: all but 2 per cent of what is left after the
    /// call's cost.
    All,
}

impl Forward {
    fn name(self) -> String {
        match self {
            Self::Explicit(gas) => gas.to_string(),
            Self::All => "all but 2/100".into(),
        }
    }
}

/// `A`'s code, writing `work` fresh slots after the call.
fn caller_code(forward: Forward, work: u64) -> Bytes {
    let mut code = vec![GAS, PUSH0, PUSH0, PUSH0, PUSH0, PUSH0, PUSH20];
    code.extend(C);
    match forward {
        Forward::Explicit(gas) => {
            code.push(PUSH4);
            code.extend(u32::try_from(gas).expect("fits a PUSH4").to_be_bytes());
        }
        Forward::All => code.push(GAS),
    }
    code.extend([CALL, GAS, PUSH4]);
    code.extend(u32::try_from(work).expect("fits a PUSH4").to_be_bytes());
    // [gas0, ok, gas1, k]: while k != 0, `SSTORE(k, 1)` and k -= 1.
    let top = code.len() as u16;
    code.extend([JUMPDEST, DUP1, ISZERO, PUSH2, 0, 0, JUMPI]);
    let out_at = code.len() - 3;
    code.extend([PUSH1, 1, DUP2, SSTORE, PUSH1, 1, SWAP1, SUB, PUSH2]);
    code.extend(top.to_be_bytes());
    code.push(JUMP);
    let out = (code.len() as u16).to_be_bytes();
    code[out_at..out_at + 2].copy_from_slice(&out);
    // Returns `gas0`, `ok` and `gas1`, a word each.
    code.extend([JUMPDEST, POP, PUSH1, 0x40, MSTORE, PUSH1, 0x20, MSTORE, PUSH0, MSTORE]);
    code.extend([PUSH1, 0x60, PUSH0, RETURN]);
    code.into()
}

/// `A`'s regular gas before the `SSTORE` of one fresh slot of its work: `JUMPDEST DUP1 ISZERO
/// PUSH2 JUMPI PUSH1 DUP2`.
const WORK_BEFORE: u64 = G_JUMPDEST + 5 * G_VERYLOW + G_JUMPI;
/// `A`'s regular gas after the `SSTORE`: `PUSH1 SWAP1 SUB PUSH2 JUMP`.
const WORK_AFTER: u64 = 4 * G_VERYLOW + G_JUMP;
/// The way out: the last `JUMPDEST DUP1 ISZERO PUSH2 JUMPI`, `JUMPDEST POP`, three `MSTORE`s with
/// their pushes and the expansion to three words, and `PUSH1 PUSH0 RETURN`.
const WORK_OUT: u64 = (G_JUMPDEST + 3 * G_VERYLOW + G_JUMPI) +
    (G_JUMPDEST + G_BASE) +
    (2 * (G_VERYLOW + G_VERYLOW) + G_BASE + G_VERYLOW + 3 * 3) +
    (G_VERYLOW + G_BASE);

/// How `A`'s work ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkEnd {
    /// `A` returns, on these pools.
    Completed(Pools),
    /// A charge ran out of gas: the transaction halts.
    OutOfGas,
    /// A slot crossed one of `A`'s budgets: `A` reverts with the stop, on these pools.
    Stopped(Stop, Pools),
}

impl WorkEnd {
    const fn completed(self) -> bool {
        matches!(self, Self::Completed(_))
    }
}

/// How `A`'s work of `slots` fresh slots, run on `pools` and `budget` after its second `GAS`,
/// ends.
fn model_work(slots: u64, mut pools: Pools, mut budget: Budget) -> WorkEnd {
    let mut work = || -> Result<WorkEnd, Halt> {
        pools.regular(G_VERYLOW)?;
        for _ in 0..slots {
            pools.regular(WORK_BEFORE)?;
            if pools.regular <= SSTORE_SENTRY {
                return Err(Halt);
            }
            pools.regular(G_SSTORE_SET_COLD)?;
            pools.draw(slot_state_gas())?;
            if let Err(stop) = budget.count(SLOT_COUNTS) {
                return Ok(WorkEnd::Stopped(stop, pools));
            }
            pools.draw(history(WRITE_RECORD_SIZE))?;
            pools.regular(WORK_AFTER)?;
        }
        pools.regular(WORK_OUT)?;
        Ok(WorkEnd::Completed(pools))
    };
    work().unwrap_or(WorkEnd::OutOfGas)
}

/// The most fresh slots `A` completes on `pools` and `budget`.
fn capacity(pools: Pools, budget: Budget) -> u64 {
    let bound = (pools.regular / G_SSTORE_SET_COLD).min(budget.left.state_growth);
    let (mut fits, mut fails) = (0, bound + 1);
    while fails - fits > 1 {
        let mid = (fits + fails) / 2;
        if model_work(mid, pools, budget).completed() {
            fits = mid;
        } else {
            fails = mid;
        }
    }
    fits
}

/// One run: a gas limit, the gas `A` forwards and what `C` runs.
#[derive(Clone, Copy, Debug)]
struct Scenario {
    gas_limit: u64,
    forward: Forward,
    callee: Callee,
}

/// What the model says the run does.
#[derive(Clone, Copy, Debug)]
struct Prediction {
    /// The reservoir once the body is paid: what `A` holds at its call.
    reservoir_at_call: u64,
    gas_before_call: u64,
    regular_at_call: u64,
    /// The regular gas and the reservoir the call hands `C`.
    handed: Pools,
    /// `C`'s budgets: 98/100 of what `A` has left, which is the transaction's limits less the
    /// body.
    budget: Budget,
    callee: CalleeModel,
    /// `A`'s pools once `C` returned, before its second `GAS`.
    pools_after_return: Pools,
    gas_after_call: u64,
    pools_after_call: Pools,
    /// `A`'s budgets once `C` returned.
    budget_after_call: Budget,
    /// `A`'s work: unless a run names it, the most fresh slots `A` completes after a callee that
    /// spent exactly its forward of regular gas and charged the reservoir nothing.
    work: u64,
    /// The most fresh slots `A` could complete after the call as it happened.
    capacity_after_call: u64,
    /// How `A`'s work ends.
    end: WorkEnd,
}

fn predict(scenario: Scenario, work: Option<u64>) -> Prediction {
    let Scenario { gas_limit, forward, callee } = scenario;
    let mut pools = first_frame(gas_limit);
    let reservoir_at_call = pools.reservoir;
    pools.regular(G_BASE).unwrap();
    let gas_before_call = pools.regular;
    let gas_operand = match forward {
        Forward::Explicit(_) => G_VERYLOW,
        Forward::All => G_BASE,
    };
    pools.regular(5 * G_BASE + G_VERYLOW + gas_operand).unwrap();
    let regular_at_call = pools.regular;
    pools.regular(G_CALL_COLD).unwrap();
    let cap = forward_cap(pools.regular);
    let forward = match forward {
        Forward::Explicit(gas) => gas.min(cap),
        Forward::All => cap,
    };
    let handed = pools.forward(forward);
    let mut budget_after_call = Budget::first_frame();
    let budget = budget_after_call.child();
    let callee = model_callee(callee, handed, budget);

    // After a callee that burns its forward and charges the reservoir nothing.
    let mut kept_by_a_single_pool = pools;
    kept_by_a_single_pool.absorb(Pools::new(0, handed.reservoir), false);
    kept_by_a_single_pool.regular(G_BASE).unwrap();
    let work = work.unwrap_or_else(|| capacity(kept_by_a_single_pool, budget_after_call));

    pools.absorb(callee.handed_back, callee.end == CalleeEnd::Returned);
    budget_after_call.merge(callee.kept);
    let pools_after_return = pools;
    pools.regular(G_BASE).unwrap();
    Prediction {
        reservoir_at_call,
        gas_before_call,
        regular_at_call,
        handed,
        budget,
        callee,
        pools_after_return,
        gas_after_call: pools.regular,
        pools_after_call: pools,
        budget_after_call,
        work,
        capacity_after_call: capacity(pools, budget_after_call),
        end: model_work(work, pools, budget_after_call),
    }
}

/* ---------- the meter ---------- */

/// What `C`'s frame held when it returned to `A`.
#[derive(Clone, Debug)]
struct Returned {
    result: InstructionResult,
    output: Bytes,
    pools: Pools,
    history: i64,
    state: i64,
}

/// A frame's pools as an inspector reads them.
fn pools_of(gas: &revm::interpreter::Gas) -> Pools {
    Pools { regular: gas.remaining(), reservoir: gas.reservoir(), spill: gas.state_gas_spilled() }
}

/// Reads `A`'s `GAS` around the call, its pools at the call and after it, what the call hands
/// `C`, what a creation `C` makes is handed, the charges `C` starts and what its frame holds at
/// its return.
#[derive(Clone, Debug, Default)]
struct Meter {
    charge: Option<u8>,
    frames: usize,
    gas_pending: bool,
    gas_before_call: Option<u64>,
    gas_after_call: Option<u64>,
    at_call: Option<Pools>,
    handed: Option<Pools>,
    created_with: Option<Pools>,
    returned: Option<Returned>,
    after_return: Option<Pools>,
    charges: u64,
}

impl<DB: Database> Inspector<MegaContext<DB>, EthInterpreter> for Meter {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        let opcode = interp.bytecode.opcode();
        match self.frames {
            1 => {
                if self.returned.is_some() && self.after_return.is_none() {
                    self.after_return = Some(pools_of(&interp.gas));
                }
                if opcode == CALL && self.at_call.is_none() {
                    self.at_call = Some(pools_of(&interp.gas));
                }
                self.gas_pending = opcode == GAS;
            }
            2 if Some(opcode) == self.charge => self.charges += 1,
            _ => {}
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut MegaContext<DB>) {
        if !core::mem::take(&mut self.gas_pending) {
            return;
        }
        let gas = interp.stack.peek(0).expect("GAS pushed its answer").to::<u64>();
        if self.returned.is_some() {
            self.gas_after_call.get_or_insert(gas);
        } else {
            self.gas_before_call.get_or_insert(gas);
        }
    }

    fn call(&mut self, _: &mut MegaContext<DB>, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.frames += 1;
        if inputs.target_address == C {
            self.handed = Some(Pools::new(inputs.gas_limit, inputs.reservoir));
        }
        None
    }

    fn call_end(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if inputs.target_address == C {
            let gas = &outcome.result.gas;
            self.returned = Some(Returned {
                result: outcome.result.result,
                output: outcome.result.output.clone(),
                pools: pools_of(gas),
                history: gas.history_gas_spent(),
                state: gas.state_gas_spent(),
            });
        }
        self.frames -= 1;
    }

    fn create(
        &mut self,
        _: &mut MegaContext<DB>,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        self.frames += 1;
        self.created_with = Some(Pools::new(inputs.gas_limit(), inputs.reservoir()));
        None
    }

    fn create_end(&mut self, _: &mut MegaContext<DB>, _: &CreateInputs, _: &mut CreateOutcome) {
        self.frames -= 1;
    }
}

/* ---------- the run ---------- */

/// One run's measured figures.
#[derive(Clone, Debug)]
struct Measured {
    scenario: Scenario,
    prediction: Prediction,
    gas_before_call: u64,
    gas_after_call: u64,
    reservoir_at_call: u64,
    handed: Pools,
    reservoir_after_call: u64,
    callee: Returned,
    charges: u64,
    outcome: MegaTransactionOutcome,
}

impl Measured {
    fn forward(&self) -> u64 {
        self.prediction.handed.regular
    }

    /// The regular gas `C` spent of its forward, spills included: all of it when `C` halted.
    fn callee_regular(&self) -> u64 {
        if self.callee.result.is_halt() {
            return self.forward();
        }
        self.forward() - self.callee.pools.regular
    }

    /// The reservoir `C` consumed: what `A` held at the call less what it holds after it.
    fn consumed(&self) -> u64 {
        self.reservoir_at_call - self.reservoir_after_call
    }

    fn completed(&self) -> bool {
        self.outcome.result.is_success()
    }

    fn callee_end(&self) -> String {
        match self.callee.result {
            InstructionResult::Stop => "returned".into(),
            InstructionResult::Revert => match decode_mega_limit_exceeded(&self.callee.output) {
                Some((kind, limit)) => format!("reverted: {kind:?} stop at {limit}"),
                None => "reverted".into(),
            },
            other => format!("{other:?}"),
        }
    }

    fn status(&self) -> String {
        status(&self.outcome)
    }

    /// Whether `A`'s work needs the reservoir: it does not fit in `A`'s regular gas after the
    /// call alone.
    fn work_needs_reservoir(&self) -> bool {
        needs_reservoir(self.prediction.work, self.gas_after_call)
    }
}

/// Whether `slots` fresh slots of `A`'s work run out of gas on `regular` gas alone, whatever the
/// budgets.
fn needs_reservoir(slots: u64, regular: u64) -> bool {
    model_work(slots, Pools::new(regular, 0), Budget::unlimited()) == WorkEnd::OutOfGas
}

fn status(outcome: &MegaTransactionOutcome) -> String {
    match &outcome.result {
        ExecutionResult::Success { .. } => "success".into(),
        ExecutionResult::Revert { output, .. } => match decode_mega_limit_exceeded(output) {
            Some((kind, limit)) => format!("revert: {kind:?} stop at {limit}"),
            None => "revert".into(),
        },
        ExecutionResult::Halt { reason, .. } => format!("halt: {reason:?}"),
    }
}

/// Runs `tx` on `db` under the meter, and again without it, and holds the two to the same
/// settlement.
#[track_caller]
fn execute_metered(
    db: impl Fn() -> MemoryDatabase,
    tx: impl Fn() -> MegaTransaction,
    meter: Meter,
    name: &str,
) -> (MegaTransactionOutcome, Meter) {
    let mut evm = MegaEvm::new(context(db())).with_inspector(meter);
    let outcome = evm.execute_transaction(tx()).expect("the transaction is valid");
    let meter = evm.inspector().clone();
    let plain = execute(db(), tx());
    assert_eq!(plain.result.is_success(), outcome.result.is_success(), "{name}: the inspector");
    assert_eq!(plain.gas, outcome.gas, "{name}: the ledgers under the inspector");
    assert_eq!(plain.usage, outcome.usage, "{name}: the usage under the inspector");
    (outcome, meter)
}

/// Holds `outcome` to how the model says `A`'s work ends: `C` kept `callee`'s charges and counts
/// and `A`'s work its slots, or the transaction ended in a halt or a stop that kept neither.
#[track_caller]
fn assert_settled(
    name: &str,
    outcome: &MegaTransactionOutcome,
    scenario: Scenario,
    p: &Prediction,
    callee: (u64, u64, LimitUsage),
) {
    let (callee_state, callee_history, callee_kept) = callee;
    let body = body_history(0);
    let nothing_kept = |what: &str| {
        assert_eq!(
            outcome.gas.reservoir_remaining, p.reservoir_at_call,
            "{name}: the reservoir comes back whole on {what}"
        );
        assert_eq!(outcome.gas.history, body, "{name}: the body alone on {what}");
        assert_eq!(outcome.gas.state, 0, "{name}: no state on {what}");
        assert_eq!(outcome.usage, BODY_COUNTS, "{name}: the body's counts alone on {what}");
    };
    match p.end {
        WorkEnd::Completed(left) => {
            assert!(
                outcome.result.is_success(),
                "{name}: A's work completes: {:?}",
                outcome.result
            );
            assert_eq!(
                outcome.gas.history,
                body + callee_history + p.work * history(WRITE_RECORD_SIZE),
                "{name}: the history ledger"
            );
            assert_eq!(
                outcome.gas.state,
                callee_state + p.work * slot_state_gas(),
                "{name}: the state ledger"
            );
            assert_eq!(
                outcome.gas.reservoir_remaining, left.reservoir,
                "{name}: the reservoir left"
            );
            assert_eq!(
                outcome.gas.gas_used,
                scenario.gas_limit - left.regular - left.reservoir,
                "{name}: the receipt"
            );
            let work_counts = LimitUsage {
                data_size: p.work * WRITE_RECORD_SIZE,
                write_records: p.work,
                state_growth: p.work,
            };
            assert_eq!(
                outcome.usage,
                plus(plus(BODY_COUNTS, callee_kept), work_counts),
                "{name}: the counts"
            );
            assert_eq!(outcome.limit_exceeded, None, "{name}: no stop");
        }
        WorkEnd::OutOfGas => {
            assert!(
                matches!(outcome.result, ExecutionResult::Halt { .. }),
                "{name}: A's work runs out of gas: {:?}",
                outcome.result
            );
            // The halt gives A's state and history back, C's merged charges included, and burns
            // the regular gas: the receipt is the execution cap and the body.
            nothing_kept("a halt");
            assert_eq!(
                outcome.gas.gas_used,
                scenario.gas_limit - p.reservoir_at_call,
                "{name}: the receipt"
            );
        }
        WorkEnd::Stopped(stop, at) => {
            assert!(
                matches!(outcome.result, ExecutionResult::Revert { .. }),
                "{name}: A's work is stopped: {:?}",
                outcome.result
            );
            assert_eq!(
                decode_mega_limit_exceeded(outcome.result.output().unwrap()),
                Some((stop.kind, stop.limit)),
                "{name}: the stop A reverts with"
            );
            assert_eq!(
                outcome.limit_exceeded,
                Some(LimitCheck::ExceedsLimit {
                    kind: stop.kind,
                    limit: stop.limit,
                    used: stop.used,
                    frame_local: true
                }),
                "{name}: the stop the outcome names"
            );
            // The revert gives A's state and history back, C's merged charges included, the
            // spill to the regular gas, which comes back to the sender with the reservoir.
            nothing_kept("a stop");
            assert_eq!(
                outcome.gas.gas_used,
                scenario.gas_limit - p.reservoir_at_call - at.regular - at.spill,
                "{name}: the receipt"
            );
        }
    }
}

/// Runs `scenario` with and without the meter, holds both runs to the model, and returns what
/// the meter read.
#[track_caller]
fn run(scenario: Scenario) -> Measured {
    run_with(scenario, None)
}

/// [`run`], with `A` writing `work` fresh slots when it is named.
#[track_caller]
fn run_with(scenario: Scenario, work: Option<u64>) -> Measured {
    let prediction = predict(scenario, work);
    let name = format!(
        "{} at a {} gas limit, forwarding {}",
        scenario.callee.name(),
        scenario.gas_limit,
        scenario.forward.name()
    );
    let db = || {
        MemoryDatabase::default()
            .account_balance(SENDER, U256::from(10u64.pow(18)))
            .account_code(A, caller_code(scenario.forward, prediction.work))
            .account_code(C, scenario.callee.code())
    };
    let tx = || call(SENDER, A, U256::ZERO, scenario.gas_limit);

    let meter =
        Meter { charge: scenario.callee.charge().map(Charge::opcode), ..Default::default() };
    let (outcome, meter) = execute_metered(db, tx, meter, &name);

    let after_return = meter.after_return.expect("A resumed");
    let measured = Measured {
        scenario,
        prediction,
        gas_before_call: meter.gas_before_call.expect("A read GAS before the call"),
        gas_after_call: meter.gas_after_call.expect("A read GAS after the call"),
        reservoir_at_call: meter.at_call.expect("A called").reservoir,
        handed: meter.handed.expect("A called C"),
        reservoir_after_call: after_return.reservoir,
        callee: meter.returned.clone().expect("C returned"),
        charges: meter.charges,
        outcome,
    };
    let p = &prediction;

    // The reservoir is the gas limit above the execution cap, less the body [S4.4].
    assert_eq!(
        meter.at_call,
        Some(Pools::new(p.regular_at_call, p.reservoir_at_call)),
        "{name}: A's pools at the call"
    );
    // `GAS` reads the regular budget, not the reservoir.
    assert_eq!(measured.gas_before_call, p.gas_before_call, "{name}: A's GAS before the call");
    // A call forwards at most all but 2 per cent of the regular gas, and the child gets all but
    // 2 per cent of the reservoir whatever the call asks for.
    assert_eq!(measured.handed, p.handed, "{name}: what the call hands C");
    assert_eq!(measured.charges, p.callee.charges, "{name}: the charges C started");
    match p.callee.end {
        CalleeEnd::Returned => {
            assert_eq!(measured.callee.result, InstructionResult::Stop, "{name}: C returns");
            assert_eq!(
                measured.callee.history, p.callee.history as i64,
                "{name}: the history C charged"
            );
            assert_eq!(measured.callee.state, p.callee.state as i64, "{name}: the state C charged");
            assert_eq!(
                measured.callee.pools, p.callee.handed_back,
                "{name}: the pools C hands back"
            );
        }
        CalleeEnd::Crossed(stop) => {
            assert_eq!(measured.callee.result, InstructionResult::Revert, "{name}: C reverts");
            assert_eq!(
                decode_mega_limit_exceeded(&measured.callee.output),
                Some((stop.kind, stop.limit)),
                "{name}: C's budget stopped it"
            );
            // A frame's end shows its gas before the settlement: the history of every charge
            // before the crossing one, which the caller's merge then gives back to the reservoir.
            let charge = scenario.callee.charge().unwrap();
            let charged = (p.callee.charges - 1) * charge.history();
            assert_eq!(measured.callee.history, charged as i64, "{name}: the history C charged");
            assert_eq!(measured.callee.pools, p.callee.at_end, "{name}: C's pools at its end");
        }
        CalleeEnd::Halted => {
            assert_eq!(scenario.callee, Callee::Burn, "{name}: only INVALID halts C");
            assert_eq!(
                measured.callee.result,
                InstructionResult::InvalidFEOpcode,
                "{name}: C halts"
            );
        }
    }
    // What C hands back, its reservoir unwinding A's spill first, is what A holds after the call.
    assert_eq!(after_return, p.pools_after_return, "{name}: A's pools after the call");
    assert_eq!(measured.gas_after_call, p.gas_after_call, "{name}: A's GAS after the call");

    let callee = (p.callee.state, p.callee.history, p.callee.kept);
    assert_settled(&name, &measured.outcome, scenario, p, callee);
    if p.end.completed() {
        let words: Vec<U256> = measured
            .outcome
            .result
            .output()
            .expect("A returns")
            .chunks(32)
            .map(U256::from_be_slice)
            .collect();
        assert_eq!(
            words,
            [
                U256::from(p.gas_before_call),
                U256::from(u64::from(p.callee.end == CalleeEnd::Returned)),
                U256::from(p.gas_after_call)
            ],
            "{name}: A's GAS around the call and the call's flag, as A read them"
        );
    }
    measured
}

/* ---------- the tables ---------- */

fn ratio(numerator: u64, denominator: u64) -> String {
    format!("{:.2}", numerator as f64 / denominator as f64)
}

fn print_table(title: &str, rows: &[Measured]) {
    println!("\n### {title}\n");
    println!(
        "| gas limit | C | forward | charges | C regular spent | C history charged | \
         C state charged | reservoir at call | reservoir to C | reservoir at return | consumed | \
         consumed / forward | (regular + consumed) / forward | A GAS before | A GAS after | \
         A work (slots) | work needs reservoir | A capacity after | C end | status | gas used |"
    );
    println!("|{}", "---:|".repeat(21));
    for row in rows {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | \
             {} | {} | {} | {} | {} |",
            row.scenario.gas_limit,
            row.scenario.callee.name(),
            row.forward(),
            row.charges,
            row.callee_regular(),
            row.callee.history,
            row.callee.state,
            row.reservoir_at_call,
            row.handed.reservoir,
            row.reservoir_after_call,
            row.consumed(),
            ratio(row.consumed(), row.forward()),
            ratio(row.callee_regular() + row.consumed(), row.forward()),
            row.gas_before_call,
            row.gas_after_call,
            row.prediction.work,
            if row.work_needs_reservoir() { "yes" } else { "no" },
            row.prediction.capacity_after_call,
            row.callee_end(),
            row.status(),
            row.outcome.gas.gas_used,
        );
    }
}

/* ---------- the measurements ---------- */

/// One slot of `A`'s work, from both pools: its regular gas and its state and history.
fn slot_cost() -> u64 {
    WORK_BEFORE + G_SSTORE_SET_COLD + WORK_AFTER + slot_state_gas() + history(WRITE_RECORD_SIZE)
}

/// With a 400M gas limit and a forward of 1M, a callee that returns at once leaves `A` the whole
/// reservoir, and a callee that logs until its regular gas runs low charges ten times its forward
/// to its share of it.
///
/// `A`'s work is the 1,000 slots the state-growth limit lets the transaction add, which `A`'s
/// regular gas pays for alone, so it completes after the log-heavy callee as after one that returns
/// at once, and the slot past it is the stop, kind 3, not an out-of-gas. A callee's charges still
/// reach `A`'s work where the callee spends both its shares: forwarding all, the greedy callee
/// leaves `A` the 2 per cent of its regular gas and of its reservoir that `A` kept, and the 329
/// slots `A` completes after a callee that burns all it was forwarded run out of gas.
#[test]
fn test_a_log_heavy_callee_charges_past_its_forward_and_leaves_two_percent_of_each_pool() {
    if runs_at_measurement_prices() {
        return;
    }
    let scenario =
        |callee| Scenario { gas_limit: 400_000_000, forward: Forward::Explicit(1_000_000), callee };
    let baseline = run(scenario(Callee::Stop));
    let heavy = run(scenario(Callee::stops_on_spill(Charge::Log0Data)));
    // The work is what a callee that spends exactly its forward leaves room for, and no more.
    let burns = run(scenario(Callee::Burn));
    let one_more = run_with(scenario(Callee::Burn), Some(burns.prediction.work + 1));
    print_table(
        "1a. Baseline vs. a log-heavy callee (400M, forward 1M)",
        &[baseline.clone(), heavy.clone(), burns.clone(), one_more.clone()],
    );
    assert!(burns.completed(), "A's work completes after a callee that burns its forward");
    assert_eq!(burns.prediction.work, TX_STATE_GROWTH_LIMIT, "the state-growth limit binds it");
    assert_eq!(
        decode_mega_limit_exceeded(one_more.outcome.result.output().unwrap()),
        Some((LimitKind::StateGrowth, TX_STATE_GROWTH_LIMIT)),
        "one slot more is the stop"
    );

    assert_eq!(baseline.consumed(), 0);
    assert!(baseline.completed(), "A's work completes after a callee that charged nothing");
    // The Rex6 comparison in `mega-evme`'s tests runs this callee's bytes on the released
    // engine, with these two numbers written out.
    assert_eq!(Charge::Log0Data.body_regular() + G_BASE, 4_478, "the drop that stops C");
    assert_eq!(Charge::Log0Data.reserve(), 52_481, "the reserve C keeps");

    assert!(heavy.callee_regular() <= heavy.forward(), "regular gas stays within the forward");
    assert!(heavy.reservoir_after_call > 0, "the forward ran out before the share");
    assert_eq!(heavy.consumed(), heavy.charges * Charge::Log0Data.history());
    assert!(
        heavy.consumed() > 10 * heavy.forward(),
        "the share pays ten times the forward: {}",
        heavy.consumed()
    );
    assert!(
        heavy.callee_regular() + heavy.consumed() > heavy.forward() + slot_cost(),
        "C charged more than its forward and one slot"
    );
    assert!(!heavy.work_needs_reservoir(), "A's regular gas pays for the work alone");
    assert!(heavy.completed(), "so C's charges do not reach it");

    // Forwarding all, after a callee that burns its forward and after one that spends both
    // shares.
    let all = |callee| Scenario { gas_limit: 400_000_000, forward: Forward::All, callee };
    let burns_all = run(all(Callee::Burn));
    let work = burns_all.prediction.work;
    let greedy = run_with(all(Callee::greedy(Charge::Log0Data)), Some(work));
    print_table("1b. Forwarding all but 2/100 (400M)", &[burns_all.clone(), greedy.clone()]);
    assert!(burns_all.completed());
    assert!(work < TX_STATE_GROWTH_LIMIT, "A's regular gas binds it: {work}");
    assert_eq!(greedy.callee.result, InstructionResult::Stop, "C returns");
    assert_eq!(greedy.callee.pools.reservoir, 0, "C spent its whole share");
    assert!(greedy.callee.pools.regular < Charge::Log0Data.reserve(), "and its forward");
    assert_eq!(
        greedy.reservoir_after_call,
        greedy.reservoir_at_call * 2 / 100,
        "A keeps its 2 per cent of the reservoir"
    );
    assert!(
        greedy.callee_regular() + greedy.consumed() > greedy.forward() + slot_cost(),
        "C charged more than its forward and one slot"
    );
    assert!(!greedy.completed(), "the work A completes after a burned forward fails");
    assert_eq!(greedy.prediction.end, WorkEnd::OutOfGas, "out of gas");
}

/// The log-heavy callee at gas limits from 400M to 2B. A forward of 1M bounds it by its regular gas
/// at every limit: it consumes the same reservoir everywhere. Forwarding all but 2 per cent, it
/// consumes its whole share of the reservoir until the share outgrows what its 98/100 data-size
/// budget lets it log; from there its budget stops it first, it reverts, and its history comes back
/// to `A`. The threshold is exact: one gas less of reservoir and the share runs out first.
///
/// The threshold is the gas limit whose share pays for the logs the budget holds, 1,353,400,537;
/// below it the callee leaves `A` its 2 per cent, and at 400M its share is just below its forward.
/// Either side of the threshold the 2 per cent `A` kept and the regular gas the callee left pay for
/// more than the 1,000 slots the state-growth limit lets `A` add, so that work, which needs the
/// reservoir, completes on both sides.
#[test]
fn test_the_frame_budget_binds_a_callee_once_the_reservoir_outgrows_it() {
    if runs_at_measurement_prices() {
        return;
    }
    let callee = Callee::stops_on_spill(Charge::Log0Data);
    let at = |gas_limit, forward| run(Scenario { gas_limit, forward, callee });

    let bounded: Vec<_> =
        SWEEP.iter().map(|&limit| at(limit, Forward::Explicit(1_000_000))).collect();
    for row in &bounded {
        assert_eq!(row.consumed(), bounded[0].consumed(), "the forward bounds what C logs");
        assert_eq!(row.callee.result, InstructionResult::Stop);
    }

    // The budget stops the log that takes C past it, before its history is charged. The share
    // pays every earlier log in full once it holds their history.
    let budget = share(TX_DATA_LIMIT - TX_BODY_SIZE);
    let kept = budget / Charge::Log0Data.bytes();
    let threshold =
        TX_GAS_LIMIT_CAP + body_history(0) + reservoir_giving(kept * Charge::Log0Data.history());

    let mut all: Vec<_> = SWEEP.iter().map(|&limit| at(limit, Forward::All)).collect();
    all.push(at(threshold - 1, Forward::All));
    all.push(at(threshold, Forward::All));
    all.sort_by_key(|row| row.scenario.gas_limit);
    for row in &all {
        assert_eq!(row.prediction.budget.given.data_size, budget, "C's data-size budget");
        if row.scenario.gas_limit < threshold {
            assert_eq!(row.callee.result, InstructionResult::Stop, "C returns");
            assert_eq!(row.consumed(), row.handed.reservoir, "C consumed its whole share");
            assert_eq!(
                row.reservoir_after_call,
                row.reservoir_at_call * 2 / 100,
                "A keeps its 2 per cent"
            );
            assert_eq!(
                row.consumed() > row.forward(),
                row.scenario.gas_limit > 400_000_000,
                "more than its forward, once its share outgrows A's regular gas"
            );
        } else {
            assert_eq!(row.charges, kept + 1, "the crossing log is the one past the budget");
            assert_eq!(
                decode_mega_limit_exceeded(&row.callee.output),
                Some((LimitKind::DataSize, budget)),
                "C's budget stops it"
            );
            assert_eq!(row.consumed(), 0, "C's history comes back to the reservoir");
            assert!(row.completed(), "A's work completes");
        }
    }
    assert!(1_200_000_000 < threshold && threshold <= 1_600_000_000, "{threshold}");

    // `A`'s own work in those rows is bound by its regular gas, so it completes on either side.
    // The most `A` may add needs the reservoir on both sides, and the reservoir `A` kept below the
    // threshold pays for it.
    let below = all.iter().find(|row| row.scenario.gas_limit == threshold - 1).unwrap();
    assert_eq!(below.prediction.capacity_after_call, TX_STATE_GROWTH_LIMIT);
    let gas_alone = capacity(below.prediction.pools_after_call, Budget::unlimited());
    assert!(gas_alone > TX_STATE_GROWTH_LIMIT, "A's gas pays for more: {gas_alone}");
    let at_work = |gas_limit| {
        run_with(Scenario { gas_limit, forward: Forward::All, callee }, Some(TX_STATE_GROWTH_LIMIT))
    };
    let below = at_work(threshold - 1);
    let at_threshold = at_work(threshold);
    assert!(below.work_needs_reservoir() && at_threshold.work_needs_reservoir());
    assert!(below.completed(), "the 2 per cent A kept pays for what its regular gas does not");
    assert!(at_threshold.completed(), "C's crossing gave the share back");

    print_table("2a. Gas-limit sweep, forward 1M", &bounded);
    print_table("2b. Gas-limit sweep, forward all but 2/100 (threshold rows included)", &all);
    print_table(
        "2c. Either side of the threshold, A's work needing the reservoir",
        &[below, at_threshold],
    );
    println!(
        "\nC's data-size budget: {budget} bytes, {kept} logs; threshold gas limit {threshold}; \
         A's capacity below it on gas alone: {gas_alone} slots"
    );
}

/// Without a reservoir every charge spills onto the callee's regular gas, so the forward bounds all
/// of it: the callee that stops on a spill logs once, the greedy one logs until its forward runs
/// low, and `A`'s work completes after either. At the execution cap that work is the 1,000 slots
/// the state-growth limit allows.
#[test]
fn test_without_a_reservoir_the_forward_bounds_every_charge() {
    if runs_at_measurement_prices() {
        return;
    }
    let mut rows = Vec::new();
    for gas_limit in [TX_GAS_LIMIT_CAP, 100_000_000] {
        for callee in [Callee::stops_on_spill(Charge::Log0Data), Callee::greedy(Charge::Log0Data)] {
            let row = run(Scenario { gas_limit, forward: Forward::Explicit(1_000_000), callee });
            assert_eq!(row.reservoir_at_call, 0);
            assert_eq!(row.handed.reservoir, 0);
            assert_eq!(row.consumed(), 0);
            assert_eq!(row.callee.history, (row.charges * Charge::Log0Data.history()) as i64);
            assert!(
                row.callee.history as u64 <= row.callee_regular() &&
                    row.callee_regular() + row.consumed() <= row.forward(),
                "every charge C made, its history included, came out of its forward"
            );
            assert!(row.completed(), "A's work completes");
            rows.push(row);
        }
    }
    assert_eq!(rows[0].charges, 1, "the first log spills");
    assert!(rows[1].charges > 1, "the greedy callee logs until its forward runs low");
    assert_eq!(rows[0].prediction.work, TX_STATE_GROWTH_LIMIT, "the state-growth limit binds");
    assert!(rows[2].prediction.work < TX_STATE_GROWTH_LIMIT, "the gas binds");
    print_table("3. No reservoir (forward 1M)", &rows);
}

/// The forwards of the forward-size tables.
fn forwards() -> [Forward; 4] {
    [
        Forward::Explicit(100_000),
        Forward::Explicit(1_000_000),
        Forward::Explicit(10_000_000),
        Forward::All,
    ]
}

/// One charge's callee, on the forwards from 100k to all but 2 per cent, at 400M.
#[track_caller]
fn forward_sizes(charge: Charge) -> Vec<Measured> {
    forwards()
        .into_iter()
        .map(|forward| {
            run(Scenario {
                gas_limit: 400_000_000,
                forward,
                callee: Callee::stops_on_spill(charge),
            })
        })
        .collect()
}

/// Holds the explicit forwards' rows: regular gas within the forward, and a reservoir consumed
/// past it that grows with it.
#[track_caller]
fn assert_past_the_forward(explicit: &[Measured]) {
    for row in explicit {
        assert!(row.callee_regular() <= row.forward(), "regular gas stays within the forward");
        assert!(row.consumed() > row.forward(), "the share pays more than the forward");
    }
    assert!(
        explicit.windows(2).all(|pair| pair[0].consumed() < pair[1].consumed()),
        "the reservoir consumed grows with the forward"
    );
}

/// The reservoir a log-heavy callee consumes against its forward, for `LOG0` with data and `LOG4`
/// without: past the forward at every explicit size, growing with it, and its whole share
/// forwarding all.
///
/// Forwarding all, the callee consumes its share, which at 400M is 98/100 of a reservoir just below
/// `A`'s regular gas, so just under the forward, and `A` keeps its 2 per cent.
#[test]
fn test_the_reservoir_a_logging_callee_consumes_grows_with_its_forward() {
    if runs_at_measurement_prices() {
        return;
    }
    let log0 = forward_sizes(Charge::Log0Data);
    let log4 = forward_sizes(Charge::Log4);
    for rows in [&log0, &log4] {
        let (all, explicit) = rows.split_last().unwrap();
        assert_past_the_forward(explicit);
        assert!(all.consumed() > explicit.last().unwrap().consumed());
        assert_eq!(all.callee.result, InstructionResult::Stop, "C returns");
        assert_eq!(all.consumed(), all.handed.reservoir, "C consumes its whole share");
        assert_eq!(all.reservoir_after_call, all.reservoir_at_call * 2 / 100, "A keeps 2 per cent");
        assert!(all.consumed() < all.forward(), "just under the forward at 400M");
    }
    print_table("4a. Forward sizes, LOG0 with 512 bytes of data (400M)", &log0);
    print_table("4b. Forward sizes, LOG4 with no data (400M)", &log4);
}

/// The same with fresh `SSTORE`s, whose state and history both draw the reservoir: past the forward
/// at every explicit size and growing with it, until the callee's state-growth budget, 98/100 of
/// the 1,000 new slots the transaction may add, stops it.
///
/// Forwarding all, the callee's 981st slot crosses its budget of 980 before the share runs out: it
/// reverts, and its charges and the share come back to `A`, which consumes nothing of the
/// reservoir. The slots the callee keeps at an explicit forward come out of the 1,000 too, so `A`'s
/// 1,000 are stopped by the limit after them.
#[test]
fn test_a_writing_callees_state_growth_budget_stops_it_before_its_share_runs_out() {
    if runs_at_measurement_prices() {
        return;
    }
    let rows = forward_sizes(Charge::FreshSlot);
    let (all, explicit) = rows.split_last().unwrap();
    assert_past_the_forward(explicit);
    for row in explicit {
        assert_eq!(
            row.consumed(),
            row.charges * (Charge::FreshSlot.state() + Charge::FreshSlot.history())
        );
        assert_eq!(
            row.prediction.budget_after_call.left.state_growth,
            TX_STATE_GROWTH_LIMIT - row.charges,
            "C's slots come out of A's"
        );
    }
    let growth = share(TX_STATE_GROWTH_LIMIT);
    assert_eq!(all.prediction.budget.given.state_growth, growth);
    assert!(
        all.handed.reservoir > (growth + 1) * (slot_state_gas() + history(WRITE_RECORD_SIZE)),
        "the share would pay for the slot past the budget"
    );
    assert_eq!(all.charges, growth + 1, "the crossing slot is the one past the budget");
    assert_eq!(
        decode_mega_limit_exceeded(&all.callee.output),
        Some((LimitKind::StateGrowth, growth)),
        "C's state-growth budget stops it"
    );
    assert_eq!(all.consumed(), 0, "C's charges and its share come back to A");
    assert!(all.completed(), "A's work completes");
    print_table("5. Forward sizes, fresh SSTOREs (400M)", &rows);
}

/* ---------- a callee that deploys code ---------- */

/// A callee that `CREATE`s a contract of `size` zero bytes, from the init code
/// `PUSH3 size PUSH0 RETURN`, then stops.
fn deploying(size: u64) -> Bytes {
    assert!(size < 1 << 24, "{size} fits a PUSH3");
    let [_, high, middle, low] = (size as u32).to_be_bytes();
    let mut code = vec![PUSH6, PUSH3, high, middle, low, PUSH0, RETURN];
    // The init code lands right-aligned in the first word: bytes 26 to 31.
    code.extend([PUSH0, MSTORE, PUSH1, 6, PUSH1, 26, PUSH0, CREATE, POP, STOP]);
    code.into()
}

/// The history of a creation's two write records: the created account and the creator's nonce.
fn creation_records_history() -> u64 {
    history(2 * WRITE_RECORD_SIZE)
}

/// What the creation pays for depositing `size` bytes: each byte's state and history gas.
fn deposit_charges(size: u64) -> u64 {
    size * COST_PER_STATE_BYTE + history(size)
}

/// What the reservoir pays for a creation that deposits `size` bytes: the created account's
/// state gas and the history of its two write records, which its creator pays at the start, and
/// the deposit.
fn deployment_charges(size: u64) -> u64 {
    account_state_gas() + creation_records_history() + deposit_charges(size)
}

/// What a creation that deposits `size` bytes counts: its two records with their bytes, the
/// created account, and the code's bytes.
const fn deployment_counts(size: u64) -> LimitUsage {
    LimitUsage { data_size: 2 * WRITE_RECORD_SIZE + size, write_records: 2, state_growth: 1 }
}

/// A code deposit charges each byte 1,530 state gas and 88 history gas against a few regular gas of
/// memory and hashing, so a callee that deploys code draws hundreds to thousands of times its
/// forward from the reservoir: 96 KiB on a forward of 100,000 and on one of 1,000,000, which
/// consume the same, and the largest contract on a forward of 1,000,000.
///
/// `C`'s regular gas is not modelled here: the reservoir it consumes is held to the schedule, its
/// regular gas to its forward, and `A`'s work, sized as in the other measurements, to the pools and
/// the budgets the call left it.
///
/// The creation's start charges `C` the account and the records and hands the creation all but 2
/// per cent of what that leaves of `C`'s share, which pays the deposit. `A`'s work is the 1,000
/// slots the state-growth limit allows, and it fails on that limit, the created account having
/// taken one of them, not on gas: the 999 slots the limit leaves `A` fit in its regular gas alone.
#[test]
fn test_a_deploying_callee_charges_the_reservoir_hundreds_of_times_its_forward() {
    if runs_at_measurement_prices() {
        return;
    }
    println!("\n### 7. A callee that deploys code\n");
    println!(
        "| gas limit | forward | code size | C regular spent | reservoir at call | \
         reservoir to C | reservoir to the creation | reservoir at return | consumed | \
         consumed / forward | A GAS after | A work (slots) | A capacity after | \
         work needs reservoir | status | gas used |"
    );
    println!("|{}", "---:|".repeat(16));
    for (gas_limit, forward, size) in [
        (400_000_000, 100_000, 96 * 1024),
        (400_000_000, 1_000_000, 96 * 1024),
        (1_200_000_000, 1_000_000, MAX_CONTRACT_SIZE as u64),
    ] {
        let name = format!("{size} bytes at a {gas_limit} gas limit, forwarding {forward}");
        // `A` runs as it does before a callee that returns at once.
        let scenario =
            Scenario { gas_limit, forward: Forward::Explicit(forward), callee: Callee::Stop };
        let p = predict(scenario, None);
        let db = || {
            MemoryDatabase::default()
                .account_balance(SENDER, U256::from(10u64.pow(18)))
                .account_code(A, caller_code(Forward::Explicit(forward), p.work))
                .account_code(C, deploying(size))
        };
        let tx = || call(SENDER, A, U256::ZERO, gas_limit);
        let (outcome, meter) = execute_metered(db, tx, Meter::default(), &name);

        assert_eq!(meter.gas_before_call, Some(p.gas_before_call), "{name}: A's GAS before");
        assert_eq!(meter.handed, Some(p.handed), "{name}: what the call hands C");
        let created_with = meter.created_with.expect("C created");
        assert_eq!(
            created_with.reservoir,
            reservoir_share(p.handed.reservoir - account_state_gas() - creation_records_history()),
            "{name}: the creation's share"
        );
        assert!(deposit_charges(size) <= created_with.reservoir, "{name}: the share pays it");
        let returned = meter.returned.clone().expect("C returned");
        assert_eq!(returned.result, InstructionResult::Stop, "{name}: C returns");
        let callee_regular = forward - returned.pools.regular;
        assert!(callee_regular <= forward, "{name}: regular gas stays within the forward");
        let after = meter.after_return.expect("A resumed");
        let consumed = p.reservoir_at_call - after.reservoir;
        assert_eq!(consumed, deployment_charges(size), "{name}: the reservoir paid the deployment");
        assert!(consumed > 100 * forward, "{name}: a hundred times the forward: {consumed}");

        let mut after_gas = after;
        after_gas.regular(G_BASE).unwrap();
        assert_eq!(meter.gas_after_call, Some(after_gas.regular), "{name}: A's GAS after");
        let mut budget = Budget::first_frame();
        budget.merge(deployment_counts(size));
        let left = capacity(after_gas, budget);
        assert_eq!(p.work, TX_STATE_GROWTH_LIMIT, "{name}: the state-growth limit binds the work");
        assert_eq!(left, TX_STATE_GROWTH_LIMIT - 1, "{name}: the created account took one");
        assert!(!needs_reservoir(left, after_gas.regular), "{name}: A's regular gas pays for it");
        let end = model_work(p.work, after_gas, budget);
        assert!(
            matches!(end, WorkEnd::Stopped(Stop { kind: LimitKind::StateGrowth, .. }, _)),
            "{name}: A's work is stopped on the state-growth limit: {end:?}"
        );
        let callee = (
            account_state_gas() + size * COST_PER_STATE_BYTE,
            creation_records_history() + history(size),
            deployment_counts(size),
        );
        assert_settled(&name, &outcome, scenario, &Prediction { end, ..p }, callee);

        let status = status(&outcome);
        println!(
            "| {gas_limit} | {forward} | {size} | {callee_regular} | {} | {} | {} | {} | \
             {consumed} | {} | {} | {} | {left} | {} | {status} | {} |",
            p.reservoir_at_call,
            p.handed.reservoir,
            created_with.reservoir,
            after.reservoir,
            ratio(consumed, forward),
            after_gas.regular,
            p.work,
            if needs_reservoir(p.work, after_gas.regular) { "yes" } else { "no" },
            outcome.gas.gas_used,
        );
    }
}
