use alloy_evm::block::{BlockExecutionError, BlockValidationError};
use alloy_primitives::TxHash;

use crate::{MegaBlockLimitExceededError, MegaTxLimitExceededError};

/// Limits for block-level resource usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockLimits {
    // ================================
    // Limits that can be checked before transaction execution.
    // ================================
    /// The maximum amount of gas for a single transaction.
    pub single_tx_gas_limit: u64,
    /// The maximum amount of gas for a block. This field is automatically set to the block env gas
    /// limit in EVM. Users should not set this field manually.
    pub(crate) block_gas_limit: u64,

    /// The maximum size of a single transaction.
    pub single_tx_size_limit: u64,
    /// The maximum size of all transactions (transaction body, not execution outcome) included in
    /// a block. The difference between this limit and the `block_da_size_limit` is that the
    /// current limit applies to the transaction size uncompressed, while the `block_da_size_limit`
    /// applies to the transaction size after DA compression.
    pub block_tx_size_limit: u64,

    /// The maximum size of a single transaction data availability size.
    pub single_tx_da_size_limit: u64,
    /// The maximum amount of data availability size allowed to generate from a block.
    pub block_da_size_limit: u64,

    // ================================
    // Limits that can be checked after transaction execution. There are only block-level limits
    // here since the transaction-level limits are enforced in the `MegaEVM` and one transaction
    // exceeding the transaction-level limits will still be included in a block.
    // ================================
    /// The maximum amount of data allowed to generate from a block.
    /// Defaults to [`crate::constants::mini_rex::BLOCK_DATA_LIMIT`].
    pub block_data_limit: u64,
    /// The maximum amount of key-value updates allowed to generate from a block.
    /// Defaults to [`crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT`].
    pub block_kv_update_limit: u64,
}

impl Default for BlockLimits {
    fn default() -> Self {
        Self {
            single_tx_gas_limit: u64::MAX,
            block_gas_limit: u64::MAX,
            single_tx_size_limit: u64::MAX,
            block_tx_size_limit: u64::MAX,
            single_tx_da_size_limit: u64::MAX,
            block_da_size_limit: u64::MAX,
            block_data_limit: crate::constants::mini_rex::BLOCK_DATA_LIMIT,
            block_kv_update_limit: crate::constants::mini_rex::BLOCK_KV_UPDATE_LIMIT,
        }
    }
}

impl BlockLimits {
    /// Set a custom single transaction gas limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction gas limit.
    pub fn with_single_tx_gas_limit(mut self, limit: u64) -> Self {
        self.single_tx_gas_limit = limit;
        self
    }

    /// Set a custom block gas limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified block gas limit.
    pub fn with_block_gas_limit(mut self, limit: u64) -> Self {
        self.block_gas_limit = limit;
        self
    }

    /// Set a custom single transaction size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction size limit.
    pub fn with_single_tx_size_limit(mut self, limit: u64) -> Self {
        self.single_tx_size_limit = limit;
        self
    }

    /// Set a custom block transaction size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified transaction size limit.
    pub fn with_block_tx_size_limit(mut self, limit: u64) -> Self {
        self.block_tx_size_limit = limit;
        self
    }

    /// Set a custom single transaction data availability size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified single transaction data availability size limit.
    pub fn with_single_tx_da_size_limit(mut self, limit: u64) -> Self {
        self.single_tx_da_size_limit = limit;
        self
    }

    /// Set a custom block data availability size limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified data availability size limit.
    pub fn with_block_da_size_limit(mut self, limit: u64) -> Self {
        self.block_da_size_limit = limit;
        self
    }

    /// Set a custom block data limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified data limit.
    pub fn with_block_data_limit(mut self, limit: u64) -> Self {
        self.block_data_limit = limit;
        self
    }

    /// Set a custom block KV update limit.
    ///
    /// This is a builder method that consumes self and returns a new instance
    /// with the specified KV update limit.
    pub fn with_block_kv_update_limit(mut self, limit: u64) -> Self {
        self.block_kv_update_limit = limit;
        self
    }

    /// Create a new block limiter using the limits.
    pub fn to_block_limiter(self) -> BlockLimiter {
        BlockLimiter {
            limits: self,
            block_gas_used: 0,
            block_data_used: 0,
            block_kv_updates_used: 0,
            block_tx_size_used: 0,
            block_da_size_used: 0,
        }
    }
}

/// A limiter for block-level resource usage.
#[derive(Debug, Clone)]
pub struct BlockLimiter {
    /// The limits.
    pub limits: BlockLimits,

    /// The amount of gas used in the block.
    pub block_gas_used: u64,
    /// The amount of transaction size used in the block.
    pub block_tx_size_used: u64,
    /// The amount of data availability size used in the block.
    pub block_da_size_used: u64,
    /// The amount of data used in the block.
    pub block_data_used: u64,
    /// The amount of key-value updates used in the block.
    pub block_kv_updates_used: u64,
}

impl BlockLimiter {
    /// Create a new block limiter using the limits.
    pub fn new(limits: BlockLimits) -> Self {
        Self {
            limits,
            block_gas_used: 0,
            block_data_used: 0,
            block_kv_updates_used: 0,
            block_tx_size_used: 0,
            block_da_size_used: 0,
        }
    }

    /// Check the limits before transaction execution.
    ///
    /// This method checks the limits before transaction execution.
    ///
    /// # Parameters
    ///
    /// - `tx_hash`: The hash of the transaction.
    /// - `gas_limit`: The gas limit of the transaction.
    /// - `tx_size`: The size of the transaction.
    /// - `da_size`: The data availability size of the transaction.
    ///
    /// # Returns
    ///
    /// Returns `Ok` if the limits are within the limits, otherwise returns an error.
    ///
    /// # Errors
    ///
    /// Returns an [`BlockValidationError::InvalidTx`] if the limits are exceeded. The transaction
    /// may exceed either the transaction-level limits [`MegaTxLimitExceededError`] or the
    /// block-level limits [`MegaBlockLimitExceededError`].
    pub fn pre_execution_check(
        &self,
        tx_hash: TxHash,
        gas_limit: u64,
        tx_size: u64,
        da_size: u64,
    ) -> Result<(), BlockExecutionError> {
        // Check single transaction gas limit
        if gas_limit > self.limits.single_tx_gas_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaTxLimitExceededError::TransactionGasLimit {
                    tx_gas_limit: gas_limit,
                    limit: self.limits.single_tx_gas_limit,
                }),
            }));
        }

        // Check block gas limit
        if self.block_gas_used + gas_limit > self.limits.block_gas_limit {
            return Err(BlockExecutionError::Validation(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: gas_limit,
                    block_available_gas: self.limits.block_gas_limit - self.block_gas_used,
                },
            ));
        }

        // Check single transaction size limit
        if tx_size > self.limits.single_tx_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaTxLimitExceededError::TransactionSizeLimit {
                    tx_size,
                    limit: self.limits.single_tx_size_limit,
                }),
            }));
        }

        // Check block transaction size limit
        if tx_size + self.block_tx_size_used > self.limits.block_tx_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::TransactionSizeLimit {
                    block_used: self.block_tx_size_used,
                    tx_used: tx_size,
                    limit: self.limits.block_tx_size_limit,
                }),
            }));
        }

        // Check single transaction data availability size limit
        if da_size > self.limits.single_tx_da_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaTxLimitExceededError::DataAvailabilitySizeLimit {
                    da_size,
                    limit: self.limits.single_tx_da_size_limit,
                }),
            }));
        }

        // Check block data availability size limit
        if da_size + self.block_da_size_used > self.limits.block_da_size_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::DataAvailabilitySizeLimit {
                    block_used: self.block_da_size_used,
                    tx_used: da_size,
                    limit: self.limits.block_da_size_limit,
                }),
            }));
        }

        Ok(())
    }

    /// Check the limits after transaction execution.
    ///
    /// This method checks the limits after transaction execution.
    ///
    /// # Parameters
    ///
    /// - `tx_hash`: The hash of the transaction.
    /// - `tx_size`: The size of the transaction.
    /// - `da_size`: The data availability size of the transaction.
    ///
    /// # Returns
    ///
    /// Returns `Ok` if the limits are within the limits, otherwise returns an error.
    ///
    /// # Errors
    ///
    /// Returns an [`BlockValidationError::InvalidTx`] if the limits are exceeded. The transaction
    /// may exceed the block-level limits [`MegaBlockLimitExceededError`].
    pub fn post_execution_check(
        &mut self,
        tx_hash: TxHash,
        gas_used: u64,
        tx_size_used: u64,
        da_size_used: u64,
        data_size_used: u64,
        kv_updates_used: u64,
    ) -> Result<(), BlockExecutionError> {
        // Block gas limit. No need to check here since it's checked before transaction execution.
        self.block_gas_used += gas_used;

        // Block tx size limit, no need to check here since it's checked before transaction
        // execution.
        self.block_tx_size_used += tx_size_used;

        // Block da size limit, no need to check here since it's checked before transaction
        // execution.
        self.block_da_size_used += da_size_used;

        // Block data limit
        if self.block_data_used + data_size_used > self.limits.block_data_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::DataLimit {
                    block_used: self.block_data_used,
                    tx_used: data_size_used,
                    limit: self.limits.block_data_limit,
                }),
            }));
        }
        self.block_data_used += data_size_used;

        // Block kv updates limit
        if self.block_kv_updates_used + kv_updates_used > self.limits.block_kv_update_limit {
            return Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                hash: tx_hash,
                error: Box::new(MegaBlockLimitExceededError::KVUpdateLimit {
                    block_used: self.block_kv_updates_used,
                    tx_used: kv_updates_used,
                    limit: self.limits.block_kv_update_limit,
                }),
            }));
        }
        self.block_kv_updates_used += kv_updates_used;

        Ok(())
    }
}
