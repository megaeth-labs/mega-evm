//! What a block counts of its transactions' gas.

use crate::MegaGasUsage;

/// A block's gas, on the three ledgers its transactions spend on.
///
/// The block executor fills it transaction by transaction with [`record`](Self::record) and
/// holds each counter to its own block limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockGasCounters {
    /// Regular gas: each transaction's regular ledger, at least its EIP-7623 floor.
    pub execution: u64,
    /// State gas.
    pub state: u64,
    /// History gas.
    pub history: u64,
}

impl BlockGasCounters {
    /// Adds one transaction's gas.
    pub const fn record(&mut self, gas: &MegaGasUsage) {
        self.execution = self.execution.saturating_add(gas.block_execution_gas());
        self.state = self.state.saturating_add(gas.state);
        self.history = self.history.saturating_add(gas.history);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_counters_add_each_ledger() {
        let mut block = BlockGasCounters::default();
        let first =
            MegaGasUsage { regular: 10, state: 20, history: 30, floor: 0, ..Default::default() };
        let floored =
            MegaGasUsage { regular: 5, state: 1, history: 2, floor: 50, ..Default::default() };
        block.record(&first);
        block.record(&floored);
        assert_eq!(block, BlockGasCounters { execution: 60, state: 21, history: 32 });
    }

    #[test]
    fn test_block_counters_saturate() {
        let mut block =
            BlockGasCounters { execution: u64::MAX, state: u64::MAX, history: u64::MAX };
        block.record(&MegaGasUsage { regular: 1, state: 1, history: 1, ..Default::default() });
        assert_eq!(
            block,
            BlockGasCounters { execution: u64::MAX, state: u64::MAX, history: u64::MAX }
        );
    }
}
