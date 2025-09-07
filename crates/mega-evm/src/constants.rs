//! Constants for the `MegaETH` EVM.
//!
//! It groups the constants for different EVM specs as sub-modules.

/// Constants for the `EQUIVALENCE` spec.
pub mod equivalence {
    use revm::interpreter::gas;

    /// Constants inherited from `revm`.
    pub use gas::{
        CALLVALUE, CALL_STIPEND, COLD_SLOAD_COST, KECCAK256WORD, LOG, LOGDATA, WARM_SSTORE_RESET,
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

    /// The cost of a log topic for the `MINI_REX` spec.
    pub const LOG_TOPIC_COST: u64 = 10000;

    /// The gas cost for setting a storage slot to a non-zero value for the `MINI_REX` spec.
    pub const SSTORE_SET_GAS: u64 = 2_000_000;
    /// The gas cost for creating a new account for the `MINI_REX` spec.
    pub const NEW_ACCOUNT_GAS: u64 = 2_000_000;
    /// The gas cost for creating a new contract for the `MINI_REX` spec.
    pub const CREATE_GAS: u64 = NEW_ACCOUNT_GAS;

    /// The maximum amount of data allowed to generate from a block for the `MINI_REX` spec.
    pub const BLOCK_DATA_LIMIT: u64 = 12 * 1024 * 1024 + 512 * 1024; // 12.5 MB
    /// The data amount to trigger the data bomb of the `MINI_REX` spec.
    pub const TX_DATA_LIMIT: u64 = BLOCK_DATA_LIMIT * 25 / 100; // 25% of the block limit
    /// The maximum amount of key-value updates allowed to generate from a transaction for the
    /// `MINI_REX` spec.
    pub const TX_KV_UPDATE_LIMIT: u64 = 1000;
}
