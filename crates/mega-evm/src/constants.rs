//! Constants for the `MegaETH` EVM.
//!
//! It groups the constants for different EVM specs as sub-modules.

/// Constants for the `EQUIVALENCE` spec.
pub mod equivalence {
    use revm::interpreter::gas;

    /// Constants inherited from `revm`.
    pub use gas::{
        BASE, BLOCKHASH, CALLVALUE, CALL_STIPEND, CODEDEPOSIT, COLD_SLOAD_COST, CREATE,
        KECCAK256WORD, LOG, LOGDATA, LOGTOPIC, NEWACCOUNT, SSTORE_RESET, SSTORE_SET,
        STANDARD_TOKEN_COST, TOTAL_COST_FLOOR_PER_TOKEN, VERYLOW, WARM_SSTORE_RESET,
        WARM_STORAGE_READ_COST,
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

    /// The maximum compute gas allowed per transaction for the `MINI_REX` spec.
    pub const TX_COMPUTE_GAS_LIMIT: u64 = 1_000_000_000;

    /// The base storage gas cost for setting a storage slot to a non-zero value for the `MINI_REX`
    /// spec. Actual cost is dynamically scaled by SALT bucket capacity: `SSTORE_SET_GAS ×
    /// (bucket_capacity / MIN_BUCKET_SIZE)`.
    pub const SSTORE_SET_STORAGE_GAS: u64 = 2_000_000;
    /// The base storage gas cost for creating a new account for the `MINI_REX` spec.
    /// Actual cost is dynamically scaled by SALT bucket capacity: `NEW_ACCOUNT_GAS ×
    /// (bucket_capacity / MIN_BUCKET_SIZE)`. Applied when transaction targets new account, CALL
    /// with transfer to empty account, or CREATE operations.
    pub const NEW_ACCOUNT_STORAGE_GAS: u64 = 2_000_000;
    /// Storage gas cost per byte for code deposit during contract creation in `MINI_REX` spec.
    pub const CODEDEPOSIT_STORAGE_GAS: u64 = 10_000;
    /// The storage gas cost for `LOGDATA` for the `MINI_REX` spec, i.e., gas cost per byte for log
    /// data.
    pub const LOG_DATA_STORAGE_GAS: u64 = super::equivalence::LOGDATA * 10;
    /// The storage gas cost for `LOGTOPIC` for the `MINI_REX` spec, i.e., gas cost per topic for
    /// log.
    pub const LOG_TOPIC_STORAGE_GAS: u64 = super::equivalence::LOGTOPIC * 10;
    /// The additional gas cost for `CALLDATA` for the `MINI_REX` spec, i.e., gas cost per token
    /// (one byte) for call data. This is charged on top of the calldata cost of standard EVM.
    pub const CALLDATA_STANDARD_TOKEN_STORAGE_GAS: u64 =
        super::equivalence::STANDARD_TOKEN_COST * 10;
    /// The additional gas cost for EIP-7623 floor gas cost, i.e., gas cost per token (one byte)
    /// for call data. This is charged on top of the floor cost of standard EVM.
    pub const CALLDATA_STANDARD_TOKEN_STORAGE_FLOOR_GAS: u64 =
        super::equivalence::TOTAL_COST_FLOOR_PER_TOKEN * 10;

    /// The maximum amount of data allowed to generate from a block for the `MINI_REX` spec.
    pub const BLOCK_DATA_LIMIT: u64 = 12 * 1024 * 1024 + 512 * 1024; // 12.5 MB
    /// The maximum data size allowed per transaction for the `MINI_REX` spec.
    /// Transactions exceeding this limit halt with `OutOfGas`, preserving remaining gas.
    pub const TX_DATA_LIMIT: u64 = BLOCK_DATA_LIMIT * 25 / 100; // 25% of the block limit
    /// The maximum amount of key-value updates allowed to generate from a block for the `MINI_REX`
    /// spec.
    pub const BLOCK_KV_UPDATE_LIMIT: u64 = 500_000;
    /// The maximum amount of key-value updates allowed to generate from a transaction for the
    /// `MINI_REX` spec.
    pub const TX_KV_UPDATE_LIMIT: u64 = BLOCK_KV_UPDATE_LIMIT * 25 / 100; // 25% of the block limit

    /// Gas limit after block environment or beneficiary data access.
    /// When block environment data or beneficiary account data is accessed, remaining gas is
    /// immediately limited to this value to force the transaction to complete quickly and
    /// prevent `DoS` attacks.
    pub const BLOCK_ENV_ACCESS_REMAINING_GAS: u64 = 20_000_000;

    /// Gas limit after oracle contract access.
    /// When oracle contract is accessed, remaining gas is immediately limited to this value
    /// to force the transaction to complete quickly and prevent `DoS` attacks.
    /// Note: If block environment was accessed first (20M gas limit), then oracle is accessed,
    /// the gas will be further restricted to this lower limit (1M gas).
    pub const ORACLE_ACCESS_REMAINING_GAS: u64 = 1_000_000;
}
