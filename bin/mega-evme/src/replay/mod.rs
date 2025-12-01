//! Replay module for fetching and executing transactions from RPC
//!
//! This module provides functionality to replay historical transactions
//! by fetching them from an RPC endpoint and re-executing them.

mod cmd;

use alloy_primitives::BlockNumber;
use alloy_provider::transport::TransportError;
pub use cmd::*;
use mega_evm::alloy_evm::block::BlockExecutionError;

/// Error types for the replay command
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// RPC error
    #[error("RPC error: {0}")]
    RpcError(String),

    /// RPC transport error
    #[error("RPC transport error: {0}")]
    RpcTransportError(TransportError),

    /// Transaction not found
    #[error("Transaction not found: {0}")]
    TransactionNotFound(String),

    /// Block not found
    #[error("Block not found: {0}")]
    BlockNotFound(BlockNumber),

    /// Block execution error
    #[error("Block execution error: {0}")]
    BlockExecutionError(#[from] BlockExecutionError),

    /// Run error (reuse from run module)
    #[error("Execution error: {0}")]
    RunError(#[from] crate::run::RunError),
}

/// Result type for the replay command
pub type Result<T> = std::result::Result<T, ReplayError>;
