//! Common type definitions for the `MegaETH` EVM.

use bitflags::bitflags;
use op_revm::transaction::deposit::DEPOSIT_TRANSACTION_TYPE;
use revm::context::{Transaction, TxEnv};
use serde::{Deserialize, Serialize};

use crate::constants::MEGA_SYSTEM_ADDRESS;

/// `MegaETH` transaction type used in revm.
pub type MegaTransaction = op_revm::OpTransaction<TxEnv>;
/// `MegaETH` transaction builder type used in revm.
pub type MegaTransactionBuilder = op_revm::transaction::abstraction::OpTransactionBuilder;

/// `MegaETH` precompiles type.
pub type MegaPrecompiles = op_revm::precompiles::OpPrecompiles;

/// `MegaETH` transaction type.
pub type MegaTxType = op_alloy_consensus::OpTxType;

bitflags! {
    /// Bitmap-based tracking of block environment accesses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct BlockEnvAccess: u16 {
        /// Block number (NUMBER opcode)
        const BLOCK_NUMBER = 1 << 0;
        /// Block timestamp (TIMESTAMP opcode)
        const TIMESTAMP = 1 << 1;
        /// Block coinbase/beneficiary (COINBASE opcode)
        const COINBASE = 1 << 2;
        /// Block difficulty (DIFFICULTY opcode)
        const DIFFICULTY = 1 << 3;
        /// Block gas limit (GASLIMIT opcode)
        const GAS_LIMIT = 1 << 4;
        /// Base fee per gas (BASEFEE opcode)
        const BASE_FEE = 1 << 5;
        /// Previous block randomness (PREVRANDAO opcode)
        const PREV_RANDAO = 1 << 6;
        /// Block hash lookup (BLOCKHASH opcode)
        const BLOCK_HASH = 1 << 7;
        /// Blob base fee per gas (BLOBBASEFEE opcode)
        const BLOB_BASE_FEE = 1 << 8;
        /// Blob hash lookup (BLOBHASH opcode)
        const BLOB_HASH = 1 << 9;
    }
}

impl Default for BlockEnvAccess {
    fn default() -> Self {
        Self::empty()
    }
}

impl BlockEnvAccess {
    /// Counts the number of accessed block environment types.
    pub fn count_accessed(self) -> usize {
        self.bits().count_ones() as usize
    }

    /// Gets the raw bitmap value.
    pub const fn raw(self) -> u16 {
        self.bits()
    }
}

/// Checks if a transaction is from the `MegaETH` system address.
///
/// Transactions from the mega system address are processed as deposit-like transactions,
/// bypassing signature validation, nonce verification, and fee deduction.
/// This is distinct from op system transactions.
///
/// # Arguments
///
/// * `tx` - The transaction to check
///
/// # Returns
///
/// Returns `true` if the transaction is from the mega system address, `false` otherwise.
pub fn is_mega_system_address_transaction(tx: &MegaTransaction) -> bool {
    tx.base.caller == MEGA_SYSTEM_ADDRESS
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
    is_mega_system_address_transaction(tx)
}
