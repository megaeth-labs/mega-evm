//! What a byte of history costs, and how many bytes each site appends.
//!
//! History gas is the third ledger of a Satin transaction. It pays for the bytes the transaction
//! appends to the chain's history — its own body, its logs, the code it deploys and one record per
//! account or storage write it keeps — as opposed to the bytes it adds to the world state, which
//! EIP-8037 state gas pays for. Both draw on the same reservoir-first budget and unwind on the same
//! paths; they are counted apart so a node can report and limit them separately.
//!
//! Every charge is a byte count from the byte table (the `limit` module) times the cost per history
//! byte, and the byte counts are the ones the data-size limit meters, so a log costs history for
//! exactly the bytes that limit counts it at. Nothing here reads a SALT bucket: history bytes live
//! in no bucket, and the multiplier that scales the state dimension never touches this one.
//!
//! The one history charge that is not made here is the deployed code's: it is a schedule entry
//! ([`code_deposit_history_gas`](revm::context_interface::cfg::GasId::code_deposit_history_gas)),
//! so revm charges it where it writes the code. The schedule gives that entry the cost of one byte
//! (see the `schedule` module), which is what switches it on.

use crate::{
    evm::prices::active_satin_prices,
    limit::{
        ACCESS_LIST_ADDRESS_SIZE, ACCESS_LIST_SLOT_SIZE, AUTHORIZATION_SIZE, LOG_BASE_SIZE,
        LOG_TOPIC_SIZE, TX_BODY_SIZE, WRITE_RECORD_SIZE,
    },
};

/// The gas `bytes` bytes of history cost, or `None` when the price does not fit in a `u64`.
///
/// A byte count that has no price is not a charge anyone can pay: the caller turns it into an
/// out-of-gas, which is what a count that large would be anyway.
#[inline]
pub fn history_gas(bytes: u64) -> Option<u64> {
    active_satin_prices().cphb.gas_for(bytes)
}

/// The gas `records` account or storage write records cost as history.
#[inline]
pub fn write_record_history_gas(records: u64) -> Option<u64> {
    history_gas(WRITE_RECORD_SIZE.checked_mul(records)?)
}

/// The history bytes a `LOG` with `topics` topics and `data_len` bytes of data appends: its own
/// record for the address that emitted it, one per topic, and the data.
#[inline]
pub const fn log_history_bytes(topics: u64, data_len: u64) -> u64 {
    LOG_BASE_SIZE.saturating_add(LOG_TOPIC_SIZE.saturating_mul(topics)).saturating_add(data_len)
}

/// The history bytes a transaction's body appends before it runs: the fixed body
/// ([`TX_BODY_SIZE`]), the calldata, one record per EIP-7702 authorization, and the access list at
/// the size of its addresses and keys.
///
/// A zero calldata byte costs the same as a non-zero one: both occupy the same space in a block.
/// The count saturates, and a saturated count has no price, so a transaction that would reach it
/// is rejected for not covering its own intrinsic gas.
#[inline]
pub const fn tx_body_history_bytes(
    calldata_len: u64,
    authorizations: u64,
    access_list_addresses: u64,
    access_list_slots: u64,
) -> u64 {
    TX_BODY_SIZE
        .saturating_add(calldata_len)
        .saturating_add(AUTHORIZATION_SIZE.saturating_mul(authorizations))
        .saturating_add(ACCESS_LIST_ADDRESS_SIZE.saturating_mul(access_list_addresses))
        .saturating_add(ACCESS_LIST_SLOT_SIZE.saturating_mul(access_list_slots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{constants::COST_PER_HISTORY_BYTE, evm::prices::runs_at_measurement_prices};

    /// The byte counts of the three sites that count bytes rather than records.
    #[test]
    fn test_the_byte_counts_follow_the_byte_table() {
        assert_eq!(log_history_bytes(0, 0), LOG_BASE_SIZE);
        assert_eq!(log_history_bytes(3, 32), LOG_BASE_SIZE + 3 * LOG_TOPIC_SIZE + 32);
        assert_eq!(log_history_bytes(3, 32), 160);
        assert_eq!(log_history_bytes(1, u64::MAX), u64::MAX, "the count saturates");

        assert_eq!(tx_body_history_bytes(0, 0, 0, 0), TX_BODY_SIZE);
        assert_eq!(tx_body_history_bytes(0, 0, 0, 0), 310);
        assert_eq!(
            tx_body_history_bytes(100, 2, 3, 4),
            TX_BODY_SIZE + 100 + 2 * AUTHORIZATION_SIZE + 3 * 20 + 4 * 32
        );
        assert_eq!(tx_body_history_bytes(u64::MAX, 1, 0, 0), u64::MAX, "the count saturates");
    }

    /// A charge is its byte count at the cost per history byte, and a count with no price
    /// reports none.
    #[test]
    fn test_a_charge_is_its_byte_count_at_the_price() {
        if runs_at_measurement_prices() {
            return;
        }
        assert_eq!(history_gas(0), Some(0));
        assert_eq!(history_gas(1), Some(COST_PER_HISTORY_BYTE));
        assert_eq!(history_gas(160), Some(160 * COST_PER_HISTORY_BYTE));
        assert_eq!(history_gas(u64::MAX), None, "an overflowing count has no price");

        assert_eq!(write_record_history_gas(0), Some(0));
        assert_eq!(write_record_history_gas(1), Some(WRITE_RECORD_SIZE * COST_PER_HISTORY_BYTE));
        assert_eq!(
            write_record_history_gas(3),
            Some(3 * WRITE_RECORD_SIZE * COST_PER_HISTORY_BYTE)
        );
        assert_eq!(write_record_history_gas(u64::MAX), None, "the byte count overflows");
    }
}
