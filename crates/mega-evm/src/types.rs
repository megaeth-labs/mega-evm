use revm::context::TxEnv;
use serde::{Deserialize, Serialize};

/// `MegaETH` halt reason type.
pub type HaltReason = op_revm::OpHaltReason;

/// `MegaETH` EVM execution transaction error type.
pub type TransactionError = op_revm::OpTransactionError;

/// `MegaETH` transaction type used in revm.
pub type Transaction = op_revm::OpTransaction<TxEnv>;

/// `MegaETH` precompiles type.
pub type Precompiles = op_revm::precompiles::OpPrecompiles;

/// `MegaETH` transaction type.
pub type TxType = op_alloy_consensus::OpTxType;

/// Types of block environment data that can be accessed during transaction execution.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BlockEnvAccess {
    /// Block number (NUMBER opcode)
    BlockNumber,
    /// Block timestamp (TIMESTAMP opcode)
    Timestamp,
    /// Block coinbase/beneficiary (COINBASE opcode)
    Coinbase,
    /// Block difficulty (DIFFICULTY opcode)
    Difficulty,
    /// Block gas limit (GASLIMIT opcode)
    GasLimit,
    /// Base fee per gas (BASEFEE opcode)
    BaseFee,
    /// Previous block randomness (PREVRANDAO opcode)
    PrevRandao,
    /// Block hash lookup (BLOCKHASH opcode)
    BlockHash,
    /// Blob base fee per gas (BLOBBASEFEE opcode)
    BlobBaseFee,
    /// Blob hash lookup (BLOBHASH opcode)  
    BlobHash,
}

/// List of block environment accesses made during transaction execution.
/// Preserves order and allows duplicate entries.
pub type BlockEnvAccessVec = Vec<BlockEnvAccess>;
