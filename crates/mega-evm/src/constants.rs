//! Constants for the `MegaETH` EVM.
//!
//! It groups the constants for different EVM specs as sub-modules.

/// Constants for the `EQUIVALENCE` spec.
pub mod equivalence {
    use revm::interpreter::gas;

    /// Constants inherited from `revm`.
    pub use gas::{
        CALLVALUE, CALL_STIPEND, CODEDEPOSIT, COLD_SLOAD_COST, CREATE, KECCAK256WORD, LOG, LOGDATA,
        LOGTOPIC, SSTORE_RESET, SSTORE_SET, STANDARD_TOKEN_COST, TOTAL_COST_FLOOR_PER_TOKEN,
        WARM_SSTORE_RESET, WARM_STORAGE_READ_COST,
    };
    pub use revm::primitives::STACK_LIMIT;
}

/// Constants for the `MINI_REX` spec.
pub mod mini_rex {
    /// The maximum contract size for the `MINI_REX` spec.
    pub const MAX_CONTRACT_SIZE: usize = 512 * 1024;
    /// The additional initcode size for the `MINI_REX` spec. The initcode size is limited to
    /// `MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE`.
    pub const ADDITIONAL_INITCODE_SIZE: usize = 24 * 1024;
    /// The maximum initcode size for the `MINI_REX` spec.
    pub const MAX_INITCODE_SIZE: usize = MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE;

    /// The base gas cost for setting a storage slot to a non-zero value for the `MINI_REX` spec.
    /// Actual cost is dynamically scaled by SALT bucket capacity: `SSTORE_SET_GAS ×
    /// (bucket_capacity / MIN_BUCKET_SIZE)`.
    pub const SSTORE_SET_GAS: u64 = 2_000_000;
    /// The base gas cost for creating a new account for the `MINI_REX` spec.
    /// Actual cost is dynamically scaled by SALT bucket capacity: `NEW_ACCOUNT_GAS ×
    /// (bucket_capacity / MIN_BUCKET_SIZE)`. Applied when transaction targets new account, CALL
    /// with transfer to empty account, or CREATE operations.
    pub const NEW_ACCOUNT_GAS: u64 = 2_000_000;
    /// Additional flat gas cost for creating a new contract for the `MINI_REX` spec.
    /// This is a fixed cost charged on top of the dynamically-scaled `NEW_ACCOUNT_GAS`.
    /// Applied to both CREATE and CREATE2 operations.
    pub const CREATE_GAS: u64 = 2_000_000;
    /// Additional gas cost per byte for code deposit during contract creation in `MINI_REX` spec.
    /// Total cost per byte: ~62,500 gas (200 standard + 62,300 additional).
    /// Formula: `2_000_000 / 32 - CODEDEPOSIT` where CODEDEPOSIT = 200.
    pub const CODEDEPOSIT_ADDITIONAL_GAS: u64 = 2_000_000 / 32 - super::equivalence::CODEDEPOSIT;
    /// The gas cost for `LOGDATA` for the `MINI_REX` spec, i.e., gas cost per byte for log data.
    pub const LOG_DATA_GAS: u64 = super::equivalence::LOGDATA * 10;
    /// The gas cost for `LOGTOPIC` for the `MINI_REX` spec, i.e., gas cost per topic for log.
    pub const LOG_TOPIC_GAS: u64 = super::equivalence::LOGTOPIC * 10;
    /// The additional gas cost for `CALLDATA` for the `MINI_REX` spec, i.e., gas cost per token
    /// (one byte) for call data. This is charged on top of the calldata cost of standard EVM.
    pub const CALLDATA_STANDARD_TOKEN_ADDITIONAL_GAS: u64 =
        super::equivalence::STANDARD_TOKEN_COST * 10 - super::equivalence::STANDARD_TOKEN_COST;
    /// The additional gas cost for EIP-7623 floor gas cost, i.e., gas cost per token (one byte)
    /// for call data. This is charged on top of the floor cost of standard EVM.
    pub const CALLDATA_STANDARD_TOKEN_ADDITIONAL_FLOOR_GAS: u64 =
        super::equivalence::TOTAL_COST_FLOOR_PER_TOKEN * 10 -
            super::equivalence::TOTAL_COST_FLOOR_PER_TOKEN;

    /// The maximum amount of data allowed to generate from a block for the `MINI_REX` spec.
    pub const BLOCK_DATA_LIMIT: u64 = 12 * 1024 * 1024 + 512 * 1024; // 12.5 MB
    /// The maximum data size allowed per transaction for the `MINI_REX` spec.
    /// Transactions exceeding this limit halt with `OutOfGas`, consuming all remaining gas.
    pub const TX_DATA_LIMIT: u64 = BLOCK_DATA_LIMIT * 25 / 100; // 25% of the block limit
    /// The maximum amount of key-value updates allowed to generate from a block for the `MINI_REX`
    /// spec.
    pub const BLOCK_KV_UPDATE_LIMIT: u64 = 500_000;
    /// The maximum amount of key-value updates allowed to generate from a transaction for the
    /// `MINI_REX` spec.
    pub const TX_KV_UPDATE_LIMIT: u64 = BLOCK_KV_UPDATE_LIMIT * 25 / 100; // 25% of the block limit
}
