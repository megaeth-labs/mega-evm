//! Tests for `Rex6` hardfork features.
//!
//! - `eip7702_authority_accounting` — consolidated per-authorization accounting: dynamic SALT
//!   account-creation gas for net-new authorities, and DataSize/KV charged only for *applied*
//!   authorities (not every recoverable one).

mod common;
mod create2_metering_order;
mod eip7702_authority_accounting;
mod error_paths;
mod metering_order_parity;
mod system_tx_metering_exemption;
