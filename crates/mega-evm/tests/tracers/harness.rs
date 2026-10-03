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

use alloy_primitives::{Address, Bytes};
use alloy_rpc_types_trace::geth::{
    CallConfig, CallFrame, DefaultFrame, GethDebugTracingOptions, GethDefaultTracingOptions,
    GethTrace, PreStateConfig, PreStateFrame,
};
use alloy_sol_types::SolError;
use mega_evm::{
    alloy_evm::Evm as _,
    test_utils::{op_transaction, zero_fee_l1_block_info, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaLimitExceeded, MegaSpecId, MegaTransaction,
    MegaTransactionOutcome,
};
use revm::{
    context::{BlockEnv, TxEnv},
    inspector::inspectors::TracerEip3155,
    primitives::U256,
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
pub(crate) fn context(db: MemoryDatabase) -> MegaContext<MemoryDatabase> {
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
    /// The transaction, kept to run it again under another tracer.
    tx: MegaTransaction,
    /// The runtime limits it ran under.
    limits: EvmTxRuntimeLimits,
}

impl Traced {
    /// Runs `tx` on a fresh Satin EVM over `db`.
    pub(crate) fn run(db: MemoryDatabase, tx: MegaTransaction, limits: EvmTxRuntimeLimits) -> Self {
        let pre_db = db.clone();
        let inspector = TracingInspector::new(TracingInspectorConfig::all());
        let mut evm =
            MegaEvm::new(context(db).with_tx_runtime_limits(limits)).with_inspector(inspector);
        let outcome = evm.execute_transaction(tx.clone()).expect("the transaction is valid");
        Self { outcome, inspector: evm.inspector().clone(), pre_db, tx, limits }
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

/// Every account the transaction's post-state names appears in the prestate tracer.
pub(crate) fn assert_prestate_covers_touched(traced: &Traced) {
    let pre = traced.prestate(false);
    let traced_accounts: BTreeSet<Address> = pre.pre_state().keys().copied().collect();
    let missing: Vec<_> = traced
        .outcome
        .state
        .keys()
        .copied()
        .filter(|address| !traced_accounts.contains(address))
        .collect();
    assert!(
        missing.is_empty(),
        "prestateTracer missed accounts the transaction touched: {missing:?}; traced {traced_accounts:?}"
    );
}

/// Every call frame's output is the ABI encoding of `MegaLimitExceeded`.
pub(crate) fn assert_limit_stop_outputs(traced: &Traced) {
    let expected = traced
        .outcome
        .limit_exceeded
        .as_ref()
        .expect("the transaction is a limit stop")
        .revert_data();
    let decoded = MegaLimitExceeded::abi_decode(&expected).expect("stop data is MegaLimitExceeded");
    fn walk(frame: &CallFrame, decoded: &MegaLimitExceeded) {
        let output = frame.output.as_ref().expect("limit-stop frame has output");
        let got = MegaLimitExceeded::abi_decode(output)
            .unwrap_or_else(|_| panic!("frame output is not MegaLimitExceeded: {output}"));
        assert_eq!(&got, decoded, "frame {} output", frame.typ);
        for child in &frame.calls {
            walk(child, decoded);
        }
    }
    walk(&traced.call_frame(true), &decoded);
}

/// The keyless call frame recorded no opcode steps; the creation frame recorded some.
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
