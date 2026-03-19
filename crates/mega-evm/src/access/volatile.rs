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

    /// Converts a single-bit flag to its bit position as a `u8`.
    /// This matches the `VolatileDataAccessType` Solidity enum discriminant.
    ///
    /// # Panics
    ///
    /// Panics if `self` is empty (no bits set).
    pub fn as_u8(self) -> u8 {
        debug_assert!(!self.is_empty(), "cannot convert empty VolatileDataAccess to u8");
        self.bits().trailing_zeros() as u8
    }

    /// Returns only the block environment access portion as a separate bitflag.
    /// This is useful for compatibility with code that expects only block env flags.
    pub fn block_env_only(self) -> Self {
        Self::from_bits_truncate(self.bits() & Self::BLOCK_ENV_MASK)
    }
}

impl From<crate::VolatileDataAccessType> for VolatileDataAccess {
    fn from(ty: crate::VolatileDataAccessType) -> Self {
        Self::from_bits_truncate(1 << (ty as u8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VolatileDataAccessType;

    #[test]
    fn test_empty_access_has_no_flags() {
        let access = VolatileDataAccess::empty();

        assert!(!access.has_block_env_access());
        assert!(!access.has_beneficiary_balance_access());
        assert!(!access.has_oracle_access());
        assert_eq!(access.count_block_env_accessed(), 0);
        assert_eq!(access.count_accessed(), 0);
        assert_eq!(access.block_env_only(), VolatileDataAccess::empty());
        assert_eq!(access.raw(), 0);
    }

    #[test]
    fn test_block_env_helpers_ignore_non_block_flags() {
        let access = VolatileDataAccess::TIMESTAMP |
            VolatileDataAccess::BLOB_HASH |
            VolatileDataAccess::BENEFICIARY_BALANCE |
            VolatileDataAccess::ORACLE;

        assert!(access.has_block_env_access());
        assert!(access.has_beneficiary_balance_access());
        assert!(access.has_oracle_access());
        assert_eq!(access.count_block_env_accessed(), 2);
        assert_eq!(access.count_accessed(), 2);
        assert_eq!(
            access.block_env_only(),
            VolatileDataAccess::TIMESTAMP | VolatileDataAccess::BLOB_HASH
        );
        assert_eq!(access.raw(), access.bits());
    }

    #[test]
    fn test_from_volatile_data_access_type_covers_all_variants() {
        let expected: &[(VolatileDataAccessType, VolatileDataAccess)] = &[
            (VolatileDataAccessType::BlockNumber, VolatileDataAccess::BLOCK_NUMBER),
            (VolatileDataAccessType::Timestamp, VolatileDataAccess::TIMESTAMP),
            (VolatileDataAccessType::Coinbase, VolatileDataAccess::COINBASE),
            (VolatileDataAccessType::Difficulty, VolatileDataAccess::DIFFICULTY),
            (VolatileDataAccessType::GasLimit, VolatileDataAccess::GAS_LIMIT),
            (VolatileDataAccessType::BaseFee, VolatileDataAccess::BASE_FEE),
            (VolatileDataAccessType::PrevRandao, VolatileDataAccess::PREV_RANDAO),
            (VolatileDataAccessType::BlockHash, VolatileDataAccess::BLOCK_HASH),
            (VolatileDataAccessType::BlobBaseFee, VolatileDataAccess::BLOB_BASE_FEE),
            (VolatileDataAccessType::BlobHash, VolatileDataAccess::BLOB_HASH),
            (VolatileDataAccessType::Beneficiary, VolatileDataAccess::BENEFICIARY_BALANCE),
            (VolatileDataAccessType::Oracle, VolatileDataAccess::ORACLE),
        ];

        for &(access_type, expected_flag) in expected {
            let converted = VolatileDataAccess::from(access_type);
            assert_eq!(converted, expected_flag);
            assert_eq!(converted.as_u8(), expected_flag.as_u8());
        }
    }

    #[test]
    fn test_all_block_env_flags_counted_correctly() {
        let all_block_env = VolatileDataAccess::BLOCK_NUMBER |
            VolatileDataAccess::TIMESTAMP |
            VolatileDataAccess::COINBASE |
            VolatileDataAccess::DIFFICULTY |
            VolatileDataAccess::GAS_LIMIT |
            VolatileDataAccess::BASE_FEE |
            VolatileDataAccess::PREV_RANDAO |
            VolatileDataAccess::BLOCK_HASH |
            VolatileDataAccess::BLOB_BASE_FEE |
            VolatileDataAccess::BLOB_HASH;

        assert_eq!(all_block_env.count_block_env_accessed(), 10);
        assert!(all_block_env.has_block_env_access());
        assert!(!all_block_env.has_beneficiary_balance_access());
        assert!(!all_block_env.has_oracle_access());
        assert_eq!(all_block_env.block_env_only(), all_block_env);
    }
}
