//! Common argument groups and functions shared between run and tx commands

use alloy_primitives::Bytes;
use alloy_rpc_types_trace::geth::GethDefaultTracingOptions;
use mega_evm::{
    revm::{context::result::ExecutionResult, state::EvmState, ExecuteEvm, InspectEvm},
    MegaContext, MegaEvm, MegaTransaction, TestExternalEnvs,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use super::{EvmeState, Result, RunError};

// Re-export from common module
pub use crate::common::{
    parse_bucket_capacity, EnvArgs, PreStateArgs, StateDumpArgs, TraceArgs, TracerType,
};

/// Execute transaction with optional tracing
pub fn execute_transaction<N, P>(
    evm_context: MegaContext<&mut EvmeState<N, P>, TestExternalEnvs>,
    tx: MegaTransaction,
    trace_args: &TraceArgs,
) -> Result<(ExecutionResult<mega_evm::MegaHaltReason>, EvmState, Option<String>)>
where
    N: alloy_network::Network,
    P: alloy_provider::Provider<N> + std::fmt::Debug,
{
    if matches!(trace_args.tracer, Some(TracerType::Trace)) {
        // Execute with tracing inspector
        let config = TracingInspectorConfig::all();
        let mut inspector = TracingInspector::new(config);
        let mut evm = MegaEvm::new(evm_context).with_inspector(&mut inspector);

        let result_and_state =
            if trace_args.tracer.is_some() { evm.inspect_tx(tx) } else { evm.transact(tx) }
                .map_err(|e| RunError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;

        // Generate GethTrace using GethTraceBuilder
        let geth_builder = inspector.geth_builder();

        // Create GethDefaultTracingOptions based on CLI arguments
        let opts = GethDefaultTracingOptions {
            disable_storage: Some(trace_args.trace_disable_storage),
            disable_memory: Some(trace_args.trace_disable_memory),
            disable_stack: Some(trace_args.trace_disable_stack),
            enable_return_data: Some(trace_args.trace_enable_return_data),
            ..Default::default()
        };

        // Get output for trace generation
        let output = match &result_and_state.result {
            ExecutionResult::Success { output, .. } => output.data().to_vec(),
            ExecutionResult::Revert { output, .. } => output.to_vec(),
            _ => Vec::new(),
        };

        // Generate the geth trace
        let geth_trace =
            geth_builder.geth_traces(result_and_state.result.gas_used(), Bytes::from(output), opts);

        // Format as JSON
        let trace_str = serde_json::to_string_pretty(&geth_trace)
            .unwrap_or_else(|e| format!("Error serializing trace: {}", e));

        Ok((result_and_state.result, result_and_state.state, Some(trace_str)))
    } else {
        // Execute without tracing
        let mut evm = MegaEvm::new(evm_context);
        let result_and_state = evm
            .transact(tx)
            .map_err(|e| RunError::ExecutionError(format!("EVM execution failed: {:?}", e)))?;

        Ok((result_and_state.result, result_and_state.state, None))
    }
}
