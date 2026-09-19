//! Keyless deployment (Nick's Method): the transaction format, its validation rules and the
//! error ABI.
//!
//! These are data-only helpers: decoding the pre-EIP-155 transaction, recovering its signer,
//! deriving the deploy address, and mapping errors to and from the `IKeylessDeploy` ABI. The
//! interceptor that dispatches `keylessDeploy` calls and the deployment as a native CREATE
//! sub-frame are not here yet.

mod error;
mod tx;

pub use error::*;
pub use tx::*;
