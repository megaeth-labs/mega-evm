use crate::{constants, BlockEnvAccess, OracleAccessTracker};
use revm::{handler::FrameResult, interpreter::Gas};

/// A tracker for sensitive data access with global gas detention mechanism.
///
/// This tracker manages sensitive data access detection (block environment, beneficiary, oracle)
/// and implements a global gas detention system to prevent `DoS` attacks while maintaining fair
/// gas accounting.
///
/// # Global Gas Detention Mechanism
///
/// When sensitive data is first accessed in a transaction:
/// 1. A `GlobalLimitedGas` instance is created with `SENSITIVE_DATA_ACCESS_REMAINING_GAS` (10,000)
/// 2. Any gas above this limit is "detained" (tracked separately, not consumed)
/// 3. The same global limit applies to all subsequent gas detentions in the transaction
/// 4. At transaction end, all detained gas is refunded via `refund_detained_gas()`
///
/// # Key Properties
///
/// - **Global Limit**: The 10,000 gas limit is established once per transaction, not per opcode
/// - **Cumulative Tracking**: All detained gas from multiple opcodes is accumulated
/// - **Cross-Call Consistency**: The global limit applies across nested calls (see
///   `update_remained_gas()`)
/// - **Fair Billing**: Users pay only for actual work, detained gas is refunded
///
/// # Example Flow
///
/// ```ignore
/// // Transaction starts with 1,000,000 gas
/// TIMESTAMP opcode:
///   - Marks block_env_accessed
///   - Creates GlobalLimitedGas { remaining: 10,000, detained: 0 }
///   - Detains 990,000 gas → GlobalLimitedGas { remaining: 10,000, detained: 990,000 }
///
/// BALANCE(beneficiary) opcode:
///   - Marks beneficiary_balance_accessed
///   - GlobalLimitedGas already exists
///   - Remaining gas is 9,500 (after TIMESTAMP + some work)
///   - No additional detention (9,500 < 10,000)
///
/// Transaction end:
///   - refund_detained_gas() returns 990,000 gas to user
///   - User only pays for ~10,000 gas of actual work
/// ```
#[derive(Debug, Clone, Default)]
pub struct SensitiveDataAccessTracker {
    /// Bitmap of block environment data accessed during transaction execution.
    block_env_accessed: BlockEnvAccess,
    /// Whether beneficiary data has been accessed in current transaction.
    beneficiary_balance_accessed: bool,
    /// Tracker for oracle contract access.
    oracle_tracker: OracleAccessTracker,
    /// The global (tx level) remaining gas after sensitive data access.
    global_limited_gas: Option<GlobalLimitedGas>,
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
        if self.global_limited_gas.is_none() {
            self.global_limited_gas = Some(GlobalLimitedGas::new());
        }
    }

    /// Checks if beneficiary balance has been accessed.
    pub fn has_accessed_beneficiary_balance(&self) -> bool {
        self.beneficiary_balance_accessed
    }

    /// Marks that beneficiary balance has been accessed.
    pub fn mark_beneficiary_balance_accessed(&mut self) {
        self.beneficiary_balance_accessed = true;
        if self.global_limited_gas.is_none() {
            self.global_limited_gas = Some(GlobalLimitedGas::new());
        }
    }

    /// Checks if the oracle contract has been accessed.
    pub fn has_accessed_oracle(&self) -> bool {
        self.oracle_tracker.has_accessed()
    }

    /// Checks if the given address is the oracle contract address and marks it as accessed.
    pub fn check_and_mark_oracle_access(&mut self, address: &alloy_primitives::Address) -> bool {
        if self.oracle_tracker.check_and_mark_oracle_access(address) {
            if self.global_limited_gas.is_none() {
                self.global_limited_gas = Some(GlobalLimitedGas::new());
            }
            true
        } else {
            false
        }
    }

    /// Resets all access tracking for a new transaction.
    pub fn reset(&mut self) {
        self.block_env_accessed = BlockEnvAccess::empty();
        self.beneficiary_balance_accessed = false;
        self.oracle_tracker.reset();
        self.global_limited_gas = None;
    }

    /// Returns the amount of detained gas by the end of transaction.
    ///
    /// If the sensitive data is not accessed, this will be a no-op.
    pub fn refund_detained_gas(&mut self, gas: &mut Gas) {
        if let Some(global_limited_gas) = self.global_limited_gas.as_mut() {
            gas.erase_cost(global_limited_gas.refund());
        }
    }

    /// Records the remaining gas in the returned frame.
    ///
    /// # Panics
    ///
    /// This will panic if the sensitive data is not accessed.
    pub fn update_remained_gas(&mut self, remaining: u64) {
        let global_limited_gas = self
            .global_limited_gas
            .as_mut()
            .expect("sensitive data is not accessed while trying to record gas remaining");
        global_limited_gas.set_remaining(remaining);
    }

    /// Force limit the remaining gas to the sensitive data access remaining gas.
    ///
    /// This method limits the remaining gas and records the enforcement gas consumed.
    /// This semantic should only be used when `MegaSpecId::MINI_REX` is enabled.
    ///
    /// If the sensitive data is not accessed, this will be a no-op.
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
    /// If the sensitive data is not accessed, this will be a no-op.
    pub fn detain_plain_gas(&mut self, gas_limit: &mut u64) {
        if let Some(global_limited_gas) = self.global_limited_gas.as_mut() {
            global_limited_gas.detain_gas(gas_limit)
        }
    }

    /// Force limit the remaining gas on a frame result.
    ///
    /// This semantic should only be used when `MegaSpecId::MINI_REX` is enabled.
    ///
    /// If the sensitive data is not accessed, this will be a no-op.
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
/// - A single, consistent gas limit (10,000) applies across all sensitive data accesses
/// - All detained gas is accumulated for later refund
/// - The remaining gas is updated as the transaction progresses through nested calls
///
/// # Fields
///
/// - `remaining`: Current global gas limit (starts at 10,000, decreases as gas is consumed)
/// - `detained`: Total amount of gas detained from all opcodes (refunded at transaction end)
#[derive(Debug, Clone)]
struct GlobalLimitedGas {
    remaining: u64,
    detained: u64,
}

impl GlobalLimitedGas {
    /// Creates a new global limited gas with the default remaining gas.
    fn new() -> Self {
        Self { remaining: constants::mini_rex::SENSITIVE_DATA_ACCESS_REMAINING_GAS, detained: 0 }
    }

    /// Detains gas from the given gas limit. Any detained gas will be refunded.
    /// The gas limit will be updated in place.
    fn detain_gas(&mut self, gas_limit: &mut u64) {
        if self.remaining < *gas_limit {
            let detained = *gas_limit - self.remaining;
            self.detained += detained;
            *gas_limit = self.remaining;
        }
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
