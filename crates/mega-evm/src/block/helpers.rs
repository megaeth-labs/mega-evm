use alloy_consensus::transaction::Recovered;
use alloy_eips::Encodable2718;

use crate::MegaTxEnvelope;

/// Helper trait that allows attaching an estimated data availability size.
pub trait TxDASize {
    /// Get the estimated data availability size of the transaction.
    ///
    /// Note: the default implementation is not efficient since it does not cache the `da_size` and
    /// always recalculates it.
    fn estimated_da_size(&self) -> u64
    where
        Self: Encodable2718,
    {
        op_alloy_flz::tx_estimated_size_fjord_bytes(self.encoded_2718().as_slice())
    }
}

impl TxDASize for Recovered<MegaTxEnvelope> {}

impl TxDASize for MegaTxEnvelope {}
