//! Common type definitions for the `MegaETH` EVM.

/// `MegaETH` transaction type used in revm.
///
/// This is the `alloy-op-evm` newtype rather than `op_revm::OpTransaction<TxEnv>` directly:
/// `alloy_evm::Evm` requires `Tx: IntoTxEnv<Self::Tx>`, and the block executor requires
/// `FromRecoveredTx<OpTxEnvelope>` / `FromTxWithEncoded<OpTxEnvelope>`. All three are foreign
/// traits on a foreign type, so they can only be implemented on this wrapper. It derefs to
/// `OpTransaction<TxEnv>`.
///
/// A `use`-rename rather than a `type` alias on purpose: a type alias carries only the type
/// namespace, so `MegaTransaction(..)` would not name the tuple-struct constructor.
pub use alloy_op_evm::OpTx as MegaTransaction;

/// Builds a [`MegaTransaction`] from a plain [`revm::context::TxEnv`], with the OP-specific
/// fields at their defaults.
///
/// This is the constructor downstream crates should reach for: it keeps the upstream layering
/// (the `alloy-op-evm` newtype wrapping `op_revm::OpTransaction`) out of their code, so upstream
/// type moves stay `mega-evm`'s problem. Callers that need `enveloped_tx` or the deposit fields
/// set them on the returned value, which derefs mutably to the OP transaction.
pub fn new_mega_transaction(base: revm::context::TxEnv) -> MegaTransaction {
    MegaTransaction(op_revm::OpTransaction::new(base))
}

/// `MegaETH` transaction type.
pub type MegaTxType = op_alloy_consensus::OpTxType;
/// `MegaETH` transaction envelope type.
pub type MegaTxEnvelope = op_alloy_consensus::OpTxEnvelope;
