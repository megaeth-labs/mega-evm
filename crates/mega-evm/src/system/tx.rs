//! System transaction for the `MegaETH` EVM.
//!
//! The mega system transaction is a special protocol-maintenance transaction with minimal
//! side effects:
//! - no transaction fee (no L2 gas fee, no L1 data fee, no base fee, etc.), thus no state change to
//!   the block beneficiary or any fee vaults.
//! - system address's nonce still bumps as normal transactions
//!
//! This module contains constants, types, and utilities related to mega system transactions.

use alloy_primitives::{address, b256, Address, TxKind, B256};
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::context::Transaction;

use crate::{types::MegaTransaction, ORACLE_CONTRACT_ADDRESS};

/// The `MegaETH` system address for deposit-like transaction processing.
/// Normal transactions sent from this address are processed as deposit transactions,
/// bypassing signature validation, nonce verification, and fee deduction.
///
/// It can only call whitelisted addresses.
pub const MEGA_SYSTEM_ADDRESS: Address = address!("0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d");

/// The whitelist of addresses that are allowed to be called by the `MegaETH` system address.
pub const MEGA_SYSTEM_TX_WHITELIST: &[Address] = &[ORACLE_CONTRACT_ADDRESS];

/// The source hash of the `MegaETH` system transaction, used to set the `source_hash` field of the
/// op deposit info. The value is `keccak256("MEGA_SYSTEM_TRANSACTION")`.
pub const MEGA_SYSTEM_TRANSACTION_SOURCE_HASH: B256 =
    b256!("852c082c0faff590c6300c2c34815d1f79882552fa95ba413cd5aeb1dba84957");

/// The source hash of the `MegaETH` keyless-deploy sandbox transaction. Used to set the
/// `source_hash` field of the op deposit info so the sandbox tx is treated as a deposit-like
/// transaction (bypasses L1/operator fee, validation, fee distribution). The value is
/// `keccak256("MEGA_SANDBOX_TRANSACTION")`.
pub const SANDBOX_TX_SOURCE_HASH: B256 =
    b256!("3bc757a39ecb6dc5b5e5715352016c2b6fd38968a78cc57e878d9be249bc62f4");

/// Checks if a transaction is sent from the given system address.
pub fn sent_from_system_address(tx: &MegaTransaction, system_address: Address) -> bool {
    tx.caller() == system_address
}

/// Checks if a transaction is a mega system transaction using the given system address.
/// A mega system transaction is a legacy transaction that is submitted by the system address
/// and calls a whitelisted address in `MEGA_SYSTEM_TX_WHITELIST`.
pub fn is_mega_system_transaction_with(tx: &MegaTransaction, system_address: Address) -> bool {
    check_if_mega_system_transaction(tx.caller(), tx.tx_type(), tx.kind(), system_address)
}

/// Checks if a transaction is a mega system transaction.
///
/// # Arguments
///
/// * `tx_signer` - The signer of the transaction
/// * `tx_type` - The type of the transaction
/// * `tx_kind` - The kind of the transaction
/// * `system_address` - The current system address for this block
///
/// # Returns
///
/// Returns `true` if the transaction is a mega system transaction, `false` otherwise.
pub fn check_if_mega_system_transaction(
    tx_signer: Address,
    tx_type: u8,
    tx_kind: TxKind,
    system_address: Address,
) -> bool {
    if tx_type == 0x0 && tx_signer == system_address {
        // a mega system transaction must be a legacy transaction
        match tx_kind {
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
pub fn is_deposit_like_transaction(tx: &MegaTransaction, system_address: Address) -> bool {
    // Check if it's an actual deposit transaction
    if tx.tx_type() == DEPOSIT_TRANSACTION_TYPE {
        return true;
    }

    // Check if it's from the mega system address
    is_mega_system_transaction_with(tx, system_address)
}

/// Checks if a transaction is *system-originated*: produced by the protocol itself or by the
/// sequencer acting as the system address, as opposed to any user transaction.
///
/// A transaction is system-originated when either:
/// - its caller is the EIP-2935 / EIP-4788 system address (`0xff..fe`), which is how mega-evm's own
///   pre-block system calls (block-hash, beacon-root, `SequencerRegistry`) are issued; or
/// - it is a mega system transaction: a legacy transaction from `system_address` calling a
///   whitelisted contract (see [`is_mega_system_transaction_with`]).
///
/// Neither branch can be produced by a user: `0xff..fe` has no key and is only ever set as
/// caller by the protocol's own pre-block helpers, and matching `system_address` requires the
/// protocol's deposit-injection path. In particular this deliberately does **not** match an
/// arbitrary user deposit transaction (`DEPOSIT_TRANSACTION_TYPE`), unlike
/// [`is_deposit_like_transaction`].
///
/// REX6+ uses this to exempt system-originated execution from `MegaETH`'s per-transaction resource
/// metering (SALT-scaled storage gas, the four `AdditionalLimit` dimensions, and gas detention),
/// so protocol-mandated state changes can never fail due to metering — e.g. when SALT buckets
/// grow. The standard EVM `gas_limit` still bounds the work as a runaway guard.
///
/// The exemption is evaluated in `MegaContext::on_new_tx`, which runs *after* `before_run` has
/// already deposit-promoted a mega system tx (stamping [`MEGA_SYSTEM_TRANSACTION_SOURCE_HASH`] and
/// flipping `tx_type()` to a deposit). The source-hash branch below therefore carries the match for
/// promoted txs; the `is_mega_system_transaction_with` branch covers the pre-promotion shape for
/// any other caller. Both the source hash and the system-address+whitelist gate are protocol-set,
/// so a user deposit (different source hash, non-system caller) is never matched — unlike
/// [`is_deposit_like_transaction`].
pub fn is_system_originated(tx: &MegaTransaction, system_address: Address) -> bool {
    // Internal pre-block system calls (EIP-2935 / EIP-4788 / SequencerRegistry) use `0xff..fe` and
    // run via `run_system_call`, so they are never deposit-promoted.
    tx.caller() == alloy_eips::eip4788::SYSTEM_ADDRESS ||
        // A mega system tx that `before_run` has already promoted to a deposit.
        tx.deposit.source_hash == MEGA_SYSTEM_TRANSACTION_SOURCE_HASH ||
        // A mega system tx in its original (pre-promotion) legacy shape.
        is_mega_system_transaction_with(tx, system_address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::TxKind;

    const EIP_SYSTEM_ADDRESS: Address = alloy_eips::eip4788::SYSTEM_ADDRESS;
    const USER: Address = address!("0000000000000000000000000000000000009999");
    const NON_WHITELIST: Address = address!("00000000000000000000000000000000000000ab");

    /// Builds a legacy (type 0x0) call transaction with the given caller and callee.
    fn legacy_call_tx(caller: Address, to: Address) -> MegaTransaction {
        let mut tx = MegaTransaction::default();
        tx.base.tx_type = 0;
        tx.base.caller = caller;
        tx.base.kind = TxKind::Call(to);
        tx
    }

    #[test]
    fn test_is_system_originated_matches_eip_system_address_caller() {
        // The protocol's own pre-block system calls (EIP-2935 / EIP-4788 / SequencerRegistry)
        // use `0xff..fe` as caller; it matches regardless of the resolved system address or target.
        let tx = legacy_call_tx(EIP_SYSTEM_ADDRESS, USER);
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
        assert!(is_system_originated(&tx, USER));
    }

    #[test]
    fn test_is_system_originated_matches_mega_system_tx() {
        // Sequencer mega system tx: legacy tx from the system address to a whitelisted contract.
        let tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    #[test]
    fn test_is_system_originated_rejects_system_caller_to_non_whitelist() {
        // System address calling a non-whitelisted contract is not a mega system tx.
        let tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, NON_WHITELIST);
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    #[test]
    fn test_is_system_originated_matches_promoted_mega_system_tx() {
        // `before_run` promotes a mega system tx to a deposit (stamping the source hash and
        // flipping `tx_type()`) *before* `on_new_tx` evaluates the exemption. The promoted shape
        // must still be recognized as system-originated.
        let mut tx = legacy_call_tx(MEGA_SYSTEM_ADDRESS, ORACLE_CONTRACT_ADDRESS);
        tx.deposit.source_hash = MEGA_SYSTEM_TRANSACTION_SOURCE_HASH;
        assert_eq!(tx.tx_type(), DEPOSIT_TRANSACTION_TYPE, "promotion flips tx_type to deposit");
        assert!(
            !is_mega_system_transaction_with(&tx, MEGA_SYSTEM_ADDRESS),
            "the legacy-typed check no longer matches after promotion",
        );
        assert!(is_system_originated(&tx, MEGA_SYSTEM_ADDRESS), "but the source-hash branch does");
    }

    #[test]
    fn test_is_system_originated_rejects_user_tx() {
        let tx = legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS);
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }

    #[test]
    fn test_is_system_originated_rejects_user_deposit_tx() {
        // Anti-bypass: a user deposit transaction (non-system caller) must NOT be treated as
        // system-originated, even though `is_deposit_like_transaction` classifies any deposit as
        // deposit-like. Otherwise a user could craft a deposit to escape SALT-scaled storage gas.
        let mut tx = legacy_call_tx(USER, ORACLE_CONTRACT_ADDRESS);
        // A non-zero deposit source hash makes `tx_type()` report `DEPOSIT_TRANSACTION_TYPE`.
        tx.deposit.source_hash = B256::repeat_byte(0x11);
        assert_eq!(tx.tx_type(), DEPOSIT_TRANSACTION_TYPE);
        assert!(is_deposit_like_transaction(&tx, MEGA_SYSTEM_ADDRESS));
        assert!(!is_system_originated(&tx, MEGA_SYSTEM_ADDRESS));
    }
}
