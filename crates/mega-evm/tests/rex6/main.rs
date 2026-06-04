//! Tests for `Rex6` hardfork features.
//!
//! - `eip7702_authority_accounting` — consolidated per-authorization accounting: dynamic SALT
//!   account-creation gas for net-new authorities, and DataSize/KV charged only for *applied*
//!   authorities (not every recoverable one).

mod beneficiary_detention;
mod common;
mod create2_metering_order;
mod create_frame_accounting;
mod eip7702_authority_accounting;
mod error_paths;
mod fee_reward_accounting;
mod frame_local_accounting;
mod keyless_sandbox_hardening;
mod metering_order_parity;
mod system_tx_metering_exemption;
