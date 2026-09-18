//! Keyless deployment (Nick's Method): the transaction format, its validation rules and the
//! error ABI.
//!
//! These are data-only helpers: decoding the pre-EIP-155 transaction, recovering its signer,
//! deriving the deploy address, and mapping errors to and from the `IKeylessDeploy` ABI. T6.1
//! adds the interceptor that dispatches `keylessDeploy` calls, and T7 executes the deployment
//! as a native CREATE sub-frame.

mod error;
mod tx;

pub use error::*;
pub use tx::*;
