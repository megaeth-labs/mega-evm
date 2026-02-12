use crate::MegaSpecId;

/// Runtime limits for a single transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvmTxRuntimeLimits {
    // ====== Limits enforced during transaction execution ======
    /// Maximum data size for a single transaction.
    pub tx_data_size_limit: u64,
    /// Maximum key-value updates for a single transaction.
    pub tx_kv_updates_limit: u64,
    /// Maximum compute gas limit for a single transaction.
    pub tx_compute_gas_limit: u64,
    /// Maximum state growth limit for a single transaction.
    pub tx_state_growth_limit: u64,
    /// Compute gas limit when accessing block environment data.
    pub block_env_access_compute_gas_limit: u64,
    /// Compute gas limit when accessing oracle data.
    pub oracle_access_compute_gas_limit: u64,
}

impl EvmTxRuntimeLimits {
    /// Creates a new `TxLimits` instance from the given `MegaSpecId`.
    pub fn from_spec(spec: MegaSpecId) -> Self {
        match spec {
            MegaSpecId::EQUIVALENCE => Self::equivalence(),
            MegaSpecId::MINI_REX => Self::mini_rex(),
            MegaSpecId::REX |
            MegaSpecId::REX1 |
            MegaSpecId::REX2 |
            MegaSpecId::REX3 |
            MegaSpecId::REX4 => Self::rex(),
        }
    }

    /// No limits.
    pub fn no_limits() -> Self {
        Self {
            tx_data_size_limit: u64::MAX,
            tx_kv_updates_limit: u64::MAX,
            tx_compute_gas_limit: u64::MAX,
            tx_state_growth_limit: u64::MAX,
            block_env_access_compute_gas_limit: u64::MAX,
            oracle_access_compute_gas_limit: u64::MAX,
        }
    }

    /// Limits for the `EQUIVALENCE` spec.
    pub fn equivalence() -> Self {
        Self::no_limits()
    }

    /// Limits for the `MINI_REX` spec.
    pub fn mini_rex() -> Self {
        Self {
            tx_data_size_limit: crate::constants::mini_rex::TX_DATA_LIMIT,
            tx_kv_updates_limit: crate::constants::mini_rex::TX_KV_UPDATE_LIMIT,
            tx_compute_gas_limit: crate::constants::mini_rex::TX_COMPUTE_GAS_LIMIT,
            block_env_access_compute_gas_limit:
                crate::constants::mini_rex::BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
            oracle_access_compute_gas_limit:
                crate::constants::mini_rex::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
            ..Self::equivalence()
        }
    }

    /// Limits for the `REX` spec.
    pub fn rex() -> Self {
        Self {
            tx_data_size_limit: crate::constants::rex::TX_DATA_LIMIT,
            tx_kv_updates_limit: crate::constants::rex::TX_KV_UPDATE_LIMIT,
            tx_compute_gas_limit: crate::constants::rex::TX_COMPUTE_GAS_LIMIT,
            tx_state_growth_limit: crate::constants::rex::TX_STATE_GROWTH_LIMIT,
            block_env_access_compute_gas_limit:
                crate::constants::mini_rex::BLOCK_ENV_ACCESS_REMAINING_COMPUTE_GAS,
            oracle_access_compute_gas_limit:
                crate::constants::mini_rex::ORACLE_ACCESS_REMAINING_COMPUTE_GAS,
        }
    }
}

impl EvmTxRuntimeLimits {
    /// Sets the maximum data size for a single transaction.
    pub fn with_tx_data_size_limit(mut self, tx_data_size_limit: u64) -> Self {
        self.tx_data_size_limit = tx_data_size_limit;
        self
    }

    /// Sets the maximum key-value updates for a single transaction.
    pub fn with_tx_kv_updates_limit(mut self, tx_kv_updates_limit: u64) -> Self {
        self.tx_kv_updates_limit = tx_kv_updates_limit;
        self
    }

    /// Sets the maximum compute gas limit for a single transaction.
    pub fn with_tx_compute_gas_limit(mut self, tx_compute_gas_limit: u64) -> Self {
        self.tx_compute_gas_limit = tx_compute_gas_limit;
        self
    }

    /// Sets the maximum state growth limit for a single transaction.
    pub fn with_tx_state_growth_limit(mut self, tx_state_growth_limit: u64) -> Self {
        self.tx_state_growth_limit = tx_state_growth_limit;
        self
    }

    /// Sets the compute gas limit when accessing block environment data.
    pub fn with_block_env_access_compute_gas_limit(
        mut self,
        block_env_access_compute_gas_limit: u64,
    ) -> Self {
        self.block_env_access_compute_gas_limit = block_env_access_compute_gas_limit;
        self
    }

    /// Sets the compute gas limit when accessing oracle data.
    pub fn with_oracle_access_compute_gas_limit(
        mut self,
        oracle_access_compute_gas_limit: u64,
    ) -> Self {
        self.oracle_access_compute_gas_limit = oracle_access_compute_gas_limit;
        self
    }
}
