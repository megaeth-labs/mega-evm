//! Tests for `Rex5` hardfork features.

mod apply_pending_changes_gas_budget;
mod call_too_deep_guard;
mod create_atomicity;
mod db_error;
mod deposit_caller_accounting;
mod deposit_create_storage_gas;
mod eip7702_metering;
mod eip7702_state_growth;
mod frame_target_updated_dedup;
mod gas_validation;
mod interceptor_selector_probe;
mod keyless_deploy_dispatch_parity;
mod keyless_fee_free;
mod keyless_replay_barrier;
mod oracle_hint_metering;
mod pre_block_system_calls;
mod precompile_compute_gas;
mod sandbox_accounting;
mod selfdestruct_beneficiary;
mod stipend_accounting;
mod system_tx_replay;
