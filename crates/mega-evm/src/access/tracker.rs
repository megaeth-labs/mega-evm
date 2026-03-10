use crate::{VolatileDataAccess, VolatileDataAccessType, ORACLE_CONTRACT_ADDRESS};
use alloy_primitives::Address;

/// A tracker for volatile data access with compute gas limit enforcement.
///
/// This tracker manages volatile data access detection (block environment, beneficiary, oracle)
/// and tracks the compute gas detention cap to prevent `DoS` attacks.
///
/// # Compute Gas Limit Enforcement
///
/// When volatile data is first accessed in a transaction, this tracker records the cap value:
/// - `BLOCK_ENV_ACCESS_COMPUTE_GAS` (20M) for block environment or beneficiary
/// - `ORACLE_ACCESS_COMPUTE_GAS` (configurable: 1M pre-Rex3, 20M Rex3+) for oracle contract
///
/// The cap value stored here is the **raw** per-access-type cap. How it is applied depends on
/// the spec:
/// - **Pre-REX4**: The cap is an **absolute** compute gas limit for the transaction.
/// - **REX4+**: The cap is **relative** — `ComputeGasTracker::set_detained_limit()` adds current
///   usage to produce the effective limit (`usage_at_access + cap`).
///
/// If additional volatile data is accessed with a different cap, the **most restrictive**
/// (minimum) raw cap is stored.
/// The caller is responsible for applying this cap to the `AdditionalLimit`.
///
/// # Key Properties
///
/// - **Type-Specific Caps**: Block env/beneficiary access → 20M compute gas, Oracle access →
///   configurable (1M pre-Rex3, 20M Rex3+)
/// - **Most Restrictive Wins**: Multiple accesses with different caps → minimum cap applied
/// - **Order Independent**: Oracle→BlockEnv or BlockEnv→Oracle both result in same final cap
/// - **Compute Gas Only**: Only limits compute gas costs, not storage gas cost
///
/// # Example Flows
///
/// ## Example 1: Block env then oracle (pre-Rex3, oracle limit = 1M)
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
/// ## Example 2: Oracle then block env (pre-Rex3, order independent)
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
///
/// In Rex3+, the oracle access limit is also 20M, so both limits are equal.
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

    /// The journal depth at which `disableVolatileDataAccess()` was activated (Rex4+).
    /// `None` means inactive. `Some(depth)` means calls with
    /// `journal.depth() >= depth` are restricted.
    disable_depth: Option<usize>,
}

impl VolatileDataAccessTracker {
    /// Creates a new tracker with no accesses recorded and configurable limits.
    pub fn new(block_env_access_limit: u64, oracle_access_limit: u64) -> Self {
        Self {
            volatile_data_accessed: VolatileDataAccess::empty(),
            compute_gas_limit: None,
            block_env_access_limit,
            oracle_access_limit,
            disable_depth: None,
        }
    }

    /// Checks if any volatile data has been accessed.
    /// If so, the remaining gas in all message calls will be limited to a small amount of gas,
    /// forcing the transaction to finish execution soon.
    pub fn accessed(&self) -> bool {
        !self.volatile_data_accessed.is_empty()
    }

    /// Returns the bitmap of volatile data accessed during transaction execution.
    /// Returns None if no volatile data has been accessed.
    pub fn get_volatile_data_info(&self) -> Option<VolatileDataAccess> {
        self.accessed().then_some(self.volatile_data_accessed)
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
    pub fn mark_block_env_accessed(&mut self, access_type: VolatileDataAccessType) {
        self.volatile_data_accessed.insert(access_type.into());
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

    /// Activates the volatile data access disable at the given depth.
    ///
    /// When active, `volatile_access_disabled(depth)` returns `true` for any
    /// `depth` greater than or equal to `disable_depth`, causing the caller and
    /// inner calls that access volatile data to revert with
    /// `VolatileDataAccessDisabled()`.
    ///
    /// If already active, keeps the shallower (more restrictive) depth.
    pub fn disable_access(&mut self, depth: usize) {
        match self.disable_depth {
            Some(current) if depth >= current => {}
            _ => self.disable_depth = Some(depth),
        }
    }

    /// Checks if volatile data access is disabled at the given journal depth.
    /// Returns `true` if `disable_depth` is set and `current_depth >= disable_depth`.
    pub fn volatile_access_disabled(&self, current_depth: usize) -> bool {
        self.disable_depth.is_some_and(|d| current_depth >= d)
    }

    /// Re-enables volatile data access.
    ///
    /// Succeeds (returns `true`) if access is not disabled, or the caller is at the same
    /// depth or shallower than the disabling frame (`caller_depth <= disable_depth`).
    /// Fails (returns `false`) if a parent frame disabled access (`caller_depth > disable_depth`).
    pub fn enable_access(&mut self, caller_depth: usize) -> bool {
        match self.disable_depth {
            None => true,
            Some(d) if caller_depth <= d => {
                self.disable_depth = None;
                true
            }
            Some(_) => false,
        }
    }

    /// Deactivates the disable if the current depth has dropped below the disable depth.
    ///
    /// Called on frame return to ensure the disable only applies to the activating frame's
    /// subtree and does not leak to sibling calls.
    /// When the journal depth drops strictly below the disable depth, the frame that called
    /// `disableVolatileDataAccess()` has returned and the restriction is no longer in scope.
    pub fn enable_access_if_returning(&mut self, current_depth: usize) {
        if self.disable_depth.is_some_and(|d| current_depth < d) {
            self.disable_depth = None;
        }
    }

    /// Resets all access tracking for a new transaction.
    /// Preserves the configured limits (only resets access state).
    pub fn reset(&mut self) {
        self.volatile_data_accessed = VolatileDataAccess::empty();
        self.compute_gas_limit = None;
        self.disable_depth = None;
    }
}
