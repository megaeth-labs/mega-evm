//! Run `MegaEvm` under `revm-inspectors` tracers and compare their output with insta snapshots.
//!
//! The JSON views are sorted JSON snapshots and the EIP-3155 trace is a string snapshot, one file
//! per scenario and view, under `tests/tracers/snapshots/`. A mismatch fails the test. Outside CI,
//! insta also writes the new value beside the old one as a `.snap.new` file.
//!
//! Review a change with `cargo insta review`, which shows each snapshot's diff and accepts or
//! rejects it on its own. Without `cargo-insta`, diff each `.snap.new` the failing run left against
//! its `.snap` and accept it on its own, by moving it over the `.snap` without its
//! `assertion_line:` header line, which insta's own accept leaves out. The comparisons run only at
//! the spec's byte prices.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    io::{self, Write},
    rc::Rc,
};

use alloy_primitives::{Address, Bytes, Log, B256};
use alloy_rpc_types_trace::geth::{
    AccountState, CallConfig, CallFrame, CallLogFrame, DefaultFrame, GethDebugTracingOptions,
    GethDefaultTracingOptions, GethTrace, PreStateConfig, PreStateFrame, PreStateMode, StructLog,
};
use alloy_sol_types::SolError;
use mega_evm::{
    alloy_evm::Evm as _,
    op_revm::constants::L1_BLOCK_CONTRACT,
    test_utils::{
        op_transaction, zero_fee_l1_block_info, MemoryDatabase, RecordingDatabase, WitnessRecord,
    },
    EvmTxRuntimeLimits, LimitCheck, LimitKind, MegaContext, MegaEvm, MegaLimitExceeded, MegaSpecId,
    MegaTransaction, MegaTransactionOutcome, L1_BLOCK_INFO_SLOTS,
};
use revm::{
    context::{result::ExecutionResult, BlockEnv, TxEnv},
    inspector::inspectors::TracerEip3155,
    primitives::{KECCAK_EMPTY, U256},
    Database,
};
use revm_inspectors::tracing::{
    types::{CallTraceNode, CallTraceStep, TraceMemberOrder},
    DebugInspector, TracingInspector, TracingInspectorConfig,
};

use crate::gas::Ledgers;

/// A block with room for any transaction these tests run.
pub(crate) fn block() -> BlockEnv {
    BlockEnv {
        number: U256::from(1),
        timestamp: U256::from(1_700_000_000),
        gas_limit: 10_000_000_000,
        ..Default::default()
    }
}

/// A Satin context over `db` with zero L1 fees.
pub(crate) fn context<DB: Database>(db: DB) -> MegaContext<DB> {
    MegaContext::new(db, MegaSpecId::SATIN).with_block(block()).with_chain(zero_fee_l1_block_info())
}

/// A call from `caller` to `to` carrying `data` and `value`.
pub(crate) fn call_tx(
    caller: Address,
    to: Address,
    data: Bytes,
    value: U256,
    gas_limit: u64,
) -> MegaTransaction {
    mega_evm::alloy_op_evm::OpTx(op_transaction(TxEnv {
        caller,
        kind: revm::primitives::TxKind::Call(to),
        data,
        value,
        gas_limit,
        ..Default::default()
    }))
}

/// A contract creation from `caller` running `init_code`.
pub(crate) fn create_tx(caller: Address, init_code: Bytes, gas_limit: u64) -> MegaTransaction {
    mega_evm::alloy_op_evm::OpTx(op_transaction(TxEnv {
        caller,
        kind: revm::primitives::TxKind::Create,
        data: init_code,
        gas_limit,
        ..Default::default()
    }))
}

/// A writer the EIP-3155 tracer writes into and the test reads back.
#[derive(Clone, Debug, Default)]
struct SharedBuf(Rc<RefCell<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One Satin-engine execution under a tracer that records steps, logs and state diffs.
#[derive(Debug)]
pub(crate) struct Traced {
    /// What the engine reported, including the three gas ledgers.
    pub outcome: MegaTransactionOutcome,
    /// The inspector after the transaction finished.
    pub inspector: TracingInspector,
    /// The pre-state database the prestate tracer reads.
    pub pre_db: MemoryDatabase,
    /// Every account, slot and code the first run read from the pre-state database.
    pub reads: WitnessRecord,
    /// The transaction, kept to run it again under another tracer.
    tx: MegaTransaction,
    /// The runtime limits it ran under.
    limits: EvmTxRuntimeLimits,
}

impl Traced {
    /// Runs `tx` on a fresh Satin EVM over `db`, recording every read the database serves.
    pub(crate) fn run(db: MemoryDatabase, tx: MegaTransaction, limits: EvmTxRuntimeLimits) -> Self {
        let pre_db = db.clone();
        let record = Rc::new(RefCell::new(WitnessRecord::default()));
        let recording = RecordingDatabase::new(db, record.clone());
        let inspector = TracingInspector::new(TracingInspectorConfig::all());
        let mut evm = MegaEvm::new(context(recording).with_tx_runtime_limits(limits))
            .with_inspector(inspector);
        let outcome = evm.execute_transaction(tx.clone()).expect("the transaction is valid");
        let inspector = evm.inspector().clone();
        drop(evm);
        let reads = record.borrow().clone();
        Self { outcome, inspector, pre_db, reads, tx, limits }
    }

    /// The EIP-3155 trace: one JSON line per step, then the summary line.
    ///
    /// The transaction runs again, on a fresh EVM over the same pre-state, under the revm fork's
    /// `TracerEip3155`, which is the tracer the state-test runner runs Satin under.
    pub(crate) fn eip3155(&self) -> String {
        let buf = SharedBuf::default();
        let mut evm =
            MegaEvm::new(context(self.pre_db.clone()).with_tx_runtime_limits(self.limits))
                .with_inspector(TracerEip3155::new(Box::new(buf.clone())));
        let outcome = evm.execute_transaction(self.tx.clone()).expect("the transaction is valid");
        assert_eq!(outcome.result, self.outcome.result, "the EIP-3155 run executed differently");
        let bytes = buf.0.borrow().clone();
        String::from_utf8(bytes).expect("EIP-3155 output is UTF-8")
    }

    /// What a node's `debug_traceTransaction` returns for this transaction under `opts`.
    ///
    /// The transaction runs again, on a fresh EVM over the same pre-state, under revm-inspectors'
    /// `DebugInspector` built from `opts`, and the trace is built by `DebugInspector::get_result`,
    /// which is what reth's debug API calls. It sets the root frame's gas limit and caller to the
    /// transaction's before it builds a view, and hands the call and opcode tracers the receipt's
    /// gas used.
    fn node_trace(&self, opts: GethDebugTracingOptions) -> GethTrace {
        let inspector = DebugInspector::new(opts).expect("a built-in tracer");
        let mut evm =
            MegaEvm::new(context(self.pre_db.clone()).with_tx_runtime_limits(self.limits))
                .with_inspector(inspector);
        let outcome = evm.execute_transaction(self.tx.clone()).expect("the transaction is valid");
        assert_eq!(outcome.result, self.outcome.result, "the traced run executed differently");
        let mut pre_db = self.pre_db.clone();
        evm.inspector_mut()
            .get_result(None, &self.tx, &block(), &outcome.result_and_state, &mut *pre_db)
            .expect("the trace builds")
    }

    /// Geth call tracer output.
    pub(crate) fn call_frame(&self, with_log: bool) -> CallFrame {
        let config = CallConfig { only_top_call: Some(false), with_log: Some(with_log) };
        self.node_trace(GethDebugTracingOptions::call_tracer(config))
            .try_into_call_frame()
            .expect("a call frame")
    }

    /// Geth opcode / struct-log tracer, with the options a node gets when `debug_traceTransaction`
    /// names none: the stack and the storage an `SLOAD` or `SSTORE` touched are kept, memory and
    /// return data are not.
    pub(crate) fn struct_logs(&self) -> DefaultFrame {
        let config = GethDefaultTracingOptions::default();
        self.node_trace(GethDebugTracingOptions { config, ..Default::default() })
            .try_into_default_frame()
            .expect("a struct-log frame")
    }

    /// Geth prestate tracer.
    pub(crate) fn prestate(&self, diff_mode: bool) -> PreStateFrame {
        let config = PreStateConfig {
            diff_mode: Some(diff_mode),
            disable_code: Some(false),
            disable_storage: Some(false),
        };
        self.node_trace(GethDebugTracingOptions::prestate_tracer(config))
            .try_into_pre_state_frame()
            .expect("a prestate frame")
    }

    /// The transaction's gas limit.
    pub(crate) fn gas_limit(&self) -> u64 {
        self.tx.0.base.gas_limit
    }
}

/// The call tracer's `logs` are exactly `expected`, in order.
pub(crate) fn assert_logs(logs: &[CallLogFrame], expected: &[Log]) {
    let got: Vec<_> = logs
        .iter()
        .map(|log| {
            (
                log.address,
                log.topics.clone().unwrap_or_default(),
                log.data.clone().unwrap_or_default(),
            )
        })
        .collect();
    let expected: Vec<_> = expected
        .iter()
        .map(|log| (Some(log.address), log.topics().to_vec(), log.data.data.clone()))
        .collect();
    assert_eq!(got, expected, "the call tracer's logs");
}

/// Known shape: the call tracer numbers a log among every log the transaction emitted, those a
/// failed frame discarded included.
///
/// revm-inspectors gives a log the count of logs it has recorded so far, in every frame, as its
/// `index`, and never takes back the logs of a frame that fails; the call builder then drops a
/// failed frame's logs and keeps the survivors' numbers. So the `index` of a log the receipt
/// keeps is its index in the receipt plus the logs discarded before it — under Satin among them
/// the EIP-7708 transfer log of a value call whose callee fails, and a log a limit refused after
/// the tracer saw it. `discarded_before` gives that count for each of the root frame's logs.
///
/// A tracer that numbers the kept logs only makes this fail: require the receipt's index then.
pub(crate) fn assert_log_index_counts_discarded(traced: &Traced, discarded_before: &[u64]) {
    let root = traced.call_frame(true);
    assert_eq!(root.logs.len(), discarded_before.len(), "the root frame's logs");
    let receipt = traced.outcome.result.logs();
    for (log, discarded) in root.logs.iter().zip(discarded_before) {
        let kept = log.clone().into_log();
        let receipt_index =
            receipt.iter().position(|logged| *logged == kept).expect("the receipt keeps the log");
        assert_eq!(
            log.index,
            Some(receipt_index as u64 + discarded),
            "known shape: the call tracer's log index ({:?}) is the receipt's ({receipt_index}) \
             plus the {discarded} log(s) a failed frame discarded before it; a tracer that numbers \
             kept logs only makes them equal: require the receipt's index here and review the \
             snapshots with `cargo insta review`",
            log.index,
        );
    }
}

/// The struct-log step of the first `op` at `depth` in the node's opcode trace.
pub(crate) fn step(traced: &Traced, op: &str, depth: u64) -> StructLog {
    traced
        .struct_logs()
        .struct_logs
        .into_iter()
        .find(|log| log.op == op && log.depth == depth)
        .unwrap_or_else(|| panic!("no {op} step at depth {depth}"))
}

/// The op lines of the EIP-3155 trace, without the summary.
pub(crate) fn eip3155_steps(traced: &Traced) -> Vec<serde_json::Value> {
    traced
        .eip3155()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("a JSON line"))
        .filter(|line| line.get("opName").is_some())
        .collect()
}

/// The EIP-3155 trace as a string snapshot. The JSON views go through the shared sorted-JSON
/// snapshot, which compares only at the spec's byte prices; this one repeats that, with the same
/// note of the skip, because a line-oriented trace is not JSON.
fn assert_eip3155_snapshot(name: &str, trace: &str) {
    if mega_evm::active_satin_prices().is_constants() {
        insta::assert_snapshot!(name, trace);
    } else {
        mega_evm::test_utils::note_snapshot_skipped();
    }
}

/// Pins every tracer view of `traced` under `scenario`'s name: one snapshot per view.
///
/// The five JSON views are sorted JSON snapshots. The EIP-3155 trace is a string snapshot of its
/// JSON lines. Both comparisons run only at the spec's byte prices.
pub(crate) fn pin_tracer_views(scenario: &str, traced: &Traced) {
    crate::assert_sorted_json_snapshot!(format!("{scenario}__call"), &traced.call_frame(false));
    crate::assert_sorted_json_snapshot!(
        format!("{scenario}__call_with_log"),
        &traced.call_frame(true)
    );
    crate::assert_sorted_json_snapshot!(format!("{scenario}__prestate"), &traced.prestate(false));
    crate::assert_sorted_json_snapshot!(
        format!("{scenario}__prestate_diff"),
        &traced.prestate(true)
    );
    crate::assert_sorted_json_snapshot!(format!("{scenario}__struct_logs"), &traced.struct_logs());
    assert_eip3155_snapshot(&format!("{scenario}__eip3155"), &traced.eip3155());
}

/// The transaction is billed `expected`, ledger by ledger, under the expected floor, and its
/// receipt is their sum, at least the floor.
pub(crate) fn assert_ledgers(traced: &Traced, expected: Ledgers) {
    let gas = &traced.outcome.gas;
    assert_eq!(
        (gas.regular, gas.state, gas.history),
        (expected.regular, expected.state, expected.history),
        "the ledgers (regular, state, history)"
    );
    assert_eq!(gas.floor, expected.floor, "the EIP-7623 floor");
    assert_eq!(gas.gas_used, expected.receipt(), "the receipt's gas used");
}

/// The tracer's own figure for the transaction's frame, against the ledgers.
///
/// revm-inspectors records the regular gas the engine handed the transaction's frame as the root
/// node's `gas_limit` and, when the frame ends, what the frame spent of it as `gas_used`
/// (`Gas::total_gas_spent`: the limit less what is left, so state and history gas that spilled
/// onto regular gas is in it, and what the reservoir paid is not). What the frame did not spend
/// and the reservoir left are the gas the transaction did not use; everything else its gas limit
/// bought is its raw spend, the three ledgers' sum:
///
/// `gas_limit = (regular + state + history) + (root.gas_limit − root.gas_used) + given_back +
/// reservoir left`
///
/// The refund and the EIP-7623 floor change only how much of that the receipt bills, so neither
/// is in it.
///
/// `given_back` is the state and history gas the frame spilled onto its regular gas and got back
/// because it failed. revm gives a failed frame that gas back when the frame's result is merged
/// (`handle_reservoir_remaining_gas`, for the transaction's own frame in `last_frame_result`),
/// which is after the inspector's `call_end` read it: the tracer counts it as spent, the receipt
/// does not bill it.
///
/// The first run's inspector is read here: the node path overwrites the root's gas limit with the
/// transaction's.
pub(crate) fn assert_root_frame_settles(traced: &Traced, given_back: u64) {
    let root = &traced.inspector.traces().nodes()[0].trace;
    let unspent = root.gas_limit - root.gas_used;
    let gas = &traced.outcome.gas;
    let spent = gas.regular + gas.state + gas.history;
    assert_eq!(
        traced.gas_limit(),
        spent + unspent + given_back + gas.reservoir_remaining,
        "the raw spend ({spent}), the root frame's unspent gas ({unspent}), what its failure gave \
         back ({given_back}) and the reservoir left do not make up the gas limit"
    );
}

/// The steps the first run's inspector recorded, in execution order, each with the index of the
/// frame that ran it.
fn steps_in_order(traced: &Traced) -> Vec<(usize, &CallTraceStep)> {
    fn walk<'a>(nodes: &'a [CallTraceNode], idx: usize, out: &mut Vec<(usize, &'a CallTraceStep)>) {
        let node = &nodes[idx];
        for member in &node.ordering {
            match member {
                TraceMemberOrder::Step(step) => out.push((idx, &node.trace.steps[*step])),
                TraceMemberOrder::Call(child) => walk(nodes, node.children[*child], out),
                TraceMemberOrder::Log(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(traced.inspector.traces().nodes(), 0, &mut out);
    out
}

/// The `gasUsed` of the EIP-3155 trace's summary line.
pub(crate) fn eip3155_summary_gas_used(traced: &Traced) -> u64 {
    let eip3155 = traced.eip3155();
    let summary = eip3155.lines().last().expect("the summary line");
    let summary: serde_json::Value = serde_json::from_str(summary).expect("a JSON line");
    quantity(&summary["gasUsed"])
}

/// A `0x`-prefixed hex quantity of the EIP-3155 trace.
fn quantity(value: &serde_json::Value) -> u64 {
    let text = value.as_str().expect("a hex quantity");
    u64::from_str_radix(text.trim_start_matches("0x"), 16).expect("a hex quantity")
}

/// The EIP-3155 trace against the other tracer's steps and the receipt.
///
/// - Its op lines are the steps the first run's inspector recorded, in execution order, the
///   creation of a keyless deployment included: the same program counter, opcode, gas before the
///   step and cost.
/// - Its summary passes exactly when the transaction succeeded.
/// - Its summary's `fork` is `Osaka`: the fork's tracer prints the L1 name of the spec the
///   configuration runs, which for Satin is Karst's base, not the schedule Satin prices with.
/// - Its summary's `gasUsed` is the transaction's gas limit less the regular gas the last executed
///   step left: the fork's `GasInspector` tracks nothing else, and starts from nothing when no
///   frame ran a step. Where that step is the transaction's own frame's, the frame is a call and it
///   was not stopped by gas detention, whose crossing the tracer sees zeroed, it is the raw spend,
///   the three ledgers' sum, plus the reservoir left: the reservoir is not regular gas, so the
///   summary counts what is left of it as used, and the refund and the EIP-7623 floor, which change
///   only what the receipt bills, are not in it. A creation is charged its deposit after its last
///   step, which the creation and keyless scenarios assert by themselves. Where a child frame ran
///   the last step, as in a stop that spans frames, the figure is that child's leftover read
///   against the transaction's gas limit, and bears no relation to the receipt.
pub(crate) fn assert_eip3155_agrees(traced: &Traced) {
    let lines: Vec<serde_json::Value> = traced
        .eip3155()
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    let (summary, ops) = lines.split_last().expect("the summary line");
    assert!(summary.get("stateRoot").is_some(), "the last line is the summary: {summary}");
    assert_eq!(
        summary["fork"], "Osaka",
        "the fork's tracer prints the L1 name of the Karst base spec, not the schedule's"
    );
    let steps = steps_in_order(traced);
    assert_eq!(ops.len(), steps.len(), "the EIP-3155 op lines are the inspector's steps");
    for (line, (_, step)) in ops.iter().zip(&steps) {
        let recorded =
            (step.pc as u64, u64::from(step.op.get()), step.gas_remaining, step.gas_cost);
        let printed = (
            line["pc"].as_u64().expect("pc"),
            line["op"].as_u64().expect("op"),
            quantity(&line["gas"]),
            quantity(&line["gasCost"]),
        );
        assert_eq!(printed, recorded, "EIP-3155 step (pc, op, gas, gasCost)");
    }
    assert_eq!(summary["pass"], traced.outcome.result.is_success(), "the summary's pass");
    let left = steps.last().map_or(0, |(_, step)| step.gas_remaining - step.gas_cost);
    let gas_used = quantity(&summary["gasUsed"]);
    assert_eq!(gas_used, traced.gas_limit() - left, "the summary's gasUsed");
    let detained = matches!(
        traced.outcome.limit_exceeded,
        Some(LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, .. })
    );
    let is_call = traced.tx.0.base.kind.is_call();
    if steps.last().is_some_and(|(frame, _)| *frame == 0) && is_call && !detained {
        let gas = &traced.outcome.gas;
        assert_eq!(
            gas_used,
            gas.regular + gas.state + gas.history + gas.reservoir_remaining,
            "the summary's gasUsed is the raw spend plus the reservoir left"
        );
    }
}

/// The root frame's `gas` in both call tracer views is the transaction's gas limit, as a node
/// reports it.
///
/// What the engine hands the first frame is less: the intrinsic gas, the body's history and what
/// is charged before the first frame are taken out of it, and the reservoir above the execution
/// cap is not regular gas at all. `DebugInspector::get_result` overwrites the root's gas limit
/// with the transaction's before it builds the call frames, so neither shows here.
pub(crate) fn assert_root_gas_is_the_gas_limit(traced: &Traced) {
    for with_log in [false, true] {
        let frame = traced.call_frame(with_log);
        assert_eq!(
            frame.gas,
            U256::from(traced.gas_limit()),
            "root gas (withLog = {with_log}) is not the transaction's gas limit"
        );
    }
}

/// The prestate names every account the engine read from the database, with what it read.
///
/// The first run read the pre-state through a `RecordingDatabase`, which records every account,
/// slot and code it serves the engine. The prestate tracer instead walks the state the
/// transaction returned and reads the pre-state database itself. An account the engine read that
/// the returned state does not carry — a read outside the journal, or a frame's accounts dropped
/// on a stop — would be recorded here and missing from the prestate, as it would from a witness
/// built from the same state.
///
/// One read is outside every transaction's state by design, and is the only one excused: op-revm
/// reads the L1 block contract's account and the slots of `L1_BLOCK_INFO_SLOTS` on the database
/// itself, not through the journal, to price a transaction's L1 fee. Block execution hands
/// exactly those keys to the witness as a pre-block read (`read_l1_block_info`). The excuse is
/// those keys, read or not, and nothing else.
pub(crate) fn assert_prestate_covers_reads(traced: &Traced) {
    let PreStateFrame::Default(PreStateMode(prestate)) = traced.prestate(false) else {
        panic!("the default prestate mode");
    };
    let reads = &traced.reads;
    let traced_accounts: BTreeSet<Address> = prestate.keys().copied().collect();
    let missing: Vec<_> = reads
        .accounts
        .keys()
        .copied()
        .filter(|address| *address != L1_BLOCK_CONTRACT && !traced_accounts.contains(address))
        .collect();
    assert!(missing.is_empty(), "the prestate misses accounts the engine read: {missing:?}");
    for (address, served) in reads.accounts.iter().filter(|(a, _)| **a != L1_BLOCK_CONTRACT) {
        let served = served.clone().unwrap_or_default();
        let code = (served.code_hash != KECCAK_EMPTY).then(|| {
            reads.codes.get(&served.code_hash).expect("served code is recorded").original_bytes()
        });
        let expected = AccountState::from_account_info(served.nonce, served.balance, code);
        let traced = &prestate[address];
        assert_eq!(traced.balance, expected.balance, "prestate balance of {address}");
        assert_eq!(traced.nonce, expected.nonce, "prestate nonce of {address}");
        assert_eq!(traced.code, expected.code, "prestate code of {address}");
    }
    for ((address, slot), value) in &reads.storage {
        if *address == L1_BLOCK_CONTRACT {
            assert!(L1_BLOCK_INFO_SLOTS.contains(slot), "an L1 block slot outside the pricing's");
            continue;
        }
        let traced =
            prestate.get(address).and_then(|account| account.storage.get(&B256::from(*slot)));
        assert_eq!(traced, Some(&B256::from(*value)), "prestate slot {slot} of {address}");
    }
}

/// The kind and limit of a `MegaLimitExceeded` revert.
pub(crate) fn decode_stop(output: &[u8]) -> (LimitKind, u64) {
    let stop = MegaLimitExceeded::abi_decode(output)
        .unwrap_or_else(|_| panic!("not MegaLimitExceeded: {}", Bytes::copy_from_slice(output)));
    (LimitKind::from_u8(stop.kind).expect("a limit kind"), stop.limit)
}

/// The transaction was stopped by the transaction-level limit `kind` at `limit`, and every one of
/// its `frames` call frames — the one that crossed it and every caller, which must not resume —
/// returns that stop, as the node's call tracer shows them.
pub(crate) fn assert_every_frame_stops(
    traced: &Traced,
    kind: LimitKind,
    limit: u64,
    frames: usize,
) {
    let Some(LimitCheck::ExceedsLimit { kind: latched, limit: latched_limit, frame_local, .. }) =
        traced.outcome.limit_exceeded
    else {
        panic!("the transaction is not stopped: {:?}", traced.outcome.result);
    };
    assert_eq!((latched, latched_limit), (kind, limit), "the limit the transaction latched");
    assert!(!frame_local, "a transaction-level limit");
    let ExecutionResult::Revert { output, .. } = &traced.outcome.result else {
        panic!("a stop settles as a revert: {:?}", traced.outcome.result);
    };
    assert_eq!(decode_stop(output), (kind, limit), "the transaction's revert data");
    fn walk(frame: &CallFrame, expected: (LimitKind, u64), seen: &mut usize) {
        *seen += 1;
        let output = frame.output.as_ref().unwrap_or_else(|| panic!("no output: {frame:?}"));
        assert_eq!(
            frame.error.as_deref(),
            Some("execution reverted"),
            "frame {}",
            frame.to.unwrap_or_default()
        );
        assert_eq!(decode_stop(output), expected, "frame {}", frame.to.unwrap_or_default());
        for child in &frame.calls {
            walk(child, expected, seen);
        }
    }
    let mut seen = 0;
    walk(&traced.call_frame(true), (kind, limit), &mut seen);
    assert_eq!(seen, frames, "the frames the stop spans");
}

/// Gas detention stopped the transaction at `compute`: the compute the stop reports as used, which
/// the regular ledger bills.
pub(crate) fn assert_compute_at_stop(traced: &Traced, compute: u64) {
    let Some(LimitCheck::ExceedsLimit { kind: LimitKind::ComputeGas, used, .. }) =
        traced.outcome.limit_exceeded
    else {
        panic!("gas detention did not stop the transaction: {:?}", traced.outcome.limit_exceeded);
    };
    assert_eq!(used, compute, "the compute at the crossing");
}

/// The transaction's own frame ran no instruction after the call that returned the stop: its
/// last struct-log step at depth 1 is a call.
pub(crate) fn assert_parent_does_not_resume(traced: &Traced) {
    let logs = traced.struct_logs().struct_logs;
    let last = logs.iter().rev().find(|log| log.depth == 1).expect("the parent ran");
    assert_eq!(last.op, "CALL", "the parent's last instruction is the call that was stopped");
    assert!(logs.iter().all(|log| log.op != "SSTORE"), "the parent wrote nothing after the call");
}

/// Known shape: the last opcode step of a gas-detention stop reads as an out-of-gas that cost
/// the frame all the gas it had, though the engine never made that charge.
///
/// The crossing charge fails in the interpreter the way an out-of-gas does, which zeroes the
/// frame's gas, and both the struct-log builder and the EIP-3155 tracer take the step's cost and
/// status in `step_end`, from that state. The engine puts the frame's gas back to what it had
/// before the charge only when it settles the frame's result, after the step, so the receipt
/// bills the compute before the charge while the step shows the frame's whole gas as its cost.
///
/// If the engine starts settling the crossing before `step_end` sees it, or a tracer stops
/// reading it there, this fails: the step then has a cost no larger than the charge that
/// crossed, and the assertion should require that instead.
pub(crate) fn assert_detention_step_reads_as_out_of_gas(traced: &Traced, crossing_op: &str) {
    let logs = traced.struct_logs().struct_logs;
    let last = logs.last().expect("the frames ran");
    let shape = "the detention crossing's step reads as an out-of-gas that spent the frame's \
                 whole gas (a fix makes this fail: require the step's own cost instead)";
    assert_eq!(last.op, crossing_op, "{shape}");
    let error = last.error.as_deref();
    assert!(error.is_some_and(|error| error.contains("OutOfGas")), "{shape}: error {error:?}");
    assert_eq!(last.gas_cost, last.gas, "{shape}");
    // How revm-inspectors renders the error, apart from what it is: the `Debug` of the step's
    // `Option` status. A tracer that prints the status alone changes only this.
    assert_eq!(
        error,
        Some("Some(OutOfGas)"),
        "revm-inspectors renders a step's error as the Debug of an Option; a tracer that prints \
         the status alone makes this fail: update the expected string and review the detention \
         snapshots with `cargo insta review`"
    );
    assert!(last.gas_cost > traced.outcome.gas.gas_used, "{shape}: the receipt bills none of it");
    let eip3155 = traced.eip3155();
    let last_step = eip3155.lines().rfind(|line| line.contains("\"opName\"")).expect("a step");
    let step: serde_json::Value = serde_json::from_str(last_step).expect("a JSON line");
    assert_eq!(step["opName"], crossing_op, "{shape} (EIP-3155)");
    assert_eq!(step["error"], "OutOfGas", "{shape} (EIP-3155)");
    assert_eq!(step["gasCost"], step["gas"], "{shape} (EIP-3155)");
}

/// The keyless call frame recorded no opcode steps; the creation frame recorded some.
///
/// This reads the inspector's own record of the frames. What a node's opcode tracer makes of it
/// is [`assert_keyless_struct_logs_miss_the_creation`].
pub(crate) fn assert_keyless_steps(traced: &Traced, expect_create: bool) {
    let nodes = traced.inspector.traces().nodes();
    assert!(!nodes.is_empty(), "the tracer recorded a call frame");
    let call_steps = nodes[0].trace.steps.len();
    assert_eq!(
        call_steps, 0,
        "the keyless call frame now runs {call_steps} step(s): this lifts the known limitation \
         `assert_keyless_struct_logs_miss_the_creation` names; update both together"
    );
    if expect_create {
        assert!(nodes.len() >= 2, "expected a creation child, got {} frames", nodes.len());
        assert!(
            !nodes[1].trace.steps.is_empty(),
            "the keyless creation frame must have struct-log steps"
        );
    } else {
        assert_eq!(nodes.len(), 1, "a refused keyless call starts no creation");
    }
}

/// Known limitation: the opcode tracer a node runs shows no step of a keyless deployment, though
/// the creation ran its init code.
///
/// The engine runs no instruction in the `keylessDeploy` call's frame, so the frame has no steps,
/// and the creation is its child one level below. revm-inspectors' struct-log builder starts
/// from the root frame's steps and enters a child frame only through a call-opcode step of its
/// parent (`push_steps_on_stack`), so it never reaches the creation's steps, which the inspector
/// did record and the EIP-3155 trace prints.
///
/// A change on either side — the engine giving the call frame a step that leads to its child, or
/// the builder walking a frame's children without one — makes this fail; that is the moment to
/// require the creation's steps in the struct logs instead and review the keyless snapshots.
pub(crate) fn assert_keyless_struct_logs_miss_the_creation(traced: &Traced) {
    let nodes = traced.inspector.traces().nodes();
    let creation_steps = nodes[1].trace.steps.len();
    let call_steps = nodes[0].trace.steps.len();
    assert_eq!(
        call_steps, 0,
        "known limitation lifted on the engine's side: the keyless call frame now runs \
         {call_steps} step(s), which may lead the struct-log builder to its creation; require the \
         creation's steps in the struct logs here instead of their absence, update \
         `assert_keyless_steps`, and review the keyless snapshots with `cargo insta review`"
    );
    assert!(creation_steps > 0, "the inspector recorded the creation's steps");
    let eip3155_steps = traced.eip3155().lines().filter(|line| line.contains("\"opName\"")).count();
    assert_eq!(eip3155_steps, creation_steps, "the EIP-3155 trace prints the creation's steps");
    let struct_logs = traced.struct_logs().struct_logs;
    assert!(
        struct_logs.is_empty(),
        "known limitation lifted: the struct logs of a keyless deployment now show {} step(s) of \
         its creation (the inspector recorded {creation_steps}); require them in \
         `assert_keyless_struct_logs_miss_the_creation` instead of their absence, and review the \
         keyless snapshots with `cargo insta review`",
        struct_logs.len()
    );
}
