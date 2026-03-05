//! Common type definitions for the `MegaETH` EVM.

use revm::context::TxEnv;

/// SALT bucket identifier.
///
/// Accounts and storage slots are mapped to buckets, which have dynamic capacities
/// that affect gas costs.
pub type BucketId = u32;

/// `MegaETH` transaction type used in revm.
pub type MegaTransaction = op_revm::OpTransaction<TxEnv>;
/// `MegaETH` transaction builder type used in revm.
pub type MegaTransactionBuilder = op_revm::transaction::abstraction::OpTransactionBuilder;

/// `MegaETH` transaction type.
pub type MegaTxType = op_alloy_consensus::OpTxType;
/// `MegaETH` transaction envelope type.
pub type MegaTxEnvelope = op_alloy_consensus::OpTxEnvelope;
