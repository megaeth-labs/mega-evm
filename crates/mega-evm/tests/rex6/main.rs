//! Tests for `Rex6` hardfork features.
//!
//! - `eip7702_authority_accounting` — consolidated per-authorization accounting: dynamic SALT
//!   account-creation gas for net-new authorities, and DataSize/KV charged only for *applied*
//!   authorities (not every recoverable one).

mod eip7702_authority_accounting;
