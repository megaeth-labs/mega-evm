/// A tracker for the total compute gas consumed during transaction execution.
#[derive(Debug, Clone, Default)]
pub struct ComputeGasTracker {
    /// Total gas consumed by the transaction across message calls.
    total_gas_used: u64,
}

impl ComputeGasTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reset(&mut self) {
        self.total_gas_used = 0;
    }

    pub(crate) fn current_gas_used(&self) -> u64 {
        self.total_gas_used
    }

    pub(crate) fn exceeds_limit(&self, limit: u64) -> bool {
        self.current_gas_used() > limit
    }
}

impl ComputeGasTracker {
    pub(crate) fn record_gas_used(&mut self, gas: u64) {
        self.total_gas_used += gas;
    }
}
