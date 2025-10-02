use crate::BlockEnvAccess;

/// A tracker for sensitive data access.
#[derive(Debug, Clone)]
pub struct SensitiveDataAccessTracker {
    /// Bitmap of block environment data accessed during transaction execution.
    block_env_accessed: BlockEnvAccess,
    /// Whether beneficiary data has been accessed in current transaction.
    beneficiary_balance_accessed: bool,
}

impl Default for SensitiveDataAccessTracker {
    fn default() -> Self {
        Self {
            block_env_accessed: BlockEnvAccess::empty(),
            beneficiary_balance_accessed: false,
        }
    }
}

impl SensitiveDataAccessTracker {
    /// Creates a new tracker with no accesses recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks if any sensitive data has been accessed.
    pub fn accessed(&self) -> bool {
        !self.block_env_accessed.is_empty() || self.beneficiary_balance_accessed
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

    /// Resets all access tracking for a new transaction.
    pub fn reset(&mut self) {
        self.block_env_accessed = BlockEnvAccess::empty();
        self.beneficiary_balance_accessed = false;
    }
}
