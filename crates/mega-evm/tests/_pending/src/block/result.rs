//! Unit tests extracted from `crates/mega-evm/src/block/result.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/block/result.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_limit_error_reports_usage_and_limit() {
        let cases = [
            (MegaTxLimitExceededError::TransactionGasLimit { tx_gas_limit: 31, limit: 30 }, 31, 30),
            (
                MegaTxLimitExceededError::TransactionEncodeSizeLimit { tx_size: 101, limit: 100 },
                101,
                100,
            ),
            (
                MegaTxLimitExceededError::DataAvailabilitySizeLimit { da_size: 11, limit: 10 },
                11,
                10,
            ),
        ];

        for (error, expected_usage, expected_limit) in cases {
            assert_eq!(error.usage(), expected_usage);
            assert_eq!(error.limit(), expected_limit);
            assert!(!error.is_nonce_too_low());
        }
    }

    #[test]
    fn test_block_limit_error_reports_block_usage_and_limit() {
        let cases = [
            (MegaBlockLimitExceededError::TransactionDataLimit { block_used: 1, limit: 10 }, 1, 10),
            (MegaBlockLimitExceededError::KVUpdateLimit { block_used: 2, limit: 11 }, 2, 11),
            (MegaBlockLimitExceededError::ComputeGasLimit { block_used: 3, limit: 12 }, 3, 12),
            (
                MegaBlockLimitExceededError::TransactionEncodeSizeLimit {
                    block_used: 4,
                    tx_used: 1,
                    limit: 13,
                },
                4,
                13,
            ),
            (
                MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                    block_used: 5,
                    tx_used: 1,
                    limit: 14,
                },
                5,
                14,
            ),
            (MegaBlockLimitExceededError::StateGrowthLimit { block_used: 6, limit: 15 }, 6, 15),
        ];

        for (error, expected_block_used, expected_limit) in cases {
            assert_eq!(error.block_used(), expected_block_used);
            assert_eq!(error.limit(), expected_limit);
            assert!(!error.is_nonce_too_low());
        }
    }
}
