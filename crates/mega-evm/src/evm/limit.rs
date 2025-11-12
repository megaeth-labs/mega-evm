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
}

impl EvmTxRuntimeLimits {
    /// Creates a new `TxLimits` instance from the given `MegaSpecId`.
    pub fn from_spec(spec: MegaSpecId) -> Self {
        match spec {
            MegaSpecId::EQUIVALENCE => Self::equivalence(),
            MegaSpecId::MINI_REX => Self::mini_rex(),
        }
    }

    /// No limits.
    pub fn no_limits() -> Self {
        Self {
            tx_data_size_limit: u64::MAX,
            tx_kv_updates_limit: u64::MAX,
            tx_compute_gas_limit: u64::MAX,
        }
    }

    /// Limits for the `EQUIVALENCE` spec.
    pub fn equivalence() -> Self {
        Self {
            tx_data_size_limit: u64::MAX,
            tx_kv_updates_limit: u64::MAX,
            tx_compute_gas_limit: u64::MAX,
        }
    }

    /// Limits for the `MINI_REX` spec.
    pub fn mini_rex() -> Self {
        Self {
            tx_data_size_limit: crate::constants::mini_rex::TX_DATA_LIMIT,
            tx_kv_updates_limit: crate::constants::mini_rex::TX_KV_UPDATE_LIMIT,
            tx_compute_gas_limit: crate::constants::mini_rex::TX_COMPUTE_GAS_LIMIT,
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
}
