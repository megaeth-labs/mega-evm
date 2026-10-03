//! What the scenarios are charged, worked out from the Satin schedule and the byte prices in
//! effect rather than read back from the engine.
//!
//! Every figure goes through the schedule's entries and the engine's price functions, so it holds
//! at any byte price a measurement build runs.

use mega_evm::{history_gas, satin_gas_params, tx_body_history_bytes, write_record_history_gas};
use revm::context_interface::cfg::GasId;

/// The schedule's entry `id`.
pub(crate) fn entry(id: GasId) -> u64 {
    satin_gas_params().get(id)
}

/// The history gas of `bytes` bytes.
pub(crate) fn history(bytes: u64) -> u64 {
    history_gas(bytes).expect("a byte count has a price")
}

/// The history gas of `records` write records.
pub(crate) fn records(records: u64) -> u64 {
    write_record_history_gas(records).expect("a record has a price")
}

/// The history gas of the body of a transaction carrying `calldata_len` bytes of calldata and no
/// access list or authorization.
pub(crate) fn body(calldata_len: usize) -> u64 {
    history(tx_body_history_bytes(calldata_len as u64, 0, 0, 0))
}

/// The state gas of one fresh storage slot, in the minimum bucket.
pub(crate) fn slot_state() -> u64 {
    entry(GasId::sstore_set_state_gas())
}

/// The regular gas of an `SSTORE` that fills a fresh, cold slot: its static cost, the cost of a
/// fresh slot before the load, and the cold load.
pub(crate) fn sstore_fresh_cold() -> u64 {
    entry(GasId::sstore_static()) +
        entry(GasId::sstore_set_without_load_cost()) +
        entry(GasId::cold_storage_cost())
}
