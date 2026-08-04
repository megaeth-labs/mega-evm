//! Common type definitions for the `MegaETH` EVM.

/// `MegaETH` transaction type used in revm.
///
/// This is the `alloy-op-evm` newtype rather than `op_revm::OpTransaction<TxEnv>` directly:
/// `alloy_evm::Evm` requires `Tx: IntoTxEnv<Self::Tx>`, and the block executor requires
/// `FromRecoveredTx<OpTxEnvelope>` / `FromTxWithEncoded<OpTxEnvelope>`. All three are foreign
/// traits on a foreign type, so they can only be implemented on this wrapper. It derefs to
/// `OpTransaction<TxEnv>`.
pub type MegaTransaction = alloy_op_evm::OpTx;
/// `MegaETH` transaction builder type used in revm.
pub type MegaTransactionBuilder = op_revm::transaction::abstraction::OpTransactionBuilder;

/// `MegaETH` transaction type.
pub type MegaTxType = op_alloy_consensus::OpTxType;
/// `MegaETH` transaction envelope type.
pub type MegaTxEnvelope = op_alloy_consensus::OpTxEnvelope;
