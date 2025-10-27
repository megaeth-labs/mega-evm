use crate::{constants, VolatileDataAccess, ORACLE_CONTRACT_ADDRESS};
use alloy_primitives::Address;
use revm::{handler::FrameResult, interpreter::Gas};

/// A tracker for volatile data access with global gas detention mechanism.
///
/// This tracker manages volatile data access detection (block environment, beneficiary, oracle)
/// and implements a global gas detention system to prevent `DoS` attacks while maintaining fair
/// gas accounting.
///
/// # Global Gas Detention Mechanism
///
/// When volatile data is first accessed in a transaction:
/// 1. A `GlobalLimitedGas` instance is created with an appropriate limit:
///    - `BLOCK_ENV_ACCESS_REMAINING_GAS` (20M) for block environment or beneficiary
///    - `ORACLE_ACCESS_REMAINING_GAS` (1M) for oracle contract
/// 2. Any gas above this limit is "detained" (tracked separately, not consumed)
/// 3. If additional volatile data is accessed with a different limit, the **most restrictive**
///    limit (minimum) is applied
/// 4. At transaction end, all detained gas is refunded via `refund_detained_gas()`
///
/// # Key Properties
///
/// - **Type-Specific Limits**: Block env/beneficiary access → 20M gas, Oracle access → 1M gas
/// - **Most Restrictive Wins**: Multiple accesses with different limits → minimum limit applied
/// - **Order Independent**: Oracle→BlockEnv or BlockEnv→Oracle both result in same final limit
/// - **Cumulative Tracking**: All detained gas from multiple opcodes is accumulated
/// - **Cross-Call Consistency**: The global limit applies across nested calls (see
///   `update_remained_gas()`)
/// - **Fair Billing**: Users pay only for actual work, detained gas is refunded
///
/// # Example Flows
///
/// ## Example 1: Block env then oracle (currently 20M > 1M)
/// ```ignore
/// // Transaction starts with 1,000,000,000 gas
/// TIMESTAMP opcode:
///   - Marks block_env_accessed
///   - Creates GlobalLimitedGas { remaining: 20M, detained: 0 }
///   - Detains excess gas → { remaining: 20M, detained: 980M }
///
/// CALL(oracle) opcode:
///   - Marks oracle_accessed
///   - Applies min(20M, 1M) = 1M limit
///   - Further restricts → { remaining: 1M, detained: 999M }
///
/// Transaction end:
///   - refund_detained_gas() returns 999M to user
/// ```
///
/// ## Example 2: Oracle then block env (order independent)
/// ```ignore
/// // Transaction starts with 1,000,000,000 gas
/// CALL(oracle) opcode:
///   - Marks oracle_accessed
///   - Creates GlobalLimitedGas { remaining: 1M, detained: 0 }
///   - Detains excess gas → { remaining: 1M, detained: 999M }
///
/// TIMESTAMP opcode:
///   - Marks block_env_accessed
///   - Applies min(1M, 20M) = 1M limit (no change)
///   - No additional detention needed
///
/// Transaction end:
///   - refund_detained_gas() returns 999M to user
/// ```
#[derive(Debug, Clone, Default)]
pub struct VolatileDataAccessTracker {
    /// Unified bitmap tracking all types of volatile data access.
    /// Includes block environment fields, beneficiary balance, and oracle access.
    volatile_data_accessed: VolatileDataAccess,
    /// The global (tx level) remaining gas after volatile data access.
    global_limited_gas: Option<GlobalLimitedGas>,
}

impl VolatileDataAccessTracker {
    /// Creates a new tracker with no accesses recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks if any volatile data has been accessed.
    /// If so, the remaining gas in all message calls will be limited to a small amount of gas,
    /// forcing the transaction to finish execution soon.
    pub fn accessed(&self) -> bool {
        !self.volatile_data_accessed.is_empty()
    }

    /// Returns the volatile data access information: (`access_type`, limit, detained).
    /// Returns None if no volatile data has been accessed.
    pub fn get_volatile_data_info(&self) -> Option<(VolatileDataAccess, u64, u64)> {
        if !self.accessed() {
            return None;
        }

        let global_limited_gas = self.global_limited_gas.as_ref()?;

        Some((self.volatile_data_accessed, global_limited_gas.limit, global_limited_gas.detained))
    }

    /// Returns the bitmap of block environment data accessed during transaction execution.
    pub fn get_block_env_accesses(&self) -> VolatileDataAccess {
        self.volatile_data_accessed.block_env_only()
    }

    /// Marks that a specific type of block environment has been accessed.
    pub fn mark_block_env_accessed(&mut self, access_type: VolatileDataAccess) {
        self.volatile_data_accessed.insert(access_type);
        self.apply_or_create_limit(constants::mini_rex::BLOCK_ENV_ACCESS_REMAINING_GAS);
    }

    /// Checks if beneficiary balance has been accessed.
    pub fn has_accessed_beneficiary_balance(&self) -> bool {
        self.volatile_data_accessed.has_beneficiary_balance_access()
    }

    /// Marks that beneficiary balance has been accessed.
    pub fn mark_beneficiary_balance_accessed(&mut self) {
        self.volatile_data_accessed.insert(VolatileDataAccess::BENEFICIARY_BALANCE);
        self.apply_or_create_limit(constants::mini_rex::BLOCK_ENV_ACCESS_REMAINING_GAS);
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
            self.apply_or_create_limit(constants::mini_rex::ORACLE_ACCESS_REMAINING_GAS);
            true
        } else {
            false
        }
    }

    /// Applies a gas limit or creates a new one if none exists.
    /// If a limit already exists, applies the more restrictive limit (minimum of current and new).
    fn apply_or_create_limit(&mut self, limit: u64) {
        if let Some(global_limited_gas) = self.global_limited_gas.as_mut() {
            // A limit already exists - apply the more restrictive one
            global_limited_gas.apply_limit(limit);
        } else {
            // First volatile data access - create new limit
            self.global_limited_gas = Some(GlobalLimitedGas::new(limit));
        }
    }

    /// Resets all access tracking for a new transaction.
    pub fn reset(&mut self) {
        self.volatile_data_accessed = VolatileDataAccess::empty();
        self.global_limited_gas = None;
    }

    /// Returns the amount of detained gas by the end of transaction.
    ///
    /// If the volatile data is not accessed, this will be a no-op.
    pub fn refund_detained_gas(&mut self, gas: &mut Gas) {
        if let Some(global_limited_gas) = self.global_limited_gas.as_mut() {
            gas.erase_cost(global_limited_gas.refund());
        }
    }

    /// Records the remaining gas in the returned frame.
    ///
    /// # Panics
    ///
    /// This will panic if the volatile data is not accessed.
    pub fn update_remained_gas(&mut self, remaining: u64) {
        let global_limited_gas = self
            .global_limited_gas
            .as_mut()
            .expect("volatile data is not accessed while trying to record gas remaining");
        global_limited_gas.set_remaining(remaining);
    }

    /// Force limit the remaining gas to the volatile data access remaining gas.
    ///
    /// This method limits the remaining gas and records the enforcement gas consumed.
    /// This semantic should only be used when `MegaSpecId::MINI_REX` is enabled.
    ///
    /// If the volatile data is not accessed, this will be a no-op.
    pub fn detain_gas(&mut self, gas: &mut Gas) {
        if let Some(global_limited_gas) = self.global_limited_gas.as_mut() {
            let mut remaining = gas.remaining();
            global_limited_gas.detain_gas(&mut remaining);
            let detained = gas.remaining() - remaining;
            let _ = gas.record_cost(detained); // no underflow error here because detained is always
                                               // smaller than gas.remaining()
        }
    }

    /// Detains gas from the given gas limit. Any detained gas will be refunded. The gas
    /// limit will be updated in place.
    ///
    /// If the volatile data is not accessed, this will be a no-op.
    pub fn detain_plain_gas(&mut self, gas_limit: &mut u64) {
        if let Some(global_limited_gas) = self.global_limited_gas.as_mut() {
            global_limited_gas.detain_gas(gas_limit)
        }
    }

    /// Force limit the remaining gas on a frame result.
    ///
    /// This semantic should only be used when `MegaSpecId::MINI_REX` is enabled.
    ///
    /// If the volatile data is not accessed, this will be a no-op.
    pub fn detain_gas_in_frame_result(&mut self, result: &mut FrameResult) {
        match result {
            FrameResult::Call(call_outcome) => {
                let gas = &mut call_outcome.result.gas;
                self.detain_gas(gas);
            }
            FrameResult::Create(create_outcome) => {
                let gas = &mut create_outcome.result.gas;
                self.detain_gas(gas);
            }
        }
    }
}

/// Global gas detention state for a transaction.
///
/// This struct manages the global gas limit and tracks detained gas across all opcodes
/// in a transaction. It ensures:
/// - A gas limit is established based on the type of volatile data accessed
/// - When multiple volatile data types are accessed, the **most restrictive** (minimum) limit
///   applies
/// - All detained gas is accumulated for later refund
/// - The remaining gas is updated as the transaction progresses through nested calls
///
/// # Fields
///
/// - `limit`: The gas limit enforced after volatile data access (immutable once set, may be lowered
///   by `apply_limit` to the more restrictive value)
/// - `remaining`: Current remaining gas (starts at limit, decreases as gas is consumed)
/// - `detained`: Total amount of gas detained from all opcodes (refunded at transaction end)
#[derive(Debug, Clone)]
struct GlobalLimitedGas {
    limit: u64,
    remaining: u64,
    detained: u64,
}

impl GlobalLimitedGas {
    /// Creates a new global limited gas with the specified gas limit.
    fn new(limit: u64) -> Self {
        Self { limit, remaining: limit, detained: 0 }
    }

    /// Detains gas from the given gas limit. Any detained gas will be refunded.
    /// The gas limit will be updated in place.
    ///
    /// Supports progressive restriction: if a new lower limit is provided, it will
    /// further restrict the remaining gas and detain the difference.
    fn detain_gas(&mut self, gas_limit: &mut u64) {
        if self.remaining < *gas_limit {
            let detained = *gas_limit - self.remaining;
            self.detained += detained;
            *gas_limit = self.remaining;
        }
    }

    /// Applies a new gas limit by taking the minimum of current and new limit.
    /// This ensures the most restrictive limit is always applied, making the behavior
    /// independent of the order in which volatile data is accessed.
    /// The actual gas detention happens when `detain_gas()` is called on the interpreter.
    fn apply_limit(&mut self, new_limit: u64) {
        // Always apply the more restrictive limit (minimum)
        self.limit = self.limit.min(new_limit);
        self.remaining = self.remaining.min(new_limit);
    }

    fn set_remaining(&mut self, remaining: u64) {
        self.remaining = remaining;
    }

    fn refund(&mut self) -> u64 {
        let refunded = self.detained;
        self.detained = 0;
        refunded
    }
}
