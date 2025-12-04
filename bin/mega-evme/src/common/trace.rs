//! Trace configuration for mega-evme

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

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
