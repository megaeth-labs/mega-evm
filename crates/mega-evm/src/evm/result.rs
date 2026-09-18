//! What a Satin transaction reports: its gas by ledger.
//!
//! A transaction pays from the EIP-8037 pools (the reservoir first, then regular gas) on three
//! ledgers, counted apart:
//!
//! - **regular**: execution. It is the compute gas the resource limits meter.
//! - **state**: bytes added to the world state (EIP-8037 state gas).
//! - **history**: bytes appended to the chain's history (logs, deployed code, write records, the
//!   transaction body).
//!
//! State or history gas that spilled from the reservoir onto regular gas stays on its own ledger,
//! so it is not compute. The ledgers are raw spend, before the refund; the receipt reports
//! [`gas_used`](MegaGasUsage::gas_used), which is their sum less the refund, at least the EIP-7623
//! floor.

use revm::context::result::ResultGas;

/// The gas one transaction spent, by ledger, and what its receipt reports.
///
/// `regular + state + history` is the transaction's raw spend,
/// [`ResultGas::total_gas_spent`]; [`gas_used`](Self::gas_used) is
/// [`ResultGas::tx_gas_used`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MegaGasUsage {
    /// Regular gas spent: everything that is neither state nor history gas, before the refund.
    pub regular: u64,
    /// State gas spent, net of refills.
    pub state: u64,
    /// History gas spent, net of refills.
    pub history: u64,
    /// The EIP-8037 reservoir left unspent, which the sender gets back.
    pub reservoir_remaining: u64,
    /// The EIP-7623 floor the receipt's gas used cannot fall below.
    pub floor: u64,
    /// The gas used the receipt reports: the raw spend less the refund, at least the floor.
    pub gas_used: u64,
}

impl MegaGasUsage {
    /// The ledgers of a transaction that settled into `result_gas` and spent `history` gas on
    /// history.
    ///
    /// [`ResultGas`] has no history field; its total includes history gas, which is taken back
    /// out of regular here.
    pub const fn new(result_gas: &ResultGas, history: u64) -> Self {
        let state = result_gas.state_gas_spent_final();
        Self {
            regular: result_gas.total_gas_spent().saturating_sub(state).saturating_sub(history),
            state,
            history,
            reservoir_remaining: result_gas.reservoir_remaining(),
            floor: result_gas.floor_gas(),
            gas_used: result_gas.tx_gas_used(),
        }
    }

    /// The regular gas a block counts for the transaction: the regular ledger, at least the
    /// floor (EIP-8037 with EIP-7778). The refund does not lower it.
    pub const fn block_execution_gas(&self) -> u64 {
        if self.regular > self.floor {
            self.regular
        } else {
            self.floor
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LimitCheck, LimitKind, MegaHaltReason, MegaLimitExceeded};
    use alloy_sol_types::SolError;
    use op_revm::OpHaltReason;
    use revm::context::result::{HaltReason as EthHaltReason, OutOfGasError};

    fn result_gas(total: u64, state: u64, refunded: u64, floor: u64) -> ResultGas {
        ResultGas::default()
            .with_total_gas_spent(total)
            .with_state_gas_spent(state)
            .with_refunded(refunded)
            .with_floor_gas(floor)
            .with_reservoir_remaining(5_000)
    }

    /// The three ledgers add up to the raw spend; the receipt figure is the spend less the refund.
    #[test]
    fn test_ledgers_split_the_raw_spend() {
        let gas = MegaGasUsage::new(&result_gas(100_000, 30_000, 4_000, 21_000), 10_000);
        assert_eq!(gas.regular, 60_000);
        assert_eq!(gas.state, 30_000);
        assert_eq!(gas.history, 10_000);
        assert_eq!(gas.regular + gas.state + gas.history, 100_000);
        assert_eq!(gas.reservoir_remaining, 5_000);
        assert_eq!(gas.gas_used, 96_000);
        assert_eq!(gas.block_execution_gas(), 60_000);
    }

    /// The floor binds the receipt figure and the block's execution gas, not the ledgers.
    #[test]
    fn test_floor_binds_receipt_and_block_execution_gas() {
        let gas = MegaGasUsage::new(&result_gas(50_000, 20_000, 10_000, 45_000), 5_000);
        assert_eq!(gas.regular, 25_000);
        assert_eq!(gas.floor, 45_000);
        assert_eq!(gas.gas_used, 45_000);
        assert_eq!(gas.block_execution_gas(), 45_000);
    }

    /// Satin's halt set is op-revm's: an Ethereum halt converts in and back out.
    #[test]
    fn test_base_halt_reasons_convert_roundtrip() {
        let eth_reason = EthHaltReason::OutOfGas(OutOfGasError::Basic);
        let mega = MegaHaltReason::from(eth_reason.clone());
        assert_eq!(mega, OpHaltReason::Base(eth_reason.clone()));
        assert_eq!(EthHaltReason::try_from(mega).unwrap(), eth_reason);
    }

    /// A limit stop is not a halt: it is a revert whose output is `MegaLimitExceeded`, for every
    /// dimension.
    #[test]
    fn test_mega_specific_halt_reasons_do_not_convert_to_eth() {
        for kind in [
            LimitKind::DataSize,
            LimitKind::KVUpdate,
            LimitKind::ComputeGas,
            LimitKind::StateGrowth,
        ] {
            let stop = LimitCheck::ExceedsLimit { kind, limit: 100, used: 101, frame_local: false };
            let decoded = MegaLimitExceeded::abi_decode(&stop.revert_data()).unwrap();
            assert_eq!(LimitKind::from_u8(decoded.kind), Some(kind));
            assert_eq!(decoded.limit, 100);
        }
    }

    /// The one halt reason op-revm adds, a failed deposit, has no Ethereum counterpart.
    #[test]
    fn test_op_specific_halt_reason_does_not_convert_to_eth() {
        let mega = MegaHaltReason::FailedDeposit;
        assert_eq!(EthHaltReason::try_from(mega.clone()), Err(mega));
    }

    /// No halt reason of Satin's own exists: every one but the failed deposit is Ethereum's.
    #[test]
    fn test_all_mega_specific_variants_fail_eth_conversion() {
        let reasons = [
            MegaHaltReason::Base(EthHaltReason::OutOfGas(OutOfGasError::Basic)),
            MegaHaltReason::Base(EthHaltReason::OpcodeNotFound),
            MegaHaltReason::Base(EthHaltReason::CallTooDeep),
            MegaHaltReason::FailedDeposit,
        ];
        let converted =
            reasons.iter().filter(|r| EthHaltReason::try_from((*r).clone()).is_ok()).count();
        assert_eq!(converted, reasons.len() - 1, "only the failed deposit is OP-specific");
    }
}
