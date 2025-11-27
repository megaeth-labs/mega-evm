//! External environment for EVM execution.
//!
//! This module provides interfaces for accessing external data sources during EVM execution:
//! - **SALT**: Bucket capacity information for dynamic gas pricing
//! - **Oracle**: Storage from the `MegaETH` oracle contract
//!
//! # Architecture
//!
//! External environments follow a factory pattern:
//! 1. [`ExternalEnvFactory`] creates block-specific environment instances
//! 2. [`ExternalEnvs`] bundles SALT and Oracle implementations
//! 3. Individual oracle methods (e.g., [`SaltEnv::get_bucket_capacity`]) provide data
//!
//! Block context is established at factory creation time, not per oracle call, ensuring
//! consistent state snapshots throughout execution.

use alloy_primitives::BlockNumber;
use auto_impl::auto_impl;
use core::fmt::Debug;

mod factory;
mod gas;
mod oracle;
mod salt;
#[cfg(any(test, feature = "test-utils"))]
mod test_utils;

pub use factory::*;
pub use gas::*;
pub use oracle::*;
pub use salt::*;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::*;

/// Type-level specification of external environment implementations.
///
/// This trait associates concrete SALT and Oracle types, allowing generic code to work
/// with any compatible environment configuration.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvTypes {
    /// SALT environment implementation for bucket capacity queries.
    type SaltEnv: SaltEnv;
    /// Oracle environment implementation for system contract storage queries.
    type OracleEnv: OracleEnv;
}

/// Tuple implementation for convenient pairing of SALT and Oracle environments.
impl<A: SaltEnv, B: OracleEnv> ExternalEnvTypes for (A, B) {
    type SaltEnv = A;
    type OracleEnv = B;
}

/// Bundle of external environment instances for a specific execution context.
///
/// This struct holds concrete SALT and Oracle implementations that are used during
/// EVM execution. Typically created by [`ExternalEnvFactory::external_envs`] at the
/// start of block processing.
#[derive(Debug, Clone)]
pub struct ExternalEnvs<T: ExternalEnvTypes> {
    /// SALT environment for bucket capacity queries and dynamic gas calculation.
    pub salt_env: T::SaltEnv,
    /// Oracle environment for reading system contract storage.
    pub oracle_env: T::OracleEnv,
}

impl Default for ExternalEnvs<EmptyExternalEnv> {
    fn default() -> Self {
        Self { salt_env: EmptyExternalEnv, oracle_env: EmptyExternalEnv }
    }
}

/// No-op external environment for testing or when oracle functionality is disabled.
///
/// This implementation:
/// - Returns minimum bucket capacity for all SALT queries
/// - Returns `None` for all Oracle storage queries
/// - Assigns all accounts and storage slots to bucket 0
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyExternalEnv;

impl ExternalEnvTypes for EmptyExternalEnv {
    type SaltEnv = Self;
    type OracleEnv = Self;
}

impl ExternalEnvFactory for EmptyExternalEnv {
    type EnvTypes = Self;

    fn external_envs(&self, _block: BlockNumber) -> ExternalEnvs<Self::EnvTypes> {
        ExternalEnvs { salt_env: *self, oracle_env: *self }
    }
}
