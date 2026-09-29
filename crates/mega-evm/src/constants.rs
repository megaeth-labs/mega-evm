//! Numeric constants of the Satin engine.
//!
//! Every value is *provisional*: a placeholder until the economics sign-off fixes the numbers.
//! A constant that no code reads yet is here so the whole placeholder set can be reviewed in
//! one place.
//!
//! | Constant | Value | Meaning | Read by |
//! |---|---:|---|---|
//! | [`COST_PER_STATE_BYTE`] | 1,530 | gas per byte of new state (EIP-8037 CPSB) | the Satin gas schedule |
//! | [`SLOT_STATE_GAS`] | 97,920 | state gas of one new storage slot (64 bytes) | tests of the Satin gas schedule |
//! | [`ACCOUNT_STATE_GAS`] | 183,600 | state gas of one new account (120 bytes) | tests of the Satin gas schedule |
//! | [`COST_PER_HISTORY_BYTE`] | 88 | gas per history byte (CPHB) | history gas, and the schedule's deposited-code entry |
//! | [`TX_GAS_LIMIT_CAP`] | 200,000,000 | execution cap: regular gas one transaction may spend | the spec configuration |
//! | [`MAX_CONTRACT_SIZE`] | 524,288 | the most bytes a deployed contract may hold | the spec configuration |
//! | [`MAX_INITCODE_SIZE`] | 1,048,576 | the most bytes an initcode may hold | the spec configuration |
//! | [`TX_DATA_LIMIT`] | 13,107,200 | data size one transaction may produce (as Rex6) | the default protocol limits |
//! | [`BLOCK_DATA_LIMIT`] | 13,107,200 | data size one block may produce (as Rex6) | the default protocol limits |
//! | [`BLOCK_ENV_ACCESS_COMPUTE_GAS`] | 20,000,000 | compute a transaction may still spend once it read the block environment or the beneficiary (as Rex6) | the default detention cap of the runtime limits and of the protocol limits |
//! | [`ORACLE_ACCESS_COMPUTE_GAS`] | 20,000,000 | compute a transaction may still spend once it read the Oracle's storage (as Rex6) | the default detention cap of the runtime limits and of the protocol limits |
//! | [`TX_COMPUTE_GAS_BOUND`] | 199,988,000 | a bound no transaction's compute reaches: the execution cap less EIP-2780's base cost | the protocol limits, whose detention caps must be below it |
//!
//! The Satin gas schedule builds its state-gas entries from the EIP-8037 byte counts at
//! [`COST_PER_STATE_BYTE`], read through [`SatinPrices`](crate::SatinPrices) so a measurement
//! build can run other byte prices without touching this table. [`SLOT_STATE_GAS`] and
//! [`ACCOUNT_STATE_GAS`] are two of those products written out: the tests assert the schedule
//! against them, and changing one of the two moves a test rather than a price.
//! [`TX_DATA_LIMIT`], [`BLOCK_DATA_LIMIT`] and the two detention caps are the defaults of the
//! Satin fork's parameters, [`ProtocolLimits::DEFAULT`](crate::ProtocolLimits::DEFAULT): a chain
//! carries its own in its configuration, and block execution reads them from there. The state-gas
//! limits have no constant at all: the per-transaction one is
//! [`EvmTxRuntimeLimits::tx_state_gas_limit`](crate::EvmTxRuntimeLimits::tx_state_gas_limit) and
//! the per-block one
//! [`ProtocolLimits::block_state_gas_limit`](crate::ProtocolLimits::block_state_gas_limit). A chain
//! sets both, and both are unlimited by default.

use revm::primitives::{
    eip2780,
    eip8037::{NEW_ACCOUNT_BYTES, SSTORE_SET_BYTES},
};

/// Gas per byte of new state, the EIP-8037 `COST_PER_STATE_BYTE`. Provisional.
pub const COST_PER_STATE_BYTE: u64 = 1_530;

/// State gas of one new storage slot: the EIP-8037 slot size times
/// [`COST_PER_STATE_BYTE`]. Provisional.
pub const SLOT_STATE_GAS: u64 = SSTORE_SET_BYTES * COST_PER_STATE_BYTE;

/// State gas of one new account: the EIP-8037 account size times
/// [`COST_PER_STATE_BYTE`]. Provisional.
pub const ACCOUNT_STATE_GAS: u64 = NEW_ACCOUNT_BYTES * COST_PER_STATE_BYTE;

/// Gas per history byte (logs, deployed code, write records, transaction body). Provisional.
///
/// Every history charge is a byte count from the byte table at this price, read through
/// [`SatinPrices`](crate::SatinPrices) so a measurement build can run other prices; the schedule's
/// `code_deposit_history_gas` entry is this number, which is the one history charge revm makes
/// itself.
pub const COST_PER_HISTORY_BYTE: u64 = 88;

/// The execution cap: the most regular gas one transaction may spend. Gas above it goes to
/// the EIP-8037 state-gas reservoir. Provisional.
pub const TX_GAS_LIMIT_CAP: u64 = 200_000_000;

/// The most bytes a deployed contract may hold, replacing the EIP-170 limit. Provisional.
pub const MAX_CONTRACT_SIZE: usize = 512 * 1024;

/// The most bytes an initcode may hold: twice [`MAX_CONTRACT_SIZE`], the ratio EIP-3860 sets
/// between the two. Provisional.
pub const MAX_INITCODE_SIZE: usize = 2 * MAX_CONTRACT_SIZE;

/// The most data one transaction may produce, 12.5 MiB as in Rex6. Provisional.
pub const TX_DATA_LIMIT: u64 = 12 * 1024 * 1024 + 512 * 1024;

/// The most data one block may produce, 12.5 MiB as in Rex6. Provisional.
pub const BLOCK_DATA_LIMIT: u64 = 12 * 1024 * 1024 + 512 * 1024;

/// The compute a transaction may still spend once it read the block environment or the block
/// beneficiary's account: its compute at the read plus this. The value and its relative reading
/// are Rex6's. Provisional.
pub const BLOCK_ENV_ACCESS_COMPUTE_GAS: u64 = 20_000_000;

/// The compute a transaction may still spend once it read the Oracle's storage: its compute at
/// the read plus this. The value and its relative reading are Rex6's; the legacy engine's first
/// specs capped an Oracle read at 1,000,000. Provisional.
pub const ORACLE_ACCESS_COMPUTE_GAS: u64 = 20_000_000;

/// A bound no transaction's compute reaches: the execution cap less EIP-2780's base cost, the
/// intrinsic regular gas every transaction pays before its first frame. Derived from
/// [`TX_GAS_LIMIT_CAP`].
///
/// Compute is the regular gas a transaction's frames spend, and their regular gas is what the
/// execution cap leaves once the intrinsic gas is paid, so a gas-detention cap at or above this
/// bound never stops a transaction, however early it read: a chain's caps must be below it. A
/// frame that runs code pays a little more before its first instruction — at least a warm account
/// access, for the recipient or the delegate it runs — so the most compute a transaction can
/// spend is a little below the bound.
pub const TX_COMPUTE_GAS_BOUND: u64 = TX_GAS_LIMIT_CAP - eip2780::TX_BASE_COST;

#[cfg(test)]
mod tests {
    use super::*;

    /// The placeholder values, written out so a change to one is a visible diff.
    #[test]
    fn test_the_provisional_values_are_written_out() {
        assert_eq!(COST_PER_STATE_BYTE, 1_530);
        assert_eq!(SLOT_STATE_GAS, 97_920);
        assert_eq!(ACCOUNT_STATE_GAS, 183_600);
        assert_eq!(COST_PER_HISTORY_BYTE, 88);
        assert_eq!(TX_GAS_LIMIT_CAP, 200_000_000);
        assert_eq!(MAX_CONTRACT_SIZE, 524_288);
        assert_eq!(MAX_INITCODE_SIZE, 1_048_576);
        assert_eq!(TX_DATA_LIMIT, 13_107_200);
        assert_eq!(BLOCK_DATA_LIMIT, 13_107_200);
        assert_eq!(BLOCK_ENV_ACCESS_COMPUTE_GAS, 20_000_000);
        assert_eq!(ORACLE_ACCESS_COMPUTE_GAS, 20_000_000);
        assert_eq!(TX_COMPUTE_GAS_BOUND, 199_988_000);
    }
}
