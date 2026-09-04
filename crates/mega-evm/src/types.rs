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

/// Constructor for [`MegaTransaction`].
///
/// [`MegaTransaction`] is a foreign type, so it cannot carry inherent methods here; this trait
/// supplies the constructor instead — import it `as _` and call `MegaTransaction::new(base)`,
/// the same convention as revm's `MainContext`/`DefaultOp` trait constructors.
pub trait MegaTransactionNew {
    /// Builds the transaction from a plain [`revm::context::TxEnv`], with the OP-specific
    /// fields at their defaults.
    ///
    /// This is the constructor callers should reach for: it keeps the upstream layering
    /// (the `alloy-op-evm` newtype wrapping `op_revm::OpTransaction`) out of call sites, so
    /// upstream type moves stay this module's problem. Callers that need `enveloped_tx` or the
    /// deposit fields set them on the returned value, which derefs mutably to the OP transaction.
    fn new(base: revm::context::TxEnv) -> Self;
}

impl MegaTransactionNew for MegaTransaction {
    fn new(base: revm::context::TxEnv) -> Self {
        Self(op_revm::OpTransaction::new(base))
    }
}

/// `MegaETH` transaction type.
pub type MegaTxType = op_alloy_consensus::OpTxType;
/// `MegaETH` transaction envelope type.
pub type MegaTxEnvelope = op_alloy_consensus::OpTxEnvelope;
