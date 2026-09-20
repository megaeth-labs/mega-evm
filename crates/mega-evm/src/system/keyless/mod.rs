//! The `KeylessDeploy` system contract, and keyless deployment (Nick's Method): the transaction
//! format, its validation rules and the error ABI.
//!
//! `keylessDeploy(bytes,uint64)` is intercepted; every other selector falls through to the
//! deployed bytecode, which reverts with `NotIntercepted()`. What the interceptor does today is
//! the dispatch and the fixed compute charge; turning the deployment into a native creation
//! belongs to native keyless deployment.
//!
//! The rest of the module is data-only: decoding the pre-EIP-155 transaction, recovering its
//! signer, deriving the deploy address, and mapping errors to and from the `IKeylessDeploy` ABI.

mod error;
mod tx;

pub use error::*;
pub use tx::*;

use alloy_primitives::{address, Address};

/// The address of the `KeylessDeploy` system contract.
pub const KEYLESS_DEPLOY_ADDRESS: Address = address!("0x6342000000000000000000000000000000000003");

/// The code of the `KeylessDeploy` contract.
pub use mega_system_contracts::keyless_deploy::LATEST_CODE as KEYLESS_DEPLOY_CODE;

/// The code hash of the `KeylessDeploy` contract.
pub use mega_system_contracts::keyless_deploy::LATEST_CODE_HASH as KEYLESS_DEPLOY_CODE_HASH;

pub use mega_system_contracts::keyless_deploy::IKeylessDeploy;
