//! Tests for `Rex5` hardfork features.

mod apply_pending_changes_gas_budget;
mod callcode_storage_gas;
mod create2_empty_initcode;
mod create2_resize_gas_metering;
mod db_error;
mod eip7702_metering;
mod keyless_empty_code_logs;
mod keyless_fee_free;
mod keyless_gas_cap_postcap_recheck;
mod keyless_replay_barrier;
mod oracle_hint_metering;
mod pre_block_system_calls;
mod sandbox_accounting;
mod selfdestruct_beneficiary;
mod sstore_storage_gas_error;
mod stipend_accounting;
