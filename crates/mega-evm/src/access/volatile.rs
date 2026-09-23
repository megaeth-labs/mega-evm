//! The kinds of volatile data a transaction can read.

use core::ops::{BitOr, BitOrAssign};

use crate::system::VolatileDataAccessType;

/// A set of the kinds of volatile data a transaction read.
///
/// One bit per kind. A bit's position is the discriminant of the kind in `MegaAccessControl`'s
/// `VolatileDataAccessType`, which is how a refused read names what it refused: bits 0 to 9 are
/// block-environment fields, bit 10 the block beneficiary's account and bit 11 the Oracle's
/// storage. Bit 12 is the block's slot number, which the contract's enum does not declare.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VolatileDataAccess(u16);

impl VolatileDataAccess {
    /// The block number (`NUMBER`, and the `BLOCKHASH` that compares against it).
    pub const BLOCK_NUMBER: Self = Self(1);
    /// The block timestamp (`TIMESTAMP`).
    pub const TIMESTAMP: Self = Self(1 << 1);
    /// The block beneficiary's address (`COINBASE`).
    pub const COINBASE: Self = Self(1 << 2);
    /// The block difficulty. Satin's base spec answers `DIFFICULTY` with the randomness instead,
    /// so no read sets it; the bit keeps its position.
    pub const DIFFICULTY: Self = Self(1 << 3);
    /// The block gas limit (`GASLIMIT`).
    pub const GAS_LIMIT: Self = Self(1 << 4);
    /// The base fee (`BASEFEE`).
    pub const BASE_FEE: Self = Self(1 << 5);
    /// The previous block's randomness (`PREVRANDAO`).
    pub const PREV_RANDAO: Self = Self(1 << 6);
    /// A block hash (`BLOCKHASH`).
    pub const BLOCK_HASH: Self = Self(1 << 7);
    /// The blob base fee (`BLOBBASEFEE`).
    pub const BLOB_BASE_FEE: Self = Self(1 << 8);
    /// A blob hash. `BLOBHASH` reads the transaction's own blob hashes, which nothing but the
    /// transaction decides, so no read sets it; the bit keeps its position.
    pub const BLOB_HASH: Self = Self(1 << 9);
    /// The block beneficiary's account: its balance, code or code hash.
    pub const BENEFICIARY_BALANCE: Self = Self(1 << 10);
    /// The Oracle contract's storage.
    pub const ORACLE: Self = Self(1 << 11);
    /// The block's slot number (`SLOTNUM`).
    pub const SLOT_NUM: Self = Self(1 << 12);

    /// The block-environment kinds: bits 0 to 9, and the slot number.
    const BLOCK_ENV_MASK: u16 = 0b0001_0011_1111_1111;

    /// No kind at all.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Whether no kind is in the set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The bits of the set.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// The bits of the set. An alias of [`bits`](Self::bits).
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Whether every kind in `other` is in the set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Adds the kinds of `other`.
    pub const fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Whether the set holds a block-environment kind.
    pub const fn has_block_env_access(self) -> bool {
        self.0 & Self::BLOCK_ENV_MASK != 0
    }

    /// Whether the set holds the beneficiary's account.
    pub const fn has_beneficiary_balance_access(self) -> bool {
        self.contains(Self::BENEFICIARY_BALANCE)
    }

    /// Whether the set holds the Oracle's storage.
    pub const fn has_oracle_access(self) -> bool {
        self.contains(Self::ORACLE)
    }

    /// How many block-environment kinds the set holds.
    pub const fn count_block_env_accessed(self) -> usize {
        (self.0 & Self::BLOCK_ENV_MASK).count_ones() as usize
    }

    /// How many block-environment kinds the set holds. An alias of
    /// [`count_block_env_accessed`](Self::count_block_env_accessed).
    pub const fn count_accessed(self) -> usize {
        self.count_block_env_accessed()
    }

    /// The block-environment kinds of the set.
    pub const fn block_env_only(self) -> Self {
        Self(self.0 & Self::BLOCK_ENV_MASK)
    }

    /// The position of the set's lowest bit, which for a single kind is its discriminant in
    /// `VolatileDataAccessType`, or the slot number's 12.
    ///
    /// # Panics
    ///
    /// In a debug build, when the set is empty.
    pub const fn as_u8(self) -> u8 {
        debug_assert!(!self.is_empty(), "an empty set names no kind");
        self.0.trailing_zeros() as u8
    }
}

impl BitOr for VolatileDataAccess {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for VolatileDataAccess {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl From<VolatileDataAccessType> for VolatileDataAccess {
    fn from(ty: VolatileDataAccessType) -> Self {
        Self(1_u16.checked_shl(ty as u32).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_access_has_no_flags() {
        let access = VolatileDataAccess::empty();

        assert!(access.is_empty());
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

        assert!(!access.is_empty());
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

        let beneficiary_and_oracle =
            VolatileDataAccess::BENEFICIARY_BALANCE | VolatileDataAccess::ORACLE;
        assert!(!beneficiary_and_oracle.has_block_env_access());
        assert!(!VolatileDataAccess::ORACLE.has_beneficiary_balance_access());
        assert!(!VolatileDataAccess::BENEFICIARY_BALANCE.has_oracle_access());
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
            assert_eq!(converted.as_u8(), access_type as u8);
            assert_eq!(converted.as_u8(), expected_flag.as_u8());
        }
        // The slot number has no variant of its own: its bit is the one after the enum's.
        assert_eq!(VolatileDataAccess::SLOT_NUM.as_u8(), 12);
    }

    /// Every block-environment kind, the slot number included, counts as one; the beneficiary
    /// and the Oracle are not the block environment.
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
            VolatileDataAccess::BLOB_HASH |
            VolatileDataAccess::SLOT_NUM;

        assert_eq!(all_block_env.count_block_env_accessed(), 11);
        assert!(all_block_env.has_block_env_access());
        assert!(!all_block_env.has_beneficiary_balance_access());
        assert!(!all_block_env.has_oracle_access());
        assert_eq!(all_block_env.block_env_only(), all_block_env);
    }

    /// A union keeps a kind both sides hold: it is a union, not a difference.
    #[test]
    fn test_a_union_keeps_what_both_sides_hold() {
        let both = VolatileDataAccess::TIMESTAMP | VolatileDataAccess::ORACLE;
        assert_eq!(both | VolatileDataAccess::TIMESTAMP, both);
        let mut assigned = both;
        assigned |= VolatileDataAccess::ORACLE;
        assert_eq!(assigned, both);
        let mut inserted = both;
        inserted.insert(both);
        assert_eq!(inserted, both);
    }

    /// Inserting is a union, and `contains` asks for every bit of its argument.
    #[test]
    fn test_insert_and_contains() {
        let mut access = VolatileDataAccess::empty();
        access.insert(VolatileDataAccess::TIMESTAMP);
        access |= VolatileDataAccess::ORACLE;
        assert!(access.contains(VolatileDataAccess::TIMESTAMP));
        assert!(access.contains(VolatileDataAccess::TIMESTAMP | VolatileDataAccess::ORACLE));
        assert!(!access.contains(VolatileDataAccess::TIMESTAMP | VolatileDataAccess::COINBASE));
        assert!(access.contains(VolatileDataAccess::empty()));
    }
}
