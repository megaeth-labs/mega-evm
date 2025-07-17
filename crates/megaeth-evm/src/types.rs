use revm::{
    context::TxEnv, handler::instructions::EthInstructions,
    interpreter::interpreter::EthInterpreter,
};

use crate::MegaethContext;

/// `MegaETH` halt reason type.
pub type MegaethHaltReason = op_revm::OpHaltReason;

/// `MegaETH` EVM execution transaction error type.
pub type MegaethTransactionError = op_revm::OpTransactionError;

/// `MegaETH` transaction type used in revm.
pub type MegaethTransaction = op_revm::OpTransaction<TxEnv>;

/// `MegaETH` precompiles type.
pub type MegaethPrecompiles = op_revm::precompiles::OpPrecompiles;

/// `MegaETH` instructions type.
pub type MegaethInstructions<DB> = EthInstructions<EthInterpreter, MegaethContext<DB>>;

/// `MegaETH` transaction type.
pub type MegaethTxType = op_alloy_consensus::OpTxType;
