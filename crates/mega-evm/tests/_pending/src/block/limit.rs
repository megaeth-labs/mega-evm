//! Unit tests extracted from `crates/mega-evm/src/block/limit.rs` when the Satin skeleton replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/block/limit.rs`.
//! Owning mechanisms are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    fn limits_with_block_gas(block_gas_limit: u64) -> BlockLimits {
        let mut limits = BlockLimits::no_limits();
        limits.block_gas_limit = block_gas_limit;
        limits.tx_gas_limit = u64::MAX;
        limits
    }

    fn limits_with_block_tx_size(block_txs_encode_size_limit: u64) -> BlockLimits {
        let mut limits = BlockLimits::no_limits();
        limits.block_txs_encode_size_limit = block_txs_encode_size_limit;
        limits
    }

    fn limits_with_block_da_size(block_da_size_limit: u64) -> BlockLimits {
        let mut limits = BlockLimits::no_limits();
        limits.block_da_size_limit = block_da_size_limit;
        limits
    }

    #[test]
    fn test_pre_execution_check_block_gas_addition_saturates() {
        // Block has very high accumulated usage and a transaction with a near-`u64::MAX`
        // gas limit; unchecked addition would wrap and accidentally pass the limit check.
        let mut limiter = BlockLimiter::new(limits_with_block_gas(1_000_000));
        limiter.block_gas_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, u64::MAX, 0, 0, false);
        assert!(result.is_err(), "saturating_add must keep the rejection in place");
    }

    #[test]
    fn test_pre_execution_check_block_tx_size_addition_saturates() {
        let mut limiter = BlockLimiter::new(limits_with_block_tx_size(1_000_000));
        limiter.block_tx_size_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, 0, u64::MAX, 0, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_pre_execution_check_block_da_size_addition_saturates() {
        let mut limiter = BlockLimiter::new(limits_with_block_da_size(1_000_000));
        limiter.block_da_size_used = u64::MAX - 10;
        let result = limiter.pre_execution_check(B256::ZERO, 0, 0, u64::MAX, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_post_execution_update_raw_saturates_all_counters() {
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        limiter.block_gas_used = u64::MAX - 1;
        limiter.block_tx_size_used = u64::MAX - 1;
        limiter.block_da_size_used = u64::MAX - 1;
        limiter.block_data_used = u64::MAX - 1;
        limiter.block_kv_updates_used = u64::MAX - 1;
        limiter.block_compute_gas_used = u64::MAX - 1;
        limiter.block_state_growth_used = u64::MAX - 1;

        limiter.post_execution_update_raw(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            false,
        );

        assert_eq!(limiter.block_gas_used, u64::MAX);
        assert_eq!(limiter.block_tx_size_used, u64::MAX);
        assert_eq!(limiter.block_da_size_used, u64::MAX);
        assert_eq!(limiter.block_data_used, u64::MAX);
        assert_eq!(limiter.block_kv_updates_used, u64::MAX);
        assert_eq!(limiter.block_compute_gas_used, u64::MAX);
        assert_eq!(limiter.block_state_growth_used, u64::MAX);
    }

    #[test]
    fn test_post_execution_update_raw_skips_da_for_deposits() {
        // Deposit transactions should not advance `block_da_size_used`, even when the value
        // would otherwise saturate.
        let mut limiter = BlockLimiter::new(BlockLimits::no_limits());
        limiter.block_da_size_used = 100;

        limiter.post_execution_update_raw(0, 0, u64::MAX, 0, 0, 0, 0, true);

        assert_eq!(limiter.block_da_size_used, 100);
    }
}
