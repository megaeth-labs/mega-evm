//! Value-pinning tests for the arithmetic `pub const` definitions in
//! `src/constants.rs`.
//!
//! Several constants are defined via compile-time arithmetic (e.g.
//! `12 * 1024 * 1024 + 512 * 1024`). Mutation testing replaces individual
//! operators (`*`→`+`, `+`→`-`, `/`→`*`, …) inside these expressions, which
//! changes the resulting numeric value but leaves the program compiling. None
//! of the targeted expressions are no-ops, so every operator swap is a true
//! behavioral change.
//!
//! Each test below asserts the constant equals its exact unmutated value. Any
//! arithmetic mutation on the relevant line yields a different value and is
//! therefore killed.

use mega_evm::constants::{mini_rex, rex};

/// Pins `mini_rex::ADDITIONAL_INITCODE_SIZE` (`24 * 1024`).
///
/// Kills `constants.rs:25:52 replace * with /` and `:25:52 replace * with +`.
#[test]
fn mini_rex_additional_initcode_size() {
    assert_eq!(mini_rex::ADDITIONAL_INITCODE_SIZE, 24 * 1024);
    assert_eq!(mini_rex::ADDITIONAL_INITCODE_SIZE, 24_576);
}

/// Pins `mini_rex::MAX_INITCODE_SIZE`
/// (`MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE`).
///
/// Kills `constants.rs:27:60 replace + with -`.
#[test]
fn mini_rex_max_initcode_size() {
    // 512 * 1024 + 24 * 1024 = 524_288 + 24_576
    assert_eq!(mini_rex::MAX_INITCODE_SIZE, 548_864);
    assert_eq!(
        mini_rex::MAX_INITCODE_SIZE,
        mini_rex::MAX_CONTRACT_SIZE + mini_rex::ADDITIONAL_INITCODE_SIZE
    );
}

/// Pins `mini_rex::BLOCK_DATA_LIMIT` (`12 * 1024 * 1024 + 512 * 1024`, 12.5 MB).
///
/// Kills `constants.rs:59:62 replace * with +`, `:59:56 replace + with -`,
/// `:59:62 replace * with /`, and `:59:56 replace + with *`.
#[test]
fn mini_rex_block_data_limit() {
    assert_eq!(mini_rex::BLOCK_DATA_LIMIT, 12 * 1024 * 1024 + 512 * 1024);
    assert_eq!(mini_rex::BLOCK_DATA_LIMIT, 13_107_200);
}

/// Pins `mini_rex::TX_DATA_LIMIT` (`BLOCK_DATA_LIMIT * 25 / 100`, 25% of block).
///
/// Kills `constants.rs:62:58 replace / with *` and `:62:53 replace * with +`.
#[test]
fn mini_rex_tx_data_limit() {
    assert_eq!(mini_rex::TX_DATA_LIMIT, mini_rex::BLOCK_DATA_LIMIT * 25 / 100);
    assert_eq!(mini_rex::TX_DATA_LIMIT, 3_276_800);
}

/// Pins `mini_rex::TX_KV_UPDATE_LIMIT`
/// (`BLOCK_KV_UPDATE_LIMIT * 25 / 100`, 25% of block).
///
/// Kills `constants.rs:68:68 replace / with *` and `:68:63 replace * with +`.
#[test]
fn mini_rex_tx_kv_update_limit() {
    assert_eq!(mini_rex::TX_KV_UPDATE_LIMIT, mini_rex::BLOCK_KV_UPDATE_LIMIT * 25 / 100);
    assert_eq!(mini_rex::TX_KV_UPDATE_LIMIT, 125_000);
}

/// Pins `rex::TX_DATA_LIMIT` (`12 * 1024 * 1024 + 512 * 1024`, same as the
/// block data limit).
///
/// Kills the full operator-swap cluster on line 188: `+:53 with *`/`with -`,
/// `*:46 with +`/`with /`, `*:59 with +`/`with /`, and `*:39 with /`/`with +`.
#[test]
fn rex_tx_data_limit() {
    assert_eq!(rex::TX_DATA_LIMIT, 12 * 1024 * 1024 + 512 * 1024);
    assert_eq!(rex::TX_DATA_LIMIT, 13_107_200);
}
