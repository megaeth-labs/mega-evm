//! Trace configuration for mega-evme

use std::{
    cell::{Ref, RefCell},
    path::PathBuf,
    rc::Rc,
};

use alloy_primitives::{Address, Bytes, Log, U256};
use alloy_rpc_types_trace::geth::{
    CallConfig, CallFrame, GethDefaultTracingOptions, PreStateConfig,
};
use clap::{Parser, ValueEnum};
use mega_evm::{
    revm::{
        bytecode::opcode::{self, OpCode},
        context::{
            result::{ExecutionResult, ResultAndState},
            ContextTr,
        },
        database::DatabaseRef,
        inspector::JournalExt,
        interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
        state::EvmState,
        ExecuteEvm, InspectEvm, Inspector,
    },
    sandbox::InspectorSandboxObserver,
    MegaContext, MegaEvm, MegaHaltReason, MegaTransaction, KEYLESS_DEPLOY_ADDRESS,
};
use revm_inspectors::tracing::{
    types::{CallTraceStep, TraceMemberOrder},
    TracingInspector, TracingInspectorConfig,
};
use tracing::{debug, info, trace};

use super::{EvmeError, EvmeExternalEnvs, EvmeState};

/// [`Inspector`] adapter over a [`TracingInspector`] shared via [`Rc`]/[`RefCell`].
///
/// Nested keyless-deploy sandbox execution is a separate EVM, so a parent inspector
/// never sees those frames. This wrapper is the outer inspector and, cloned, the
/// inner inspector of [`InspectorSandboxObserver`].
///
/// Outer and sandbox use *two* of these handles. The sandbox journal reports depth 0
/// for its top-level CREATE; recording that into the outer inspector would overwrite
/// the CALL root (`CallTraceArena` treats depth 0 as the entry). After the transaction
/// returns, [`splice_sandbox_traces`] grafts the sandbox arena under the `KeylessDeploy`
/// CALL.
#[derive(Clone, Debug)]
pub(crate) struct RcTracingInspector(Rc<RefCell<TracingInspector>>);

impl RcTracingInspector {
    /// Wraps an existing [`TracingInspector`].
    pub(crate) fn new(inspector: TracingInspector) -> Self {
        Self(Rc::new(RefCell::new(inspector)))
    }

    /// Resets recorded traces so a subsequent transaction starts from a clean arena.
    pub(crate) fn fuse(&self) {
        self.0.borrow_mut().fuse();
    }

    /// Borrows the inner inspector.
    pub(crate) fn borrow(&self) -> Ref<'_, TracingInspector> {
        self.0.borrow()
    }

    /// Observer handle for [`MegaEvm::set_keyless_sandbox_observer`].
    pub(crate) fn as_sandbox_observer(&self) -> Rc<RefCell<InspectorSandboxObserver<Self>>> {
        Rc::new(RefCell::new(InspectorSandboxObserver(self.clone())))
    }
}

impl<CTX> Inspector<CTX> for RcTracingInspector
where
    CTX: ContextTr<Journal: JournalExt>,
{
    fn step(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.0.borrow_mut().step(interp, context);
    }

    fn step_end(&mut self, interp: &mut Interpreter, context: &mut CTX) {
        self.0.borrow_mut().step_end(interp, context);
    }

    fn log(&mut self, interp: &mut Interpreter, context: &mut CTX, log: Log) {
        self.0.borrow_mut().log(interp, context, log);
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.0.borrow_mut().call(context, inputs)
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.0.borrow_mut().call_end(context, inputs, outcome);
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.0.borrow_mut().create(context, inputs)
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.0.borrow_mut().create_end(context, inputs, outcome);
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        <TracingInspector as Inspector<CTX>>::selfdestruct(
            &mut self.0.borrow_mut(),
            contract,
            target,
            value,
        );
    }
}

/// Returns true when `sandbox` recorded at least one completed frame.
fn sandbox_has_frames(sandbox: &TracingInspector) -> bool {
    sandbox.traces().nodes().iter().any(|node| {
        node.trace.status.is_some() || !node.trace.steps.is_empty() || !node.children.is_empty()
    })
}

/// Attaches `sandbox` traces as children of the outer [`KEYLESS_DEPLOY_ADDRESS`] CALL.
///
/// Sandbox execution uses a fresh journal whose top-level CREATE is depth 0. Recording
/// that into the outer inspector would replace the CALL root, so the sandbox is traced
/// on a sibling inspector and grafted here. Depths and arena indices are rewritten so
/// the Geth call tree hangs the CREATE under the CALL.
///
/// `geth_traces` only descends into a child when the parent frame has a matching
/// call-like opcode in `steps`. Intercepted `KeylessDeploy` records no bytecode, so
/// a CREATE step is grafted onto the parent; without it, sandbox constructor
/// steps are omitted from struct-log output rather than nested in execution order.
fn splice_sandbox_traces(outer: &RcTracingInspector, sandbox: &RcTracingInspector) {
    let sandbox_inspector = sandbox.borrow();
    if !sandbox_has_frames(&sandbox_inspector) {
        return;
    }

    let mut outer_inspector = outer.0.borrow_mut();
    let parent_idx = outer_inspector
        .traces()
        .nodes()
        .iter()
        .rposition(|node| {
            node.trace.address == KEYLESS_DEPLOY_ADDRESS && !node.trace.kind.is_any_create()
        })
        .unwrap_or(0);
    let depth_offset = outer_inspector.traces().nodes()[parent_idx].trace.depth + 1;
    let base = outer_inspector.traces().nodes().len();

    let remapped: Vec<_> = sandbox_inspector
        .traces()
        .nodes()
        .iter()
        .cloned()
        .enumerate()
        .map(|(old_idx, mut node)| {
            node.idx = base + old_idx;
            node.trace.depth += depth_offset;
            for step in &mut node.trace.steps {
                step.depth += depth_offset as u64;
            }
            node.parent = Some(match node.parent {
                None => parent_idx,
                Some(parent) => base + parent,
            });
            for child in &mut node.children {
                *child += base;
            }
            node
        })
        .collect();
    drop(sandbox_inspector);

    outer_inspector.traces_mut().nodes_mut().extend(remapped);
    let parent = &mut outer_inspector.traces_mut().nodes_mut()[parent_idx];
    let child_slot = parent.children.len();
    parent.children.push(base);

    // geth_traces pairs call-like parent steps with `children` in order. A
    // missing CREATE step here would leave the grafted child unreachable.
    let calllike = parent.trace.steps.iter().filter(|s| is_calllike_opcode(s.op.get())).count();
    if calllike < parent.children.len() {
        let step_idx = parent.trace.steps.len();
        let create_depth = parent.trace.depth as u64 + 1;
        let contract = parent.trace.address;
        let gas_remaining = parent.trace.gas_limit;
        parent.trace.steps.push(intercepted_create_step(create_depth, contract, gas_remaining));
        parent.ordering.push(TraceMemberOrder::Step(step_idx));
    }
    parent.ordering.push(TraceMemberOrder::Call(child_slot));
}

/// CREATE/CALL-family opcodes that `geth_traces` uses to walk into children.
fn is_calllike_opcode(op: u8) -> bool {
    matches!(
        op,
        opcode::CALL |
            opcode::DELEGATECALL |
            opcode::STATICCALL |
            opcode::CREATE |
            opcode::CALLCODE |
            opcode::CREATE2
    )
}

/// Placeholder CREATE recorded on an intercepted `KeylessDeploy` CALL so struct-log
/// output can descend into the spliced sandbox frame.
fn intercepted_create_step(depth: u64, contract: Address, gas_remaining: u64) -> CallTraceStep {
    CallTraceStep {
        depth,
        pc: 0,
        op: OpCode::new(opcode::CREATE).expect("CREATE is a defined opcode"),
        contract,
        stack: None,
        push_stack: None,
        memory: None,
        returndata: Bytes::new(),
        gas_remaining,
        gas_refund_counter: 0,
        gas_used: 0,
        gas_cost: 0,
        storage_change: None,
        status: None,
        immediate_bytes: None,
        decoded: None,
    }
}

/// Tracer type for execution analysis
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
#[non_exhaustive]
pub enum TracerType {
    /// Enable execution tracing (opcode-level trace in Geth format)
    #[default]
    Opcode,
    /// Enable call tracing (tracks call frames in nested tree structure)
    Call,
    /// Enable pre-state tracing (retrieves account state before execution)
    #[value(alias = "prestate")]
    PreState,
}

/// Trace configuration arguments
#[derive(Parser, Debug, Clone)]
#[command(next_help_heading = "Trace Options")]
pub struct TraceArgs {
    /// Enable tracing
    #[arg(long = "trace")]
    pub trace: bool,

    /// Output file for trace data (if not specified, prints to console)
    #[arg(long = "trace.output")]
    pub trace_output_file: Option<PathBuf>,

    /// Tracer type to use (defaults to struct logger if not specified)
    #[arg(long = "tracer", value_enum, default_value_t = TracerType::Opcode)]
    pub tracer: TracerType,

    /// Disable memory capture in traces (opcode tracer only)
    #[arg(long = "trace.opcode.disable-memory")]
    pub trace_opcode_disable_memory: bool,

    /// Disable stack capture in traces (opcode tracer only)
    #[arg(long = "trace.opcode.disable-stack")]
    pub trace_opcode_disable_stack: bool,

    /// Disable storage capture in traces (opcode tracer only)
    #[arg(long = "trace.opcode.disable-storage")]
    pub trace_opcode_disable_storage: bool,

    /// Enable return data capture in traces (opcode tracer only)
    #[arg(long = "trace.opcode.enable-return-data")]
    pub trace_opcode_enable_return_data: bool,

    /// Only trace top-level call (call tracer only)
    #[arg(long = "trace.call.only-top-call")]
    pub trace_call_only_top_call: bool,

    /// Include logs in call trace (call tracer only)
    #[arg(long = "trace.call.with-log")]
    pub trace_call_with_log: bool,

    /// Show state diff instead of prestate (pre-state tracer only)
    #[arg(long = "trace.prestate.diff-mode", visible_aliases = ["trace.pre-state.diff-mode"])]
    pub trace_prestate_diff_mode: bool,

    /// Disable code in prestate output (pre-state tracer only)
    #[arg(long = "trace.prestate.disable-code", visible_aliases = ["trace.pre-state.disable-code"])]
    pub trace_prestate_disable_code: bool,

    /// Disable storage in prestate output (pre-state tracer only)
    #[arg(long = "trace.prestate.disable-storage", visible_aliases = ["trace.pre-state.disable-storage"])]
    pub trace_prestate_disable_storage: bool,
}

impl TraceArgs {
    /// Returns true if tracing is enabled
    pub fn is_tracing_enabled(&self) -> bool {
        self.trace
    }

    /// Creates a [`TracingInspector`] configured for full tracing
    pub fn create_inspector(&self) -> TracingInspector {
        let config = TracingInspectorConfig::all();
        TracingInspector::new(config)
    }

    /// Creates [`GethDefaultTracingOptions`] from CLI arguments
    pub fn create_geth_options(&self) -> GethDefaultTracingOptions {
        GethDefaultTracingOptions {
            disable_storage: Some(self.trace_opcode_disable_storage),
            disable_memory: Some(self.trace_opcode_disable_memory),
            disable_stack: Some(self.trace_opcode_disable_stack),
            enable_return_data: Some(self.trace_opcode_enable_return_data),
            ..Default::default()
        }
    }

    /// Creates [`CallConfig`] from CLI arguments
    pub fn create_call_config(&self) -> CallConfig {
        CallConfig {
            only_top_call: Some(self.trace_call_only_top_call),
            with_log: Some(self.trace_call_with_log),
        }
    }

    /// Creates [`PreStateConfig`] from CLI arguments
    pub fn create_prestate_config(&self) -> PreStateConfig {
        PreStateConfig {
            diff_mode: Some(self.trace_prestate_diff_mode),
            disable_code: Some(self.trace_prestate_disable_code),
            disable_storage: Some(self.trace_prestate_disable_storage),
        }
    }

    /// Generates a JSON trace string for the default tracer
    fn generate_default_trace<HaltReason>(
        &self,
        inspector: &TracingInspector,
        exec_result: &ExecutionResult<HaltReason>,
    ) -> String {
        let geth_builder = inspector.geth_builder();
        let opts = self.create_geth_options();
        debug!(opts = ?opts, "Generating default opcode trace");

        // Get output for trace generation
        let output = match exec_result {
            ExecutionResult::Success { output, .. } => output.data().to_vec(),
            ExecutionResult::Revert { output, .. } => output.to_vec(),
            _ => Vec::new(),
        };

        // Generate the geth trace
        let geth_trace =
            geth_builder.geth_traces(exec_result.gas_used(), Bytes::from(output), opts);

        // Format as JSON
        serde_json::to_string_pretty(&geth_trace)
            .unwrap_or_else(|e| format!("Error serializing trace: {}", e))
    }

    /// Generates a JSON trace string for the call tracer
    fn generate_call_trace<HaltReason>(
        &self,
        inspector: &TracingInspector,
        exec_result: &ExecutionResult<HaltReason>,
    ) -> String {
        let geth_builder = inspector.geth_builder();
        let config = self.create_call_config();
        debug!(config = ?config, "Generating call trace");

        // Generate the call trace
        let call_frame: CallFrame = geth_builder.geth_call_traces(config, exec_result.gas_used());

        // Format as JSON
        serde_json::to_string_pretty(&call_frame)
            .unwrap_or_else(|e| format!("Error serializing call trace: {}", e))
    }

    /// Generates a JSON trace string for the prestate tracer.
    fn generate_prestate_trace(
        &self,
        inspector: &TracingInspector,
        result_and_state: &ResultAndState<MegaHaltReason>,
        prestate: impl DatabaseRef,
    ) -> String {
        let geth_builder = inspector.geth_builder();
        let config = self.create_prestate_config();
        debug!(config = ?config, "Generating prestate trace");

        // Generate the prestate trace using the database
        match geth_builder.geth_prestate_traces(result_and_state, &config, prestate) {
            Ok(prestate_frame) => serde_json::to_string_pretty(&prestate_frame)
                .unwrap_or_else(|e| format!("Error serializing prestate trace: {}", e)),
            Err(e) => format!("Error generating prestate trace: {:?}", e),
        }
    }

    /// Generates a JSON trace string from inspector and execution result based on tracer type.
    /// Note: For `PreState` tracer, use `generate_prestate_trace` directly with database access.
    pub fn generate_trace(
        &self,
        inspector: &TracingInspector,
        result_and_state: &ResultAndState<MegaHaltReason>,
        prestate: impl DatabaseRef,
    ) -> String {
        info!(tracer = ?self.tracer, "Generating trace");
        match self.tracer {
            TracerType::Opcode => self.generate_default_trace(inspector, &result_and_state.result),
            TracerType::Call => self.generate_call_trace(inspector, &result_and_state.result),
            TracerType::PreState => {
                self.generate_prestate_trace(inspector, result_and_state, prestate)
            }
        }
    }

    /// Splices sandbox frames under the outer `KeylessDeploy` CALL and renders the trace.
    pub(crate) fn generate_trace_with_sandbox(
        &self,
        outer: &RcTracingInspector,
        sandbox: &RcTracingInspector,
        result_and_state: &ResultAndState<MegaHaltReason>,
        prestate: impl DatabaseRef,
    ) -> String {
        splice_sandbox_traces(outer, sandbox);
        self.generate_trace(&outer.borrow(), result_and_state, prestate)
    }

    /// Execute transaction with optional tracing
    pub fn execute_transaction<N, P>(
        &self,
        evm_context: MegaContext<&mut EvmeState<N, P>, EvmeExternalEnvs>,
        tx: MegaTransaction,
    ) -> Result<(ExecutionResult<MegaHaltReason>, EvmState, Option<String>), EvmeError>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        if self.is_tracing_enabled() {
            info!(tracer = ?self.tracer, "Evm executing with tracing");
            let outer = RcTracingInspector::new(self.create_inspector());
            let sandbox = RcTracingInspector::new(self.create_inspector());
            let mut evm = MegaEvm::new(evm_context).with_inspector(outer.clone());
            evm.set_keyless_sandbox_observer(Some(sandbox.as_sandbox_observer()));

            let result_and_state = evm
                .inspect_tx(tx)
                .map_err(|e| EvmeError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;
            trace!(result_and_state = ?result_and_state, "Evm execution result and state");

            let trace_str =
                self.generate_trace_with_sandbox(&outer, &sandbox, &result_and_state, evm.db_ref());
            trace!(trace_str = ?trace_str, "Generated trace");

            Ok((result_and_state.result, result_and_state.state, Some(trace_str)))
        } else {
            info!("Evm executing without tracing");
            // Execute without tracing
            let mut evm = MegaEvm::new(evm_context);
            let result_and_state = evm
                .transact(tx)
                .map_err(|e| EvmeError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;
            trace!(result_and_state = ?result_and_state, "Evm execution result and state");

            Ok((result_and_state.result, result_and_state.state, None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{transaction::Recovered, Signed, TxLegacy};
    use alloy_primitives::{address, hex, Signature, TxKind, B256};
    use alloy_sol_types::SolCall;
    use mega_evm::{
        alloy_evm::EvmEnv,
        alloy_op_evm::block::OpAlloyReceiptBuilder,
        revm::{
            bytecode::opcode::{CODECOPY, RETURN, SSTORE},
            context::{BlockEnv, CfgEnv, TxEnv},
            database::StateBuilder,
        },
        test_utils::{BytecodeBuilder, MemoryDatabase},
        BlockLimits, EmptyExternalEnv, IKeylessDeploy, MegaBlockExecutionCtx,
        MegaBlockExecutorFactory, MegaEvmFactory, MegaHardforkConfig, MegaSpecId, MegaTxEnvelope,
    };

    const TEST_CALLER: Address = address!("0000000000000000000000000000000000100000");
    const LARGE_GAS_LIMIT_OVERRIDE: u64 = 10_000_000_000;
    const LARGE_SIGNER_BALANCE: u128 = 1_000_000_000_000_000_000_000;
    const SIGNED_TX_GAS_LIMIT: u64 = 1_000_000;
    const OUTER_GAS_LIMIT: u64 = 1_000_000_000_000;

    fn success_constructor() -> Bytes {
        BytecodeBuilder::default()
            .sstore(U256::from(0), U256::from(1))
            .push_number(1_u8)
            .push_number(0_u8)
            .push_number(0_u8)
            .append(CODECOPY)
            .push_number(1_u8)
            .push_number(0_u8)
            .append(RETURN)
            .build()
    }

    fn create_pre_eip155_deploy_tx(init_code: Bytes) -> (Bytes, Address) {
        let tx = TxLegacy {
            nonce: 0,
            gas_price: 100_000_000_000,
            gas_limit: SIGNED_TX_GAS_LIMIT,
            to: TxKind::Create,
            value: U256::ZERO,
            input: init_code,
            chain_id: None,
        };
        let r = U256::from_be_bytes(hex!(
            "2222222222222222222222222222222222222222222222222222222222222222"
        ));
        let s = U256::from_be_bytes(hex!(
            "2222222222222222222222222222222222222222222222222222222222222222"
        ));
        let sig = Signature::new(r, s, false);
        let signed = Signed::new_unchecked(tx, sig, B256::ZERO);
        let mut buf = Vec::new();
        signed.rlp_encode(&mut buf);
        let tx_bytes = Bytes::from(buf);
        let signer = signed.recover_signer().expect("should recover signer");
        (tx_bytes, signer)
    }

    fn keyless_deploy_tx(keyless_deployment_tx: Bytes) -> MegaTransaction {
        let call_data = IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: keyless_deployment_tx,
            gasLimitOverride: U256::from(LARGE_GAS_LIMIT_OVERRIDE),
        }
        .abi_encode();
        let tx = TxEnv {
            caller: TEST_CALLER,
            kind: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
            data: call_data.into(),
            value: U256::ZERO,
            gas_limit: OUTER_GAS_LIMIT,
            gas_price: 0,
            ..Default::default()
        };
        let mut tx = MegaTransaction::new(tx);
        tx.enveloped_tx = Some(Bytes::new());
        tx
    }

    fn funded_db(signer: Address) -> MemoryDatabase {
        let mut db = MemoryDatabase::default();
        db.set_account_balance(signer, U256::from(LARGE_SIGNER_BALANCE));
        db.set_account_balance(TEST_CALLER, U256::from(LARGE_SIGNER_BALANCE));
        db
    }

    fn keyless_context(
        db: &mut MemoryDatabase,
    ) -> MegaContext<&mut MemoryDatabase, EmptyExternalEnv> {
        let mut context = MegaContext::new(db, MegaSpecId::REX5);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        context
    }

    fn run_traced_keyless(
        outer: RcTracingInspector,
        sandbox: RcTracingInspector,
    ) -> (ResultAndState<MegaHaltReason>, MemoryDatabase, Address) {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db = funded_db(signer);
        let result = {
            let context = keyless_context(&mut db);
            let mut evm = MegaEvm::new(context).with_inspector(outer);
            evm.set_keyless_sandbox_observer(Some(sandbox.as_sandbox_observer()));
            let result = evm.inspect_tx(keyless_deploy_tx(tx_bytes)).expect("keyless deploy");
            assert!(result.result.is_success(), "deploy should succeed: {:?}", result.result);
            result
        };
        (result, db, signer)
    }

    #[test]
    fn test_keyless_call_trace_nests_sandbox_create() {
        let outer = RcTracingInspector::new(TracingInspector::new(TracingInspectorConfig::all()));
        let sandbox = RcTracingInspector::new(TracingInspector::new(TracingInspectorConfig::all()));
        let (result_and_state, _db, _signer) = run_traced_keyless(outer.clone(), sandbox.clone());
        splice_sandbox_traces(&outer, &sandbox);

        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_eq!(nodes[0].trace.address, KEYLESS_DEPLOY_ADDRESS, "root is KeylessDeploy CALL");
        assert!(!nodes[0].trace.kind.is_any_create(), "root stays a CALL, not overwritten CREATE");
        assert!(!nodes[0].children.is_empty(), "sandbox CREATE is a child of the CALL");

        let create = &nodes[nodes[0].children[0]];
        assert!(create.trace.kind.is_any_create(), "child is the sandbox CREATE");
        assert_eq!(create.trace.depth, nodes[0].trace.depth + 1);
        assert!(
            create.trace.steps.iter().any(|s| s.op.get() == SSTORE),
            "sandbox constructor steps are on the CREATE node"
        );

        let call_frame: CallFrame = outer_ref.geth_builder().geth_call_traces(
            CallConfig { only_top_call: Some(false), with_log: Some(false) },
            result_and_state.result.gas_used(),
        );
        assert_eq!(call_frame.typ, "CALL");
        assert_eq!(call_frame.to, Some(KEYLESS_DEPLOY_ADDRESS));
        assert_eq!(call_frame.calls.len(), 1, "CREATE nested under KeylessDeploy CALL");
        assert_eq!(call_frame.calls[0].typ, "CREATE");
    }

    #[test]
    fn test_keyless_opcode_trace_includes_sandbox_steps_in_execution_order() {
        let outer = RcTracingInspector::new(TracingInspector::new(TracingInspectorConfig::all()));
        let sandbox = RcTracingInspector::new(TracingInspector::new(TracingInspectorConfig::all()));
        let (result_and_state, db, _signer) = run_traced_keyless(outer.clone(), sandbox.clone());
        splice_sandbox_traces(&outer, &sandbox);

        let outer_ref = outer.borrow();
        let geth_trace = outer_ref.geth_builder().geth_traces(
            result_and_state.result.gas_used(),
            Bytes::new(),
            GethDefaultTracingOptions::default(),
        );
        let logs = &geth_trace.struct_logs;
        let ops: Vec<_> = logs.iter().map(|s| format!("d{}:{}", s.depth, s.op)).collect();

        let create = logs
            .iter()
            .position(|s| s.op == "CREATE")
            .unwrap_or_else(|| panic!("CREATE missing from structLogs: {ops:?}"));
        let sstore = logs.iter().position(|s| s.op == "SSTORE").unwrap_or_else(|| {
            panic!("sandbox constructor SSTORE missing from structLogs: {ops:?}")
        });
        let first_outer_after_create = logs
            .iter()
            .skip(create + 1)
            .position(|s| s.depth <= logs[create].depth)
            .map(|rel| create + 1 + rel)
            .unwrap_or(logs.len());

        assert!(create < sstore, "sandbox SSTORE should follow the outer CREATE: {ops:?}");
        assert_eq!(
            sstore,
            create + 3,
            "SSTORE should be the constructor's third opcode after CREATE, not delayed: {ops:?}"
        );
        assert!(
            logs[sstore].depth > logs[create].depth,
            "sandbox SSTORE should nest deeper than CREATE: {ops:?}"
        );
        assert!(
            sstore < first_outer_after_create,
            "sandbox SSTORE should sit inside the CREATE nest, not after later parent steps: {ops:?}"
        );

        outer_ref
            .geth_builder()
            .geth_prestate_traces(&result_and_state, &PreStateConfig::default(), &*db)
            .expect("prestate tracer after sandbox splice");
    }

    #[test]
    fn test_replay_executor_nests_sandbox_create() {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        let mut db = funded_db(signer);
        let mut state = StateBuilder::new().with_database(&mut db).with_bundle_update().build();

        let evm_factory = MegaEvmFactory::new().with_external_env_factory(EvmeExternalEnvs::new());
        let hardforks = MegaHardforkConfig::default().with_all_activated();
        let factory = MegaBlockExecutorFactory::new(
            &hardforks,
            evm_factory,
            OpAlloyReceiptBuilder::default(),
        );

        let mut cfg_env = CfgEnv::default();
        cfg_env.spec = MegaSpecId::REX5;
        let evm_env =
            EvmEnv::new(cfg_env, BlockEnv { gas_limit: OUTER_GAS_LIMIT, ..Default::default() });
        let block_ctx = MegaBlockExecutionCtx::new(
            B256::ZERO,
            Some(B256::ZERO),
            Bytes::new(),
            BlockLimits::no_limits(),
        );

        let outer = RcTracingInspector::new(TracingInspector::new(TracingInspectorConfig::all()));
        let sandbox = RcTracingInspector::new(TracingInspector::new(TracingInspectorConfig::all()));
        let mut executor =
            factory.create_executor_with_inspector(&mut state, block_ctx, evm_env, outer.clone());
        // Skip `apply_pre_execution_changes`: it needs a configured SequencerRegistry on
        // REX5+, and KeylessDeploy interception does not require the predeploy bytecode.
        executor.inspector_mut().fuse();
        executor.evm.ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        executor.set_keyless_sandbox_observer(Some(sandbox.as_sandbox_observer()));

        let call_data = IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: tx_bytes,
            gasLimitOverride: U256::from(LARGE_GAS_LIMIT_OVERRIDE),
        }
        .abi_encode();
        let tx_legacy = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price: 0,
            gas_limit: OUTER_GAS_LIMIT,
            to: TxKind::Call(KEYLESS_DEPLOY_ADDRESS),
            value: U256::ZERO,
            input: call_data.into(),
        };
        let signed = Signed::new_unchecked(tx_legacy, Signature::test_signature(), B256::ZERO);
        let recovered = Recovered::new_unchecked(MegaTxEnvelope::Legacy(signed), TEST_CALLER);

        let outcome = executor.run_transaction(&recovered).expect("run keyless deploy");
        assert!(outcome.inner.result.is_success(), "{:?}", outcome.inner.result);

        splice_sandbox_traces(&outer, &sandbox);
        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_eq!(nodes[0].trace.address, KEYLESS_DEPLOY_ADDRESS);
        assert!(!nodes[0].children.is_empty());
        assert!(nodes[nodes[0].children[0]].trace.kind.is_any_create());
    }
}
