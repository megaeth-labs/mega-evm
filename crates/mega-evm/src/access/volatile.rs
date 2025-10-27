//! Volatile data access bitflags.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    /// Unified bitmap for tracking all types of volatile data access.
    ///
    /// This combines both fine-grained block environment tracking and coarse-grained
    /// tracking of beneficiary balance and oracle contract access.
    ///
    /// Bits 0-9: Specific block environment fields (10 bits)
    /// Bit 10: Beneficiary balance access
    /// Bit 11: Oracle contract access
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct VolatileDataAccess: u16 {
        // Block environment fields (bits 0-9)
        /// Block number (NUMBER opcode)
        const BLOCK_NUMBER = 1 << 0;
        /// Block timestamp (TIMESTAMP opcode)
        const TIMESTAMP = 1 << 1;
        /// Block coinbase/beneficiary (COINBASE opcode)
        const COINBASE = 1 << 2;
        /// Block difficulty (DIFFICULTY opcode)
        const DIFFICULTY = 1 << 3;
        /// Block gas limit (GASLIMIT opcode)
        const GAS_LIMIT = 1 << 4;
        /// Base fee per gas (BASEFEE opcode)
        const BASE_FEE = 1 << 5;
        /// Previous block randomness (PREVRANDAO opcode)
        const PREV_RANDAO = 1 << 6;
        /// Block hash lookup (BLOCKHASH opcode)
        const BLOCK_HASH = 1 << 7;
        /// Blob base fee per gas (BLOBBASEFEE opcode)
        const BLOB_BASE_FEE = 1 << 8;
        /// Blob hash lookup (BLOBHASH opcode)
        const BLOB_HASH = 1 << 9;

        // Other volatile data types (bits 10-11)
        /// Beneficiary balance was accessed
        const BENEFICIARY_BALANCE = 1 << 10;
        /// Oracle contract was accessed
        const ORACLE = 1 << 11;
    }
}

impl VolatileDataAccess {
    /// Mask for all block environment access flags (bits 0-9).
    const BLOCK_ENV_MASK: u16 = 0b0000_0011_1111_1111;

    /// Checks if any block environment data has been accessed.
    pub fn has_block_env_access(self) -> bool {
        (self.bits() & Self::BLOCK_ENV_MASK) != 0
    }

    /// Checks if beneficiary balance has been accessed.
    pub fn has_beneficiary_balance_access(self) -> bool {
        self.contains(Self::BENEFICIARY_BALANCE)
    }

    /// Checks if oracle contract has been accessed.
    pub fn has_oracle_access(self) -> bool {
        self.contains(Self::ORACLE)
    }

    /// Counts the number of distinct block environment fields accessed.
    pub fn count_block_env_accessed(self) -> usize {
        (self.bits() & Self::BLOCK_ENV_MASK).count_ones() as usize
    }

    /// Counts the total number of accessed volatile data types.
    /// This is an alias for `count_block_env_accessed` for backward compatibility.
    pub fn count_accessed(self) -> usize {
        self.count_block_env_accessed()
    }

    /// Gets the raw bitmap value.
    pub const fn raw(self) -> u16 {
        self.bits()
    }

    /// Returns only the block environment access portion as a separate bitflag.
    /// This is useful for compatibility with code that expects only block env flags.
    pub fn block_env_only(self) -> Self {
        Self::from_bits_truncate(self.bits() & Self::BLOCK_ENV_MASK)
    }
}
