//! Run `MegaEvm` under `revm-inspectors` tracers and compare JSON against pinned goldens.
//!
//! Set `UPDATE_GOLDENS=1` to rewrite the files under `tests/tracers/goldens/`. The default
//! path only compares, and never writes.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    rc::Rc,
};

use alloy_primitives::{Address, Bytes, B256};
use alloy_rpc_types_trace::geth::{
    AccountState, CallConfig, CallFrame, DefaultFrame, GethDebugTracingOptions,
    GethDefaultTracingOptions, GethTrace, PreStateConfig, PreStateFrame, PreStateMode,
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
use revm_inspectors::tracing::{DebugInspector, TracingInspector, TracingInspectorConfig};
use serde::Serialize;

/// Directory that holds the pinned JSON files, relative to this crate's manifest.
const GOLDENS_DIR: &str = "tests/tracers/goldens";

/// Environment variable that rewrites goldens instead of comparing them.
const UPDATE_GOLDENS: &str = "UPDATE_GOLDENS";

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

    /// Geth opcode / struct-log tracer. Memory and storage are omitted; the stack is kept.
    pub(crate) fn struct_logs(&self) -> DefaultFrame {
        let config = GethDefaultTracingOptions {
            disable_memory: Some(true),
            disable_stack: Some(false),
            disable_storage: Some(true),
            enable_return_data: Some(false),
            ..Default::default()
        };
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

/// Whether this process should rewrite goldens.
pub(crate) fn update_goldens() -> bool {
    matches!(std::env::var(UPDATE_GOLDENS), Ok(value) if value == "1")
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDENS_DIR).join(name)
}

/// Serializes `value` as pretty JSON with a trailing newline and compares it byte-for-byte
/// with the pinned file `name`. Rewrites the file when `UPDATE_GOLDENS=1`.
pub(crate) fn assert_golden(name: &str, value: &impl Serialize) {
    let json = format!("{}\n", serde_json::to_string_pretty(value).expect("json"));
    assert_golden_text(name, &json);
}

/// Compares `text` byte-for-byte with the pinned file `name`. Rewrites the file when
/// `UPDATE_GOLDENS=1`.
pub(crate) fn assert_golden_text(name: &str, text: &str) {
    let path = golden_path(name);
    if update_goldens() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create goldens dir");
        }
        fs::write(&path, text).unwrap_or_else(|err| panic!("write {}: {err}", path.display()));
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("missing golden {} ({err}); rerun with UPDATE_GOLDENS=1 to pin it", path.display())
    });
    assert_eq!(expected, text, "golden mismatch for {name}");
}

/// Pins every tracer view of `traced` under `scenario/`.
pub(crate) fn pin_tracer_views(scenario: &str, traced: &Traced) {
    assert_golden(&format!("{scenario}/call.json"), &traced.call_frame(false));
    assert_golden(&format!("{scenario}/call_with_log.json"), &traced.call_frame(true));
    assert_golden(&format!("{scenario}/prestate.json"), &traced.prestate(false));
    assert_golden(&format!("{scenario}/prestate_diff.json"), &traced.prestate(true));
    assert_golden(&format!("{scenario}/struct_logs.json"), &traced.struct_logs());
    assert_golden_text(&format!("{scenario}/eip3155.jsonl"), &traced.eip3155());
}

/// callTracer top-level `gasUsed` versus the receipt gas ledger.
pub(crate) fn assert_call_gas_matches_receipt(traced: &Traced) {
    let frame = traced.call_frame(false);
    let tracer = u64::try_from(frame.gas_used).expect("gasUsed fits u64");
    let receipt = traced.outcome.gas.gas_used;
    let result_gas = traced.outcome.result.gas().tx_gas_used();
    assert_eq!(receipt, result_gas, "receipt gas and ExecutionResult gas must agree");
    if tracer != receipt {
        // Keep the golden as-is. A mismatch is a tracer-shape issue, not an engine bug to fix.
        eprintln!(
            "suspected issue: callTracer gasUsed={tracer} receipt_gas={receipt} regular={} state={} history={}",
            traced.outcome.gas.regular,
            traced.outcome.gas.state,
            traced.outcome.gas.history
        );
    }
    assert_eq!(
        tracer, receipt,
        "callTracer gasUsed should be the receipt gas (three ledgers after refund, at least the floor), not the regular ledger alone"
    );
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
    assert_eq!(last.error.as_deref(), Some("Some(OutOfGas)"), "{shape}");
    assert_eq!(last.gas_cost, last.gas, "{shape}");
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
    assert!(
        nodes[0].trace.steps.is_empty(),
        "the keyless call frame must have no struct-log steps, got {}",
        nodes[0].trace.steps.len()
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

/// Spec byte prices are in effect. Measurement builds skip the exact JSON comparison.
pub(crate) fn at_spec_prices() -> bool {
    if mega_evm::active_satin_prices().is_constants() {
        return true;
    }
    mega_evm::test_utils::note_price_guard("tracer goldens are pinned at the spec's byte prices");
    false
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
/// require the creation's steps in the struct logs instead and regenerate the keyless goldens.
pub(crate) fn assert_keyless_struct_logs_miss_the_creation(traced: &Traced) {
    let nodes = traced.inspector.traces().nodes();
    let creation_steps = nodes[1].trace.steps.len();
    assert!(nodes[0].trace.steps.is_empty(), "the keyless call frame ran no instruction");
    assert!(creation_steps > 0, "the inspector recorded the creation's steps");
    let eip3155_steps = traced.eip3155().lines().filter(|line| line.contains("\"opName\"")).count();
    assert_eq!(eip3155_steps, creation_steps, "the EIP-3155 trace prints the creation's steps");
    let struct_logs = traced.struct_logs().struct_logs;
    assert!(
        struct_logs.is_empty(),
        "known limitation lifted: the struct logs of a keyless deployment now show {} step(s) of \
         its creation (the inspector recorded {creation_steps}); require them in \
         `assert_keyless_struct_logs_miss_the_creation` instead of their absence, and regenerate \
         the keyless goldens with UPDATE_GOLDENS=1",
        struct_logs.len()
    );
}
