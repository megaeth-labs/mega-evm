//! Block environment access tracking.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    /// Bitmap-based tracking of block environment accesses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct BlockEnvAccess: u16 {
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
    }
}

impl Default for BlockEnvAccess {
    fn default() -> Self {
        Self::empty()
    }
}

impl BlockEnvAccess {
    /// Counts the number of accessed block environment types.
    pub fn count_accessed(self) -> usize {
        self.bits().count_ones() as usize
    }

    /// Gets the raw bitmap value.
    pub const fn raw(self) -> u16 {
        self.bits()
    }
}
