//! Trace configuration for mega-evme

use std::path::PathBuf;

use alloy_primitives::Bytes;
use alloy_rpc_types_trace::geth::GethDefaultTracingOptions;
use clap::{Parser, ValueEnum};
use mega_evm::{
    revm::{context::result::ExecutionResult, state::EvmState, ExecuteEvm, InspectEvm},
    MegaContext, MegaEvm, MegaTransaction, TestExternalEnvs,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use super::{EvmeError, EvmeState};

/// Tracer type for execution analysis
#[derive(Debug, Clone, Copy, ValueEnum)]
#[non_exhaustive]
pub enum TracerType {
    /// Enable execution tracing (opcode-level trace in Geth format)
    Trace,
}

/// Trace configuration arguments
#[derive(Parser, Debug, Clone)]
pub struct TraceArgs {
    /// Tracer to enable during execution
    #[arg(long = "tracer", value_enum)]
    pub tracer: Option<TracerType>,

    /// Disable memory capture in traces
    #[arg(long = "trace.disable-memory")]
    pub trace_disable_memory: bool,

    /// Disable stack capture in traces
    #[arg(long = "trace.disable-stack")]
    pub trace_disable_stack: bool,

    /// Disable storage capture in traces
    #[arg(long = "trace.disable-storage")]
    pub trace_disable_storage: bool,

    /// Enable return data capture in traces
    #[arg(long = "trace.enable-return-data")]
    pub trace_enable_return_data: bool,

    /// Output file for trace data (if not specified, prints to console)
    #[arg(long = "trace.output")]
    pub trace_output_file: Option<PathBuf>,
}

impl TraceArgs {
    /// Returns true if tracing is enabled
    pub fn is_tracing_enabled(&self) -> bool {
        self.tracer.is_some()
    }

    /// Creates a [`TracingInspector`] configured for full tracing
    pub fn create_inspector(&self) -> TracingInspector {
        let config = TracingInspectorConfig::all();
        TracingInspector::new(config)
    }

    /// Creates [`GethDefaultTracingOptions`] from CLI arguments
    pub fn create_geth_options(&self) -> GethDefaultTracingOptions {
        GethDefaultTracingOptions {
            disable_storage: Some(self.trace_disable_storage),
            disable_memory: Some(self.trace_disable_memory),
            disable_stack: Some(self.trace_disable_stack),
            enable_return_data: Some(self.trace_enable_return_data),
            ..Default::default()
        }
    }

    /// Generates a JSON trace string from inspector and execution result
    pub fn generate_trace<HaltReason>(
        &self,
        inspector: &TracingInspector,
        exec_result: &ExecutionResult<HaltReason>,
    ) -> String {
        let geth_builder = inspector.geth_builder();
        let opts = self.create_geth_options();

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

    /// Execute transaction with optional tracing
    pub fn execute_transaction<N, P>(
        &self,
        evm_context: MegaContext<&mut EvmeState<N, P>, TestExternalEnvs>,
        tx: MegaTransaction,
    ) -> Result<(ExecutionResult<mega_evm::MegaHaltReason>, EvmState, Option<String>), EvmeError>
    where
        N: alloy_network::Network,
        P: alloy_provider::Provider<N> + std::fmt::Debug,
    {
        if self.is_tracing_enabled() {
            // Execute with tracing inspector
            let mut inspector = self.create_inspector();
            let mut evm = MegaEvm::new(evm_context).with_inspector(&mut inspector);

            let result_and_state = evm
                .inspect_tx(tx)
                .map_err(|e| EvmeError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;

            // Generate trace string
            let trace_str = self.generate_trace(&inspector, &result_and_state.result);

            Ok((result_and_state.result, result_and_state.state, Some(trace_str)))
        } else {
            // Execute without tracing
            let mut evm = MegaEvm::new(evm_context);
            let result_and_state = evm
                .transact(tx)
                .map_err(|e| EvmeError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;

            Ok((result_and_state.result, result_and_state.state, None))
        }
    }
}
