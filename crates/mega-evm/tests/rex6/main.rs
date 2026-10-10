//! Tests for `Rex6` hardfork features.
//!
//! - `eip7702_authority_accounting` — consolidated per-authorization accounting: dynamic SALT
//!   account-creation gas for net-new authorities, and DataSize/KV charged only for *applied*
//!   authorities (not every recoverable one).
//! - `modexp_gas` — a cross-spec pin rather than a Rex6 feature: every spec keeps the historical
//!   zero-base/zero-modulus `ModExp` pricing. It lives in the latest spec's module.

mod beneficiary_detention;
mod common;
mod create2_metering_order;
mod create_frame_accounting;
mod create_storage_gas_residual;
mod eip7702_authority_accounting;
mod error_paths;
mod fee_reward_accounting;
mod frame_local_accounting;
mod keyless_sandbox_hardening;
mod metering_order_parity;
mod modexp_gas;
mod oracle_hint_volatile_access;
mod self_transfer_account_dedup;
mod sequencer_registry_rotation;
mod system_tx_metering_exemption;
mod volatile_guard_charge_order;
mod volatile_guard_static_gas;
