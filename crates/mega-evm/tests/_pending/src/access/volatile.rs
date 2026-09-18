//! Unit tests extracted from `crates/mega-evm/src/access/volatile.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/access/volatile.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

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
