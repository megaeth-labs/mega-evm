//! Numeric constants of the Satin engine.
//!
//! Every value is *provisional*: a placeholder until the economics sign-off fixes the numbers.
//! A constant that no code reads yet is here so the whole placeholder set can be reviewed in
//! one place.
//!
//! | Constant | Value | Meaning | Read from |
//! |---|---:|---|---|
//! | [`COST_PER_STATE_BYTE`] | 1,530 | gas per byte of new state (EIP-8037 CPSB) | T3.1 |
//! | [`SLOT_STATE_GAS`] | 97,920 | state gas of one new storage slot (64 bytes) | T3.1 |
//! | [`ACCOUNT_STATE_GAS`] | 183,600 | state gas of one new account (120 bytes) | T3.1 |
//! | [`COST_PER_HISTORY_BYTE`] | 88 | gas per history byte (CPHB) | T3.3 |
//! | [`TX_GAS_LIMIT_CAP`] | 200,000,000 | execution cap: regular gas one transaction may spend | now |
//! | [`TX_DATA_LIMIT`] | 13,107,200 | data size one transaction may produce (as Rex6) | T4.1 |
//! | [`BLOCK_DATA_LIMIT`] | 13,107,200 | data size one block may produce (as Rex6) | T4.1 |
//!
//! The storage call stipend and the per-transaction and per-block state-gas limits are further
//! placeholders; T3.3 and T4.3 add them with the mechanisms that read them.

use revm::primitives::eip8037::{NEW_ACCOUNT_BYTES, SSTORE_SET_BYTES};

/// Gas per byte of new state, the EIP-8037 `COST_PER_STATE_BYTE`. Provisional.
pub const COST_PER_STATE_BYTE: u64 = 1_530;

/// State gas of one new storage slot: the EIP-8037 slot size times
/// [`COST_PER_STATE_BYTE`]. Provisional.
pub const SLOT_STATE_GAS: u64 = SSTORE_SET_BYTES * COST_PER_STATE_BYTE;

/// State gas of one new account: the EIP-8037 account size times
/// [`COST_PER_STATE_BYTE`]. Provisional.
pub const ACCOUNT_STATE_GAS: u64 = NEW_ACCOUNT_BYTES * COST_PER_STATE_BYTE;

/// Gas per history byte (logs, deployed code, write records, transaction body). Provisional.
pub const COST_PER_HISTORY_BYTE: u64 = 88;

/// The execution cap: the most regular gas one transaction may spend. Gas above it goes to
/// the EIP-8037 state-gas reservoir. Provisional.
pub const TX_GAS_LIMIT_CAP: u64 = 200_000_000;

/// The most data one transaction may produce, 12.5 MiB as in Rex6. Provisional.
pub const TX_DATA_LIMIT: u64 = 12 * 1024 * 1024 + 512 * 1024;

/// The most data one block may produce, 12.5 MiB as in Rex6. Provisional.
pub const BLOCK_DATA_LIMIT: u64 = 12 * 1024 * 1024 + 512 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    /// The placeholder values, written out so a change to one is a visible diff.
    #[test]
    fn test_provisional_values_match_the_work_order() {
        assert_eq!(COST_PER_STATE_BYTE, 1_530);
        assert_eq!(SLOT_STATE_GAS, 97_920);
        assert_eq!(ACCOUNT_STATE_GAS, 183_600);
        assert_eq!(COST_PER_HISTORY_BYTE, 88);
        assert_eq!(TX_GAS_LIMIT_CAP, 200_000_000);
        assert_eq!(TX_DATA_LIMIT, 13_107_200);
        assert_eq!(BLOCK_DATA_LIMIT, 13_107_200);
    }
}
