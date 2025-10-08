use crate::{BlockEnvAccess, OracleAccessTracker};

/// A tracker for sensitive data access.
#[derive(Debug, Clone, Default)]
pub struct SensitiveDataAccessTracker {
    /// Bitmap of block environment data accessed during transaction execution.
    block_env_accessed: BlockEnvAccess,
    /// Whether beneficiary data has been accessed in current transaction.
    beneficiary_balance_accessed: bool,
    /// Tracker for oracle contract access.
    oracle_tracker: OracleAccessTracker,
    /// Amount of gas artificially consumed for enforcement purposes.
    /// This tracks the gas we "spent" to limit execution, which should be reimbursed to the user.
    enforcement_gas_consumed: u64,
}

impl SensitiveDataAccessTracker {
    /// Creates a new tracker with no accesses recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks if any sensitive data has been accessed.
    /// If so, the remaining gas in all message calls will be limited to a small amount of gas,
    /// forcing the transaction to finish execution soon.
    pub fn accessed(&self) -> bool {
        !self.block_env_accessed.is_empty() ||
            self.beneficiary_balance_accessed ||
            self.oracle_tracker.has_accessed()
    }

    /// Returns the bitmap of block environment data accessed during transaction execution.
    pub fn get_block_env_accesses(&self) -> BlockEnvAccess {
        self.block_env_accessed
    }

    /// Marks that a specific type of block environment has been accessed.
    pub fn mark_block_env_accessed(&mut self, access_type: BlockEnvAccess) {
        self.block_env_accessed.insert(access_type);
    }

    /// Checks if beneficiary balance has been accessed.
    pub fn has_accessed_beneficiary_balance(&self) -> bool {
        self.beneficiary_balance_accessed
    }

    /// Marks that beneficiary balance has been accessed.
    pub fn mark_beneficiary_balance_accessed(&mut self) {
        self.beneficiary_balance_accessed = true;
    }

    /// Checks if the oracle contract has been accessed.
    pub fn has_accessed_oracle(&self) -> bool {
        self.oracle_tracker.has_accessed()
    }

    /// Checks if the given address is the oracle contract address and marks it as accessed.
    pub fn check_and_mark_oracle_access(&mut self, address: &alloy_primitives::Address) -> bool {
        self.oracle_tracker.check_and_mark_oracle_access(address)
    }

    /// Returns the amount of gas artificially consumed for enforcement purposes.
    pub fn enforcement_gas_consumed(&self) -> u64 {
        self.enforcement_gas_consumed
    }

    /// Records additional enforcement gas consumed.
    pub fn record_enforcement_gas(&mut self, gas: u64) {
        self.enforcement_gas_consumed = self.enforcement_gas_consumed.saturating_add(gas);
    }

    /// Resets all access tracking for a new transaction.
    pub fn reset(&mut self) {
        self.block_env_accessed = BlockEnvAccess::empty();
        self.beneficiary_balance_accessed = false;
        self.oracle_tracker.reset();
        self.enforcement_gas_consumed = 0;
    }
}
