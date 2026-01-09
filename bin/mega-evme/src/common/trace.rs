//! Trace configuration for mega-evme

use std::path::PathBuf;

use alloy_primitives::{Address, Bytes, U256};
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
        inspector::JournalExt,
        interpreter::{
            interpreter::EthInterpreter,
            interpreter_types::{Jumps, StackTr},
            CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter, InterpreterTypes,
        },
        state::EvmState,
        ExecuteEvm, InspectEvm, Inspector,
    },
    MegaContext, MegaEvm, MegaHaltReason, MegaTransaction, TestExternalEnvs,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use tracing::{debug, info, trace};

use super::{EvmeError, EvmeState};

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

    /// Execute transaction with optional tracing
    pub fn execute_transaction<N, P>(
        &self,
        evm_context: MegaContext<&mut EvmeState<N, P>, TestExternalEnvs>,
        tx: MegaTransaction,
    ) -> Result<(ExecutionResult<MegaHaltReason>, EvmState, Option<String>), EvmeError>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        if self.is_tracing_enabled() {
            info!(tracer = ?self.tracer, "Evm executing with tracing");
            // Execute with tracing inspector
            let mut inspector = self.create_inspector();
            let mut evm = MegaEvm::new(evm_context).with_inspector(&mut inspector);

            let result_and_state = evm
                .inspect_tx(tx)
                .map_err(|e| EvmeError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;
            trace!(result_and_state = ?result_and_state, "Evm execution result and state");

            // Generate trace string based on tracer type
            let trace_str = self.generate_trace(evm.inspector, &result_and_state, evm.db_ref());
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

/// Custom inspector that overrides SLOAD values.
///
/// When SLOAD returns 0x647fb72723817:
/// - First occurrence in a transaction: pass through unchanged
/// - Subsequent occurrences: override to 0x647fb7270fdc2
#[derive(Debug, Default)]
pub struct CustomInspector {
    /// Tracks if we've seen the magic SLOAD value in this transaction
    seen_magic_value: bool,
}

impl CustomInspector {
    /// Reset the seen_magic_value flag.
    /// Called before each transaction via CombinedInspector::fuse().
    pub fn reset(&mut self) {
        self.seen_magic_value = false;
    }
}

impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for CustomInspector
where
    CTX: ContextTr,
{
    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        // SLOAD opcode = 0x54
        if interp.bytecode.opcode() == 0x54 {
            // Check the value that was just pushed to stack
            if let Some(top_value) = interp.stack.top() {
                let magic_value = U256::from(0x647fb72723817u64);
                if *top_value == magic_value {
                    if self.seen_magic_value {
                        // Subsequent read: override the value in place
                        *top_value = U256::from(0x647fb7270fdc2u64);
                        println!("Overriding SLOAD value to 0x647fb7270fdc2");
                    } else {
                        // First read: mark as seen, don't modify
                        self.seen_magic_value = true;
                    }
                }
            }
        }
    }
}

/// Combined inspector that wraps a TracingInspector and a CustomInspector.
///
/// Delegates all inspector methods to both inner inspectors.
#[derive(Debug)]
pub struct CombinedInspector {
    /// The tracing inspector for recording execution traces.
    pub tracing: TracingInspector,
    /// The custom inspector (currently noop).
    pub custom: CustomInspector,
}

impl CombinedInspector {
    /// Create a new combined inspector from the given tracing inspector.
    pub fn new(tracing: TracingInspector) -> Self {
        Self { tracing, custom: CustomInspector::default() }
    }

    /// Get a reference to the tracing inspector.
    pub fn tracing_inspector(&self) -> &TracingInspector {
        &self.tracing
    }

    /// Get a mutable reference to the tracing inspector.
    pub fn tracing_inspector_mut(&mut self) -> &mut TracingInspector {
        &mut self.tracing
    }

    /// Fuse the tracing inspector and reset the custom inspector.
    /// Called before each target transaction.
    pub fn fuse(&mut self) {
        self.tracing.fuse();
        self.custom.reset();
    }
}

impl<CTX> Inspector<CTX> for CombinedInspector
where
    CTX: ContextTr,
    CTX::Journal: JournalExt,
{
    fn initialize_interp(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut CTX) {
        self.tracing.initialize_interp(interp, context);
        self.custom.initialize_interp(interp, context);
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut CTX) {
        self.tracing.step(interp, context);
        self.custom.step(interp, context);
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, context: &mut CTX) {
        self.tracing.step_end(interp, context);
        self.custom.step_end(interp, context);
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let outcome = self.tracing.call(context, inputs);
        if outcome.is_some() {
            return outcome;
        }
        <CustomInspector as Inspector<CTX, EthInterpreter>>::call(&mut self.custom, context, inputs)
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.tracing.call_end(context, inputs, outcome);
        <CustomInspector as Inspector<CTX, EthInterpreter>>::call_end(
            &mut self.custom,
            context,
            inputs,
            outcome,
        );
    }

    fn create(&mut self, context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let outcome = self.tracing.create(context, inputs);
        if outcome.is_some() {
            return outcome;
        }
        <CustomInspector as Inspector<CTX, EthInterpreter>>::create(
            &mut self.custom,
            context,
            inputs,
        )
    }

    fn create_end(
        &mut self,
        context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.tracing.create_end(context, inputs, outcome);
        <CustomInspector as Inspector<CTX, EthInterpreter>>::create_end(
            &mut self.custom,
            context,
            inputs,
            outcome,
        );
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        <TracingInspector as Inspector<CTX>>::selfdestruct(
            &mut self.tracing,
            contract,
            target,
            value,
        );
        <CustomInspector as Inspector<CTX, EthInterpreter>>::selfdestruct(
            &mut self.custom,
            contract,
            target,
            value,
        );
    }
}
