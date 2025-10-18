//! System transaction for the `MegaETH` EVM.
//!
//! This module contains constants, types, and utilities related to mega system transactions.

use alloy_primitives::{address, b256, Address, TxKind, B256};
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::context::Transaction;

use crate::types::MegaTransaction;

/// The `MegaETH` system address for deposit-like transaction processing.
/// Normal transactions sent from this address are processed as deposit transactions,
/// bypassing signature validation, nonce verification, and fee deduction.
///
/// The mega system address is held by the sequencer and can only call whitelisted addresses.
pub const MEGA_SYSTEM_ADDRESS: Address = address!("0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d");

/// The whitelist of addresses that are allowed to be called by the `MegaETH` system address.
pub const MEGA_SYSTEM_TX_WHITELIST: &[Address] = &[
    // High-precision timestamp oracle pre-deployed contract
    address!("0x4200000000000000000000000000000000000101"),
];

/// The source hash of the `MegaETH` system transaction, used to set the `source_hash` field of the
/// op deposit info. The value is `keccak256("MEGA_SYSTEM_TRANSACTION")`.
pub const MEGA_SYSTEM_TRANSACTION_SOURCE_HASH: B256 =
    b256!("852c082c0faff590c6300c2c34815d1f79882552fa95ba413cd5aeb1dba84957");

/// Checks if a transaction is sent from the `MEGA_SYSTEM_ADDRESS`.
pub fn sent_from_mega_system_address(tx: &MegaTransaction) -> bool {
    tx.caller() == MEGA_SYSTEM_ADDRESS
}

/// Checks if a transaction is a mega system transaction.
/// A mega system transaction is a legacy transaction that is submitted by the `MEGA_SYSTEM_ADDRESS`
/// and calls a whitelisted address in `MEGA_SYSTEM_TX_WHITELIST`.
pub fn is_mega_system_transaction(tx: &MegaTransaction) -> bool {
    if tx.tx_type() == 0x0 && sent_from_mega_system_address(tx) {
        // a mega system transaction must be a legacy transaction
        match tx.kind() {
            TxKind::Create => false,
            TxKind::Call(address) => MEGA_SYSTEM_TX_WHITELIST.contains(&address),
        }
    } else {
        false
    }
}

/// Checks if a transaction should be processed as a deposit-like transaction.
///
/// This includes both actual deposit transactions (`DEPOSIT_TRANSACTION_TYPE`) and normal
/// transactions from the `MegaETH` system address (mega system transactions).
///
/// # Arguments
///
/// * `tx` - The transaction to check
///
/// # Returns
///
/// Returns `true` if the transaction should be processed as deposit-like, `false` otherwise.
pub fn is_deposit_like_transaction(tx: &MegaTransaction) -> bool {
    // Check if it's an actual deposit transaction
    if tx.tx_type() == DEPOSIT_TRANSACTION_TYPE {
        return true;
    }

    // Check if it's from the mega system address
    is_mega_system_transaction(tx)
}

/// The multiplier for the gas stipend of the deposit transaction. If an Op Stack deposit tx calls a
/// whitelisted address, its gas limit will be automatically multiplied by this multiplier as
/// additional gas stipend.
pub const DEPOSIT_TX_GAS_STIPEND_MULTIPLIER: u64 = 100;

/// The whitelist of callee addresses of Op Stack deposit txs inducing additional gas stipend.
pub const DEPOSIT_TX_GAS_STIPEND_WHITELIST: &[Address] = &[
    // L1Block pre-deployed contract
    address!("0x4200000000000000000000000000000000000015"),
    // GasPriaceOracle pre-deployed contract
    address!("0x420000000000000000000000000000000000000F"),
    // OperatorFeeVault pre-deployed contract
    address!("0x420000000000000000000000000000000000001b"),
];
