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

impl BlockEnvAccess {
    /// Convert enum variant to bit position for bitmap tracking.
    const fn to_bit(self) -> u16 {
        match self {
            Self::BlockNumber => 1 << 0,
            Self::Timestamp => 1 << 1,
            Self::Coinbase => 1 << 2,
            Self::Difficulty => 1 << 3,
            Self::GasLimit => 1 << 4,
            Self::BaseFee => 1 << 5,
            Self::PrevRandao => 1 << 6,
            Self::BlockHash => 1 << 7,
            Self::BlobBaseFee => 1 << 8,
            Self::BlobHash => 1 << 9,
        }
    }
}

/// Bitmap-based tracking of block environment accesses.
/// More efficient than Vec<BlockEnvAccess> for frequent access checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockEnvAccessBitmap(u16);

impl BlockEnvAccessBitmap {
    /// Create a new empty bitmap.
    pub const fn new() -> Self {
        Self(0)
    }

    /// Mark a specific block environment access type as accessed.
    pub fn mark(&mut self, access_type: BlockEnvAccess) {
        self.0 |= access_type.to_bit();
    }

    /// Check if a specific block environment access type was accessed.
    pub const fn has_accessed(self, access_type: BlockEnvAccess) -> bool {
        (self.0 & access_type.to_bit()) != 0
    }

    /// Check if any block environment data was accessed.
    pub const fn has_any_access(self) -> bool {
        self.0 != 0
    }

    /// Clear all access flags.
    pub fn clear(&mut self) {
        self.0 = 0;
    }

    /// Count the number of accessed block environment types.
    pub fn count_accessed(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Get the raw bitmap value.
    pub const fn raw(self) -> u16 {
        self.0
    }
}
