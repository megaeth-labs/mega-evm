//! Common type definitions of the Satin engine.
//!
//! The result and error types are aliases of the OP ones for now; the common execution layer
//! decides the result and gas types the engine exposes when it lands.

/// `MegaETH` transaction as the EVM executes it.
///
/// It is alloy-op-evm's wrapper of [`op_revm::OpTransaction`], which carries the conversions
/// from signed transactions that the node's block executor needs.
pub type MegaTransaction = alloy_op_evm::OpTx;

/// Why a transaction halted.
pub type MegaHaltReason = op_revm::OpHaltReason;

/// Transaction validation error, as the alloy-evm interface reports it.
pub type MegaTransactionError = alloy_op_evm::OpTxError;

/// `MegaETH` transaction type.
pub type MegaTxType = op_alloy_consensus::OpTxType;

/// `MegaETH` transaction envelope type.
pub type MegaTxEnvelope = op_alloy_consensus::OpTxEnvelope;
