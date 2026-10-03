//! Run `MegaEvm` under `revm-inspectors` tracers and compare JSON against pinned goldens.
//!
//! Set `UPDATE_GOLDENS=1` to rewrite the files under `tests/tracers/goldens/`. The default
//! path only compares, and never writes.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use alloy_primitives::{Address, Bytes};
use alloy_rpc_types_trace::geth::{
    CallConfig, CallFrame, DefaultFrame, GethDefaultTracingOptions, PreStateConfig, PreStateFrame,
};
use alloy_sol_types::SolError;
use mega_evm::{
    test_utils::{op_transaction, zero_fee_l1_block_info, MemoryDatabase},
    EvmTxRuntimeLimits, MegaContext, MegaEvm, MegaLimitExceeded, MegaSpecId, MegaTransaction,
    MegaTransactionOutcome,
};
use revm::{
    context::{BlockEnv, TxEnv},
    primitives::U256,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
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

/// One Satin-engine execution under a tracer that records steps, logs and state diffs.
#[derive(Debug)]
pub(crate) struct Traced {
    /// What the engine reported, including the three gas ledgers.
    pub outcome: MegaTransactionOutcome,
    /// The inspector after the transaction finished.
    pub inspector: TracingInspector,
    /// The pre-state database the prestate tracer reads.
    pub pre_db: MemoryDatabase,
}

impl Traced {
    /// Runs `tx` on a fresh Satin EVM over `db`.
    pub(crate) fn run(db: MemoryDatabase, tx: MegaTransaction, limits: EvmTxRuntimeLimits) -> Self {
        let pre_db = db.clone();
        let inspector = TracingInspector::new(TracingInspectorConfig::all());
        let mut evm =
            MegaEvm::new(context(db).with_tx_runtime_limits(limits)).with_inspector(inspector);
        let outcome = evm.execute_transaction(tx).expect("the transaction is valid");
        Self { outcome, inspector: evm.inspector().clone(), pre_db }
    }

    /// Receipt gas: the three ledgers after the refund, at least the EIP-7623 floor.
    ///
    /// `callTracer`'s top-level `gasUsed` is filled from this figure (the same number
    /// `ExecutionResult::gas().tx_gas_used()` reports). It is **not** the regular / execution
    /// ledger alone, and it is not state or history gas on their own.
    pub(crate) fn receipt_gas(&self) -> u64 {
        self.outcome.gas.gas_used
    }

    /// Geth call tracer output.
    pub(crate) fn call_frame(&self, with_log: bool) -> CallFrame {
        self.inspector.geth_builder().geth_call_traces(
            CallConfig { only_top_call: Some(false), with_log: Some(with_log) },
            self.receipt_gas(),
        )
    }

    /// Geth opcode / struct-log tracer. Memory and storage are omitted; the stack is kept.
    pub(crate) fn struct_logs(&self) -> DefaultFrame {
        let output = match &self.outcome.result {
            revm::context::result::ExecutionResult::Success { output, .. } => output.data().clone(),
            revm::context::result::ExecutionResult::Revert { output, .. } => output.clone(),
            revm::context::result::ExecutionResult::Halt { .. } => Bytes::new(),
        };
        self.inspector.geth_builder().geth_traces(
            self.receipt_gas(),
            output,
            GethDefaultTracingOptions {
                disable_memory: Some(true),
                disable_stack: Some(false),
                disable_storage: Some(true),
                enable_return_data: Some(false),
                ..Default::default()
            },
        )
    }

    /// Geth prestate tracer.
    pub(crate) fn prestate(&self, diff_mode: bool) -> PreStateFrame {
        self.inspector
            .geth_builder()
            .geth_prestate_traces(
                &self.outcome.result_and_state,
                &PreStateConfig {
                    diff_mode: Some(diff_mode),
                    disable_code: Some(false),
                    disable_storage: Some(false),
                },
                &*self.pre_db,
            )
            .expect("prestate tracer")
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
    let path = golden_path(name);
    if update_goldens() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create goldens dir");
        }
        fs::write(&path, &json).unwrap_or_else(|err| panic!("write {}: {err}", path.display()));
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("missing golden {} ({err}); rerun with UPDATE_GOLDENS=1 to pin it", path.display())
    });
    assert_eq!(expected, json, "golden mismatch for {name}");
}

/// Pins every tracer view of `traced` under `scenario/`.
pub(crate) fn pin_tracer_views(scenario: &str, traced: &Traced) {
    assert_golden(&format!("{scenario}/call.json"), &traced.call_frame(false));
    assert_golden(&format!("{scenario}/call_with_log.json"), &traced.call_frame(true));
    assert_golden(&format!("{scenario}/prestate.json"), &traced.prestate(false));
    assert_golden(&format!("{scenario}/prestate_diff.json"), &traced.prestate(true));
    assert_golden(&format!("{scenario}/struct_logs.json"), &traced.struct_logs());
}

/// callTracer top-level `gasUsed` versus the receipt gas ledger. See [`Traced::receipt_gas`].
pub(crate) fn assert_call_gas_matches_receipt(traced: &Traced) {
    let frame = traced.call_frame(false);
    let tracer = u64::try_from(frame.gas_used).expect("gasUsed fits u64");
    let receipt = traced.receipt_gas();
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
