//! Literal value-pinning tests for every consensus `pub const` in
//! `src/constants.rs`.
//!
//! Expected values are hard-coded decimal (or size) literals only.
//! They must never be derived from the same constants under test, so a
//! `gas_const` mutator that rewrites a RHS by ±1 (or swaps arithmetic
//! operators in a compound expression) is killed regardless of whether
//! behavioral tests happen to re-import the same constant into their
//! expected side.
//!
//! Covers every module-level and nested `pub const` declared in
//! `constants.rs` (not re-exports of upstream revm gas symbols).

use mega_evm::constants::{mini_rex, rex, rex2, rex3, rex4, rex5, PRE_REX5_SYSTEM_CALL_GAS_LIMIT};

// =============================================================================
// Top-level
// =============================================================================

#[test]
fn test_pre_rex5_system_call_gas_limit_literal() {
    assert_eq!(PRE_REX5_SYSTEM_CALL_GAS_LIMIT, 30_000_000);
}

// =============================================================================
// mini_rex
// =============================================================================

#[test]
fn test_mini_rex_max_contract_size_literal() {
    assert_eq!(mini_rex::MAX_CONTRACT_SIZE, 524_288);
}

#[test]
fn test_mini_rex_additional_initcode_size() {
    assert_eq!(mini_rex::ADDITIONAL_INITCODE_SIZE, 24_576);
}

#[test]
fn test_mini_rex_max_initcode_size() {
    assert_eq!(mini_rex::MAX_INITCODE_SIZE, 548_864);
}

#[test]
fn test_mini_rex_tx_compute_gas_limit_literal() {
    assert_eq!(mini_rex::TX_COMPUTE_GAS_LIMIT, 1_000_000_000);
}

#[test]
fn test_mini_rex_sstore_set_storage_gas_literal() {
    assert_eq!(mini_rex::SSTORE_SET_STORAGE_GAS, 2_000_000);
}

#[test]
fn test_mini_rex_new_account_storage_gas_literal() {
    assert_eq!(mini_rex::NEW_ACCOUNT_STORAGE_GAS, 2_000_000);
}

#[test]
fn test_mini_rex_codedeposit_storage_gas_literal() {
    assert_eq!(mini_rex::CODEDEPOSIT_STORAGE_GAS, 10_000);
}

#[test]
fn test_mini_rex_log_data_storage_gas_literal() {
    assert_eq!(mini_rex::LOG_DATA_STORAGE_GAS, 80);
}

#[test]
fn test_mini_rex_log_topic_storage_gas_literal() {
    assert_eq!(mini_rex::LOG_TOPIC_STORAGE_GAS, 3_750);
}

#[test]
fn test_mini_rex_calldata_standard_token_storage_gas_literal() {
    assert_eq!(mini_rex::CALLDATA_STANDARD_TOKEN_STORAGE_GAS, 40);
}

#[test]
fn test_mini_rex_calldata_standard_token_storage_floor_gas_literal() {
    assert_eq!(mini_rex::CALLDATA_STANDARD_TOKEN_STORAGE_FLOOR_GAS, 100);
}

#[test]
fn test_mini_rex_block_data_limit() {
    assert_eq!(mini_rex::BLOCK_DATA_LIMIT, 13_107_200);
}

#[test]
fn test_mini_rex_tx_data_limit() {
    assert_eq!(mini_rex::TX_DATA_LIMIT, 3_276_800);
}

#[test]
fn test_mini_rex_block_kv_update_limit_literal() {
    assert_eq!(mini_rex::BLOCK_KV_UPDATE_LIMIT, 500_000);
}

#[test]
fn test_mini_rex_tx_kv_update_limit() {
    assert_eq!(mini_rex::TX_KV_UPDATE_LIMIT, 125_000);
}

/// Kills `gas_const:mini_rex::BLOCK_ENV_ACCESS_COMPUTE_GAS` ±1.
///
/// Behavioral suites often assert against the same constant (or against the
/// equal Rex3 oracle cap), so only a literal pin catches the +1 polarity.
#[test]
fn test_mini_rex_block_env_access_compute_gas_literal() {
    assert_eq!(mini_rex::BLOCK_ENV_ACCESS_COMPUTE_GAS, 20_000_000);
}

/// Kills `gas_const:mini_rex::ORACLE_ACCESS_COMPUTE_GAS` ±1.
///
/// Existing oracle/detention tests assert
/// `compute_gas_limit == ORACLE_ACCESS_COMPUTE_GAS`, which is tautological
/// under constant mutation.
#[test]
fn test_mini_rex_oracle_access_compute_gas_literal() {
    assert_eq!(mini_rex::ORACLE_ACCESS_COMPUTE_GAS, 1_000_000);
}

// =============================================================================
// rex2
// =============================================================================

#[test]
fn test_rex2_keyless_deploy_overhead_gas_literal() {
    assert_eq!(rex2::KEYLESS_DEPLOY_OVERHEAD_GAS, 100_000);
}

// =============================================================================
// rex3
// =============================================================================

#[test]
fn test_rex3_oracle_access_compute_gas_literal() {
    assert_eq!(rex3::ORACLE_ACCESS_COMPUTE_GAS, 20_000_000);
}

// =============================================================================
// rex4
// =============================================================================

#[test]
fn test_rex4_frame_limit_numerator_literal() {
    assert_eq!(rex4::FRAME_LIMIT_NUMERATOR, 98);
}

#[test]
fn test_rex4_frame_limit_denominator_literal() {
    assert_eq!(rex4::FRAME_LIMIT_DENOMINATOR, 100);
}

#[test]
fn test_rex4_storage_call_stipend_literal() {
    assert_eq!(rex4::STORAGE_CALL_STIPEND, 23_000);
}

// =============================================================================
// rex5
// =============================================================================

#[test]
fn test_rex5_system_call_gas_limit_floor_literal() {
    assert_eq!(rex5::SYSTEM_CALL_GAS_LIMIT_FLOOR, 30_000_000);
}

// =============================================================================
// rex
// =============================================================================

#[test]
fn test_rex_tx_intrinsic_storage_gas_literal() {
    assert_eq!(rex::TX_INTRINSIC_STORAGE_GAS, 39_000);
}

#[test]
fn test_rex_sstore_set_storage_gas_base_literal() {
    assert_eq!(rex::SSTORE_SET_STORAGE_GAS_BASE, 20_000);
}

#[test]
fn test_rex_new_account_storage_gas_base_literal() {
    assert_eq!(rex::NEW_ACCOUNT_STORAGE_GAS_BASE, 25_000);
}

#[test]
fn test_rex_contract_creation_storage_gas_base_literal() {
    assert_eq!(rex::CONTRACT_CREATION_STORAGE_GAS_BASE, 32_000);
}

#[test]
fn test_rex_tx_compute_gas_limit_literal() {
    assert_eq!(rex::TX_COMPUTE_GAS_LIMIT, 200_000_000);
}

#[test]
fn test_rex_tx_data_limit() {
    assert_eq!(rex::TX_DATA_LIMIT, 13_107_200);
}

#[test]
fn test_rex_tx_kv_update_limit_literal() {
    assert_eq!(rex::TX_KV_UPDATE_LIMIT, 500_000);
}

#[test]
fn test_rex_tx_state_growth_limit_literal() {
    assert_eq!(rex::TX_STATE_GROWTH_LIMIT, 1_000);
}

#[test]
fn test_rex_block_state_growth_limit_literal() {
    assert_eq!(rex::BLOCK_STATE_GROWTH_LIMIT, 1_000);
}
