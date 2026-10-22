//! Common type definitions of the Satin engine.
//!
//! These stay aliases of the OP types, because Satin adds nothing to them:
//!
//! - [`MegaTransaction`], [`MegaTxType`] and [`MegaTxEnvelope`]: Satin executes OP transactions.
//! - [`MegaHaltReason`]: Satin halts only where the EVM does. A resource limit stops a transaction
//!   with a revert whose output is [`MegaLimitExceeded`](crate::MegaLimitExceeded), not with a
//!   halt, so the halt set is op-revm's.
//! - [`MegaTransactionError`]: Satin validates transactions as op-revm does.
//!
//! What Satin does add has types of its own: the gas a transaction spent by ledger
//! ([`MegaGasUsage`](crate::MegaGasUsage)), the limit verdict and revert data
//! ([`LimitCheck`](crate::LimitCheck), [`MegaLimitExceeded`](crate::MegaLimitExceeded)) and the
//! block's counters ([`BlockGasCounters`](crate::BlockGasCounters)).

/// `MegaETH` transaction as the EVM executes it.
///
/// It is alloy-op-evm's wrapper of [`op_revm::OpTransaction`], which carries the conversions
/// from signed transactions that the node's block executor needs.
pub type MegaTransaction = alloy_op_evm::OpTx;

/// Why a transaction halted: an EVM halt, or a failed deposit.
///
/// A resource limit does not halt a transaction; see the module documentation.
pub type MegaHaltReason = op_revm::OpHaltReason;

/// Transaction validation error, as the alloy-evm interface reports it.
pub type MegaTransactionError = alloy_op_evm::OpTxError;

/// `MegaETH` transaction type.
pub type MegaTxType = op_alloy_consensus::OpTxType;

/// `MegaETH` transaction envelope type.
pub type MegaTxEnvelope = op_alloy_consensus::OpTxEnvelope;
