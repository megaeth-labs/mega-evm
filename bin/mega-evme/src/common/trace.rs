//! Trace configuration for mega-evme

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use alloy_primitives::Bytes;
use alloy_rpc_types_trace::geth::{
    CallConfig, CallFrame, GethDefaultTracingOptions, PreStateConfig,
};
use clap::{Parser, ValueEnum};
use mega_evm::{
    revm::{
        context::{
            result::{ExecutionResult, ResultAndState},
            ContextTr,
        },
        database::DatabaseRef,
        state::EvmState,
        ExecuteEvm, InspectEvm,
    },
    sandbox::trace::{paired, splice_sandbox_traces, SandboxTracer, SharedTracingInspector},
    MegaContext, MegaEvm, MegaHaltReason, MegaTransaction,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use tracing::{debug, info, trace};

use super::{EvmeError, EvmeExternalEnvs, EvmeState};

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

    /// The [`TracingInspectorConfig`] every mega-evme tracer records with: everything on.
    pub fn inspector_config(&self) -> TracingInspectorConfig {
        TracingInspectorConfig::all()
    }

    /// Creates a [`TracingInspector`] configured for full tracing
    pub fn create_inspector(&self) -> TracingInspector {
        TracingInspector::new(self.inspector_config())
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
        outer: &SharedTracingInspector,
        sandbox: &Rc<RefCell<SandboxTracer>>,
        result_and_state: &ResultAndState<MegaHaltReason>,
        prestate: impl DatabaseRef,
    ) -> String {
        splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
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
            let (outer, sandbox) = paired(self.inspector_config());
            let mut evm = MegaEvm::new(evm_context).with_inspector(outer.clone());
            evm.set_keyless_sandbox_observer(Rc::clone(&sandbox));

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
    use alloy_primitives::{address, hex, Address, Signature, TxKind, B256, U256};
    use alloy_rpc_types_eth::TransactionInfo;
    use alloy_rpc_types_trace::parity::{Action, TraceOutput};
    use alloy_sol_types::SolCall;
    use mega_evm::{
        alloy_evm::EvmEnv,
        alloy_op_evm::block::OpAlloyReceiptBuilder,
        revm::{
            bytecode::opcode::{CODECOPY, RETURN, SSTORE},
            context::{BlockEnv, CfgEnv, TxEnv},
            database::StateBuilder,
        },
        test_utils::{
            deep_mixed_init, nested_keyless_call_init, revert_after_create_init, BytecodeBuilder,
            MemoryDatabase, DEEP_MIXED_LOG_TOPIC, REVERTING_RUNTIME,
        },
        BlockLimits, EmptyExternalEnv, IKeylessDeploy, MegaBlockExecutionCtx,
        MegaBlockExecutorFactory, MegaEvmFactory, MegaHardforkConfig, MegaSpecId, MegaTxEnvelope,
        KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE,
    };

    const REVERTER: Address = address!("0000000000000000000000000000000000aaaaaa");

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
        create_pre_eip155_deploy_tx_with_gas_limit(init_code, SIGNED_TX_GAS_LIMIT)
    }

    fn create_pre_eip155_deploy_tx_with_gas_limit(
        init_code: Bytes,
        gas_limit: u64,
    ) -> (Bytes, Address) {
        let tx = TxLegacy {
            nonce: 0,
            gas_price: 100_000_000_000,
            gas_limit,
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

    fn keyless_deploy_tx_with_override(
        keyless_deployment_tx: Bytes,
        gas_limit_override: u64,
    ) -> MegaTransaction {
        let call_data = IKeylessDeploy::keylessDeployCall {
            keylessDeploymentTransaction: keyless_deployment_tx,
            gasLimitOverride: U256::from(gas_limit_override),
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
        outer: SharedTracingInspector,
        sandbox: Rc<RefCell<SandboxTracer>>,
    ) -> (ResultAndState<MegaHaltReason>, MemoryDatabase, Address) {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(success_constructor());
        run_traced_keyless_bytes(outer, sandbox, tx_bytes, signer, LARGE_GAS_LIMIT_OVERRIDE, |_| {})
    }

    /// Runs one traced keyless deploy of an already-signed `tx_bytes`, with `setup` applied
    /// to the funded database first (extra contracts the constructor calls into).
    fn run_traced_keyless_bytes(
        outer: SharedTracingInspector,
        sandbox: Rc<RefCell<SandboxTracer>>,
        tx_bytes: Bytes,
        signer: Address,
        gas_limit_override: u64,
        setup: impl Fn(&mut MemoryDatabase),
    ) -> (ResultAndState<MegaHaltReason>, MemoryDatabase, Address) {
        let mut db = funded_db(signer);
        setup(&mut db);
        let result = {
            let context = keyless_context(&mut db);
            let mut evm = MegaEvm::new(context).with_inspector(outer);
            evm.set_keyless_sandbox_observer(sandbox);
            let result = evm
                .inspect_tx(keyless_deploy_tx_with_override(tx_bytes, gas_limit_override))
                .expect("keyless deploy");
            assert!(result.result.is_success(), "deploy should succeed: {:?}", result.result);
            result
        };
        (result, db, signer)
    }

    /// Traces one keyless deploy and renders the Geth call tree with logs.
    fn traced_call_frame(
        tx_bytes: Bytes,
        signer: Address,
        gas_limit_override: u64,
        setup: impl Fn(&mut MemoryDatabase),
    ) -> (CallFrame, SharedTracingInspector, ResultAndState<MegaHaltReason>, MemoryDatabase) {
        let (outer, sandbox) = paired(TracingInspectorConfig::all());
        let (result_and_state, db, _signer) = run_traced_keyless_bytes(
            outer.clone(),
            sandbox.clone(),
            tx_bytes,
            signer,
            gas_limit_override,
            setup,
        );
        splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
        let frame = outer.borrow().geth_builder().geth_call_traces(
            CallConfig { only_top_call: Some(false), with_log: Some(true) },
            result_and_state.result.gas_used(),
        );
        (frame, outer, result_and_state, db)
    }

    #[test]
    fn test_keyless_call_trace_nests_sandbox_create() {
        let (outer, sandbox) = paired(TracingInspectorConfig::all());
        let (result_and_state, _db, _signer) = run_traced_keyless(outer.clone(), sandbox.clone());
        splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());

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
        let (outer, sandbox) = paired(TracingInspectorConfig::all());
        let (result_and_state, db, _signer) = run_traced_keyless(outer.clone(), sandbox.clone());
        splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());

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

        let (outer, sandbox) = paired(TracingInspectorConfig::all());
        let mut executor =
            factory.create_executor_with_inspector(&mut state, block_ctx, evm_env, outer.clone());
        // Skip `apply_pre_execution_changes`: it needs a configured SequencerRegistry on
        // REX5+, and KeylessDeploy interception does not require the predeploy bytecode.
        executor.inspector_mut().fuse();
        executor.evm.ctx.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        executor.set_keyless_sandbox_observer(Rc::clone(&sandbox));

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

        splice_sandbox_traces(&mut outer.borrow_mut(), &mut sandbox.borrow_mut());
        let outer_ref = outer.borrow();
        let nodes = outer_ref.traces().nodes();
        assert_eq!(nodes[0].trace.address, KEYLESS_DEPLOY_ADDRESS);
        assert!(!nodes[0].children.is_empty());
        assert!(nodes[nodes[0].children[0]].trace.kind.is_any_create());
    }

    /// A constructor that CREATEs a child and then reverts: the sandbox CREATE frame carries
    /// the revert, the child CREATE stays visible beneath it (events stream, they are not
    /// retroactively truncated), and the prestate diff shows neither contract landing.
    #[test]
    fn test_keyless_trace_revert_after_create_keeps_child_frame() {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(revert_after_create_init());
        let deploy_address = signer.create(0);
        let child = deploy_address.create(1);
        let (frame, outer, result_and_state, db) =
            traced_call_frame(tx_bytes, signer, LARGE_GAS_LIMIT_OVERRIDE, |_| {});

        assert_eq!(frame.typ, "CALL");
        assert!(frame.error.is_none(), "outer call succeeds with errorData");
        let create = &frame.calls[0];
        assert_eq!(create.typ, "CREATE");
        assert!(create.error.is_some(), "sandbox CREATE reverted: {:?}", create.error);
        assert_eq!(create.calls.len(), 1, "the child CREATE is still nested under it");
        assert_eq!(create.calls[0].typ, "CREATE");
        assert_eq!(create.calls[0].to, Some(child));
        assert!(create.calls[0].error.is_none(), "the child itself succeeded");

        let prestate = outer
            .borrow()
            .geth_builder()
            .geth_prestate_traces(
                &result_and_state,
                &PreStateConfig { diff_mode: Some(true), ..Default::default() },
                &*db,
            )
            .expect("prestate diff");
        let post = prestate.as_diff().expect("diff mode").post.clone();
        assert!(!post.contains_key(&deploy_address), "parent rolled back");
        assert!(!post.contains_key(&child), "child rolled back");
    }

    /// Three CREATE levels, two reverting CALLs, and a log inside one sandbox: the call tree
    /// nests the whole shape under the `KeylessDeploy` CALL and the struct logs descend one
    /// depth level per frame.
    #[test]
    fn test_keyless_trace_deep_mixed_nests_three_levels() {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(deep_mixed_init(REVERTER));
        let parent = signer.create(0);
        let child = parent.create(1);
        let grandchild = child.create(1);
        let (frame, outer, result_and_state, _db) =
            traced_call_frame(tx_bytes, signer, LARGE_GAS_LIMIT_OVERRIDE, |db| {
                db.set_account_code(REVERTER, Bytes::from_static(&REVERTING_RUNTIME));
            });

        let sandbox = &frame.calls[0];
        assert_eq!((sandbox.typ.as_str(), sandbox.to), ("CREATE", Some(parent)));
        assert!(sandbox.error.is_none());
        let kinds: Vec<(String, Option<Address>, bool)> =
            sandbox.calls.iter().map(|c| (c.typ.clone(), c.to, c.error.is_some())).collect();
        assert_eq!(
            kinds,
            vec![
                ("CREATE".to_owned(), Some(child), false),
                ("CALL".to_owned(), Some(REVERTER), true)
            ],
            "parent frame: child CREATE then reverting CALL"
        );
        let child_frame = &sandbox.calls[0];
        let child_kinds: Vec<(String, Option<Address>, bool)> =
            child_frame.calls.iter().map(|c| (c.typ.clone(), c.to, c.error.is_some())).collect();
        assert_eq!(
            child_kinds,
            vec![
                ("CREATE".to_owned(), Some(grandchild), false),
                ("CALL".to_owned(), Some(REVERTER), true)
            ],
            "child frame: grandchild CREATE then reverting CALL"
        );
        assert_eq!(sandbox.logs.len(), 1, "the parent constructor's LOG1");
        assert_eq!(
            sandbox.logs[0].topics.as_deref().map(|t| t[0]),
            Some(B256::from(DEEP_MIXED_LOG_TOPIC))
        );

        let geth_trace = outer.borrow().geth_builder().geth_traces(
            result_and_state.result.gas_used(),
            Bytes::new(),
            GethDefaultTracingOptions::default(),
        );
        let max_depth = geth_trace.struct_logs.iter().map(|s| s.depth).max().unwrap_or(0);
        assert_eq!(max_depth, 4, "synthetic CREATE at 1, parent 2, child 3, grandchild 4");
    }

    /// An inner gas budget above the intrinsic cost but below the constructor's needs: the
    /// sandbox CREATE frame reports out of gas while the outer call still succeeds.
    #[test]
    fn test_keyless_trace_sandbox_out_of_gas_marks_create_frame() {
        const TIGHT_GAS: u64 = 110_000;
        let (tx_bytes, signer) =
            create_pre_eip155_deploy_tx_with_gas_limit(success_constructor(), TIGHT_GAS);
        let (frame, _outer, _result, _db) = traced_call_frame(tx_bytes, signer, TIGHT_GAS, |_| {});

        assert!(frame.error.is_none(), "outer call succeeds with errorData");
        let create = &frame.calls[0];
        assert_eq!(create.typ, "CREATE");
        let error = create.error.as_deref().unwrap_or_default().to_ascii_lowercase();
        assert!(error.contains("out of gas"), "sandbox CREATE halted out of gas: {error:?}");
    }

    /// A constructor that CALLs `KeylessDeploy` from inside the sandbox: interception is
    /// depth-0 only, so the trace shows an ordinary reverted CALL to the system contract
    /// nested under the sandbox CREATE.
    #[test]
    fn test_keyless_trace_nested_keyless_call_is_a_reverted_frame() {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(nested_keyless_call_init());
        let (frame, outer, result_and_state, _db) =
            traced_call_frame(tx_bytes, signer, LARGE_GAS_LIMIT_OVERRIDE, |db| {
                db.set_account_code(KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE);
            });

        let sandbox = &frame.calls[0];
        assert_eq!(sandbox.typ, "CREATE");
        assert!(sandbox.error.is_none(), "the constructor ignores the failed call");
        assert_eq!(sandbox.calls.len(), 1);
        let nested = &sandbox.calls[0];
        assert_eq!((nested.typ.as_str(), nested.to), ("CALL", Some(KEYLESS_DEPLOY_ADDRESS)));
        assert!(nested.error.is_some(), "not intercepted at depth > 0, so it reverted");

        // The contract's dispatcher runs at depth 3 before it reverts, so the struct logs
        // reach one level below the sandbox constructor.
        let geth_trace = outer.borrow().geth_builder().geth_traces(
            result_and_state.result.gas_used(),
            Bytes::new(),
            GethDefaultTracingOptions::default(),
        );
        let max_depth = geth_trace.struct_logs.iter().map(|s| s.depth).max().unwrap_or(0);
        assert_eq!(
            max_depth, 3,
            "synthetic CREATE at 1, constructor at 2, KeylessDeploy code at 3"
        );
    }

    /// Splicing a recorded sandbox into an outer inspector that does not hold its
    /// `KeylessDeploy` CALL drops the sandbox: the arena's default root is not that frame, and
    /// hanging the sandbox under it would invent a call tree the outer EVM never executed.
    #[test]
    fn test_splice_into_empty_outer_drops_the_sandbox() {
        let (outer, sandbox) = paired(TracingInspectorConfig::all());
        run_traced_keyless(outer, sandbox.clone());
        assert_eq!(sandbox.borrow().pending(), 1, "the sandbox recorded the CREATE");

        let mut empty = TracingInspector::new(TracingInspectorConfig::all());
        splice_sandbox_traces(&mut empty, &mut sandbox.borrow_mut());
        assert_eq!(sandbox.borrow().pending(), 0, "the sandbox is consumed either way");
        assert_eq!(empty.traces().nodes().len(), 1, "the arena keeps only its default root");
        assert!(empty.traces().nodes()[0].children.is_empty(), "nothing was grafted");
    }

    /// The parity rendering (`trace_*` and `debug_traceTransaction` with `flatCallTracer`) of
    /// the spliced trace: the sandbox CREATE is trace address `[0]` under the `KeylessDeploy`
    /// CALL, every nested frame extends that prefix, and `subtraces` counts match the tree.
    #[test]
    fn test_keyless_flat_trace_addresses_nest_under_sandbox_create() {
        let (tx_bytes, signer) = create_pre_eip155_deploy_tx(deep_mixed_init(REVERTER));
        let parent = signer.create(0);
        let child = parent.create(1);
        let grandchild = child.create(1);
        let (_frame, outer, _result, _db) =
            traced_call_frame(tx_bytes, signer, LARGE_GAS_LIMIT_OVERRIDE, |db| {
                db.set_account_code(REVERTER, Bytes::from_static(&REVERTING_RUNTIME));
            });

        let traces = outer
            .borrow()
            .clone()
            .into_parity_builder()
            .into_localized_transaction_traces(TransactionInfo::default());

        /// `(trace_address, action kind, target, subtraces, errored)` of one flat entry.
        type FlatEntry = (Vec<usize>, &'static str, Option<Address>, usize, bool);
        let shape: Vec<FlatEntry> = traces
            .iter()
            .map(|t| {
                let (kind, to) = match &t.trace.action {
                    Action::Call(call) => ("call", Some(call.to)),
                    Action::Create(_) => (
                        "create",
                        match &t.trace.result {
                            Some(TraceOutput::Create(out)) => Some(out.address),
                            _ => None,
                        },
                    ),
                    other => panic!("unexpected action {other:?}"),
                };
                (
                    t.trace.trace_address.clone(),
                    kind,
                    to,
                    t.trace.subtraces,
                    t.trace.error.is_some(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (vec![], "call", Some(KEYLESS_DEPLOY_ADDRESS), 1, false),
                (vec![0], "create", Some(parent), 2, false),
                (vec![0, 0], "create", Some(child), 2, false),
                (vec![0, 0, 0], "create", Some(grandchild), 0, false),
                (vec![0, 0, 1], "call", Some(REVERTER), 0, true),
                (vec![0, 1], "call", Some(REVERTER), 0, true),
            ],
            "flat traces: {traces:#?}"
        );
    }
}
