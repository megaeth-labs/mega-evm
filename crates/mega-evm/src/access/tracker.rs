use crate::{VolatileDataAccess, ORACLE_CONTRACT_ADDRESS};
use alloy_primitives::Address;

/// A tracker for volatile data access with compute gas limit enforcement.
///
/// This tracker manages volatile data access detection (block environment, beneficiary, oracle)
/// and tracks the compute gas limit to prevent `DoS` attacks.
///
/// # Compute Gas Limit Enforcement
///
/// When volatile data is first accessed in a transaction:
/// 1. The compute gas limit is determined based on the type:
///    - `BLOCK_ENV_ACCESS_REMAINING_GAS` (20M) for block environment or beneficiary
///    - `ORACLE_ACCESS_REMAINING_GAS` (1M) for oracle contract
/// 2. If additional volatile data is accessed with a different limit, the **most restrictive**
///    limit (minimum) is applied
/// 3. The caller is responsible for applying this limit to the `AdditionalLimit`
///
/// # Key Properties
///
/// - **Type-Specific Limits**: Block env/beneficiary access → 20M compute gas, Oracle access → 1M
///   compute gas
/// - **Most Restrictive Wins**: Multiple accesses with different limits → minimum limit applied
/// - **Order Independent**: Oracle→BlockEnv or BlockEnv→Oracle both result in same final limit
/// - **Compute Gas Only**: Only limits compute gas costs, not storage gas cost
///
/// # Example Flows
///
/// ## Example 1: Block env then oracle (currently 20M > 1M)
/// ```ignore
/// // Transaction starts with compute gas limit of 30M
/// TIMESTAMP opcode:
///   - Marks block_env_accessed
///   - Compute gas limit tracked as 20M
///
/// CALL(oracle) opcode:
///   - Marks oracle_accessed
///   - Compute gas limit lowered to min(20M, 1M) = 1M
///
/// // Caller applies 1M compute gas limit to AdditionalLimit
/// ```
///
/// ## Example 2: Oracle then block env (order independent)
/// ```ignore
/// // Transaction starts with compute gas limit of 30M
/// CALL(oracle) opcode:
///   - Marks oracle_accessed
///   - Compute gas limit tracked as 1M
///
/// TIMESTAMP opcode:
///   - Marks block_env_accessed
///   - Compute gas limit remains min(1M, 20M) = 1M
///
/// // Caller applies 1M compute gas limit to AdditionalLimit
/// ```
#[derive(Debug, Clone)]
pub struct VolatileDataAccessTracker {
    /// Unified bitmap tracking all types of volatile data access.
    /// Includes block environment fields, beneficiary balance, and oracle access.
    volatile_data_accessed: VolatileDataAccess,
    /// The compute gas limit to enforce when volatile data is accessed.
    compute_gas_limit: Option<u64>,
    /// Compute gas limit when accessing block environment data.
    block_env_access_limit: u64,
    /// Compute gas limit when accessing oracle data.
    oracle_access_limit: u64,
}

impl VolatileDataAccessTracker {
    /// Creates a new tracker with no accesses recorded and configurable limits.
    pub fn new(block_env_access_limit: u64, oracle_access_limit: u64) -> Self {
        Self {
            volatile_data_accessed: VolatileDataAccess::empty(),
            compute_gas_limit: None,
            block_env_access_limit,
            oracle_access_limit,
        }
    }

    /// Checks if any volatile data has been accessed.
    /// If so, the remaining gas in all message calls will be limited to a small amount of gas,
    /// forcing the transaction to finish execution soon.
    pub fn accessed(&self) -> bool {
        !self.volatile_data_accessed.is_empty()
    }

    /// Returns the volatile data access information: (`access_type`, `compute_gas_limit`).
    /// Returns None if no volatile data has been accessed.
    pub fn get_volatile_data_info(&self) -> Option<(VolatileDataAccess, u64)> {
        if !self.accessed() {
            return None;
        }

        let compute_gas_limit = self.compute_gas_limit?;

        Some((self.volatile_data_accessed, compute_gas_limit))
    }

    /// Returns the compute gas limit for the accessed volatile data.
    /// Returns None if no volatile data has been accessed.
    pub fn get_compute_gas_limit(&self) -> Option<u64> {
        self.compute_gas_limit
    }

    /// Returns the bitmap of block environment data accessed during transaction execution.
    pub fn get_block_env_accesses(&self) -> VolatileDataAccess {
        self.volatile_data_accessed.block_env_only()
    }

    /// Returns the bitmap of all volatile data accessed during transaction execution.
    pub fn get_volatile_data_accessed(&self) -> VolatileDataAccess {
        self.volatile_data_accessed
    }

    /// Marks that a specific type of block environment has been accessed.
    pub fn mark_block_env_accessed(&mut self, access_type: VolatileDataAccess) {
        self.volatile_data_accessed.insert(access_type);
        self.apply_or_create_limit(self.block_env_access_limit);
    }

    /// Checks if beneficiary balance has been accessed.
    pub fn has_accessed_beneficiary_balance(&self) -> bool {
        self.volatile_data_accessed.has_beneficiary_balance_access()
    }

    /// Marks that beneficiary balance has been accessed.
    pub fn mark_beneficiary_balance_accessed(&mut self) {
        self.volatile_data_accessed.insert(VolatileDataAccess::BENEFICIARY_BALANCE);
        self.apply_or_create_limit(self.block_env_access_limit);
    }

    /// Checks if the oracle contract has been accessed.
    pub fn has_accessed_oracle(&self) -> bool {
        self.volatile_data_accessed.has_oracle_access()
    }

    /// Checks if the given address is the oracle contract address and marks it as accessed.
    /// Applies the oracle access gas limit, which may further restrict gas if a less
    /// restrictive limit was already in place.
    pub fn check_and_mark_oracle_access(&mut self, address: &Address) -> bool {
        if address == &ORACLE_CONTRACT_ADDRESS {
            self.volatile_data_accessed.insert(VolatileDataAccess::ORACLE);
            self.apply_or_create_limit(self.oracle_access_limit);
            true
        } else {
            false
        }
    }

    /// Applies a compute gas limit or creates a new one if none exists.
    /// If a limit already exists, applies the more restrictive limit (minimum of current and new).
    fn apply_or_create_limit(&mut self, limit: u64) {
        if let Some(current_limit) = self.compute_gas_limit {
            // A limit already exists - apply the more restrictive one
            self.compute_gas_limit = Some(current_limit.min(limit));
        } else {
            // First volatile data access - set the initial limit
            self.compute_gas_limit = Some(limit);
        }
    }

    /// Resets all access tracking for a new transaction.
    /// Preserves the configured limits (only resets access state).
    pub fn reset(&mut self) {
        self.volatile_data_accessed = VolatileDataAccess::empty();
        self.compute_gas_limit = None;
    }
}
