//! Constants for the `MegaETH` EVM.
//!
//! It groups the constants for different EVM specs as sub-modules.

use alloy_primitives::{address, b256, Address, B256};

/// The `MegaETH` system address for deposit-like transaction processing.
/// Normal transactions sent from this address are processed as deposit transactions,
/// bypassing signature validation, nonce verification, and fee deduction.
///
/// TODO: change this address to one account that we have private key.
pub const MEGA_SYSTEM_ADDRESS: Address = address!("0xdeaddeaddeaddeaddeaddeaddeaddeaddead0002");

/// The source hash of the `MegaETH` system transaction, used to set the `source_hash` field of the
/// op deposit info. The value is `keccak256("MEGA_SYSTEM_TRANSACTION")`.
pub const MEGA_SYSTEM_TRANSACTION_SOURCE_HASH: B256 =
    b256!("852c082c0faff590c6300c2c34815d1f79882552fa95ba413cd5aeb1dba84957");

/// Constants for the `EQUIVALENCE` spec.
pub mod equivalence {
    use revm::interpreter::gas;

    /// Constants inherited from `revm`.
    pub use gas::{
        CALLVALUE, CALL_STIPEND, CODEDEPOSIT, COLD_SLOAD_COST, CREATE, KECCAK256WORD, LOG, LOGDATA,
        LOGTOPIC, SSTORE_RESET, SSTORE_SET, STANDARD_TOKEN_COST, WARM_SSTORE_RESET,
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

    /// The gas cost for setting a storage slot to a non-zero value for the `MINI_REX` spec.
    pub const SSTORE_SET_GAS: u64 = 2_000_000;
    /// The gas cost for creating a new account for the `MINI_REX` spec.
    pub const NEW_ACCOUNT_GAS: u64 = 2_000_000;
    /// The additional gas cost for creating a new contract for the `MINI_REX` spec. This is charged
    /// on top of the `NEW_ACCOUNT_GAS`.
    pub const CREATE_GAS: u64 = 2_000_000;
    /// The additional gas cost for `CODEDEPOSIT` for the `MINI_REX` spec, i.e., gas cost per byte
    /// for code deposit during contract creation. This is charged on
    /// top of the `CODEDEPOSIT` gas cost of standard EVM.
    pub const CODEDEPOSIT_ADDITIONAL_GAS: u64 = 2_000_000 / 32 - super::equivalence::CODEDEPOSIT;
    /// The gas cost for `LOGDATA` for the `MINI_REX` spec, i.e., gas cost per byte for log data.
    pub const LOG_DATA_GAS: u64 = super::equivalence::LOGDATA * 100;
    /// The gas cost for `LOGTOPIC` for the `MINI_REX` spec, i.e., gas cost per topic for log.
    pub const LOG_TOPIC_GAS: u64 = super::equivalence::LOGTOPIC * 100;
    /// The additional gas cost for `CALLDATA` for the `MINI_REX` spec, i.e., gas cost per token
    /// (one byte) for call data. This is charged on top of the calldata cost of standard EVM.
    pub const CALLDATA_STANDARD_TOKEN_ADDITIONAL_GAS: u64 =
        super::equivalence::STANDARD_TOKEN_COST * 100 - super::equivalence::STANDARD_TOKEN_COST;

    /// The maximum amount of data allowed to generate from a block for the `MINI_REX` spec.
    pub const BLOCK_DATA_LIMIT: u64 = 12 * 1024 * 1024 + 512 * 1024; // 12.5 MB
    /// The data amount to trigger the data bomb of the `MINI_REX` spec.
    pub const TX_DATA_LIMIT: u64 = BLOCK_DATA_LIMIT * 25 / 100; // 25% of the block limit
    /// The maximum amount of key-value updates allowed to generate from a transaction for the
    /// `MINI_REX` spec.
    pub const TX_KV_UPDATE_LIMIT: u64 = 1000;
}
