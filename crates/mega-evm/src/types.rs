//! Common type definitions for the `MegaETH` EVM.

use revm::context::TxEnv;

/// `MegaETH` transaction type used in revm.
pub type MegaTransaction = op_revm::OpTransaction<TxEnv>;
/// `MegaETH` transaction builder type used in revm.
pub type MegaTransactionBuilder = op_revm::transaction::abstraction::OpTransactionBuilder;

/// `MegaETH` precompiles type.
pub type MegaPrecompiles = op_revm::precompiles::OpPrecompiles;

/// `MegaETH` transaction type.
pub type MegaTxType = op_alloy_consensus::OpTxType;
