//! Tests for `Rex5` hardfork features.

mod apply_pending_changes_gas_budget;
mod create_atomicity;
mod db_error;
mod deposit_create_storage_gas;
mod eip7702_metering;
mod eip7702_state_growth;
mod frame_target_updated_dedup;
mod gas_validation;
mod keyless_replay_barrier;
mod pre_block_system_calls;
mod precompile_compute_gas;
mod sandbox_accounting;
mod selfdestruct_beneficiary;
mod system_tx_replay;
