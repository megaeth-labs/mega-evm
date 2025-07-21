use revm::context::TxEnv;

/// `MegaETH` halt reason type.
pub type HaltReason = op_revm::OpHaltReason;

/// `MegaETH` EVM execution transaction error type.
pub type TransactionError = op_revm::OpTransactionError;

/// `MegaETH` transaction type used in revm.
pub type Transaction = op_revm::OpTransaction<TxEnv>;

/// `MegaETH` precompiles type.
pub type Precompiles = op_revm::precompiles::OpPrecompiles;

/// `MegaETH` transaction type.
pub type TxType = op_alloy_consensus::OpTxType;
