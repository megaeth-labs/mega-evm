use alloy_primitives::{hex::FromHexError, BlockNumber, TxHash};
use alloy_provider::transport::TransportError;
use mega_evm::{alloy_evm::block::BlockExecutionError, revm::bytecode::BytecodeDecodeError};

/// Error types for the replay command
#[derive(Debug, thiserror::Error)]
pub enum EvmeError {
    /// RPC transport error
    #[error("RPC transport error: {0}")]
    RpcTransportError(TransportError),

    /// Transaction not found
    #[error("Transaction not found: {0}")]
    TransactionNotFound(TxHash),

    /// Block not found
    #[error("Block not found: {0}")]
    BlockNotFound(BlockNumber),

    /// Block execution error
    #[error("Block execution error: {0}")]
    BlockExecutionError(#[from] BlockExecutionError),

    /// Invalid bytecode
    #[error("Invalid bytecode: {0}")]
    InvalidBytecode(#[from] BytecodeDecodeError),

    /// Failed to read file
    #[error("Failed to read file: {0}")]
    FileRead(#[from] std::io::Error),

    /// Invalid hex string
    #[error("Invalid hex string: {0}")]
    InvalidHex(#[from] FromHexError),

    /// EVM execution error
    #[error("EVM execution error: {0}")]
    ExecutionError(String),

    /// Invalid input
    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// RPC error
    #[error("RPC error: {0}")]
    RpcError(String),

    /// Other error
    #[error("Other error: {0}")]
    Other(String),
}

// Implement DBErrorMarker to allow EvmeError to be used as Database error type
impl mega_evm::revm::database::DBErrorMarker for EvmeError {}

/// Result type for the mega-evme command
pub type Result<T> = std::result::Result<T, EvmeError>;
