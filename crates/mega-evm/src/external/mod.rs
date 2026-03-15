//! External environment for EVM execution.
//!
//! This module provides interfaces for accessing external data sources during EVM execution:
//! - **Oracle**: Storage from the `MegaETH` oracle contract
//!
//! # Architecture
//!
//! External environments follow a factory pattern:
//! 1. [`ExternalEnvFactory`] creates environment instances
//! 2. [`ExternalEnvs`] bundles Oracle implementations
//! 3. Individual oracle methods (e.g., [`OracleEnv::get_oracle_storage`]) provide data

use auto_impl::auto_impl;
use core::fmt::Debug;

mod factory;
mod gas;
mod oracle;
#[cfg(any(test, feature = "test-utils"))]
mod test_utils;

pub use factory::*;
pub use gas::*;
pub use oracle::*;
#[cfg(any(test, feature = "test-utils"))]
pub use test_utils::*;

/// Type-level specification of external environment implementations.
///
/// This trait associates concrete Oracle types, allowing generic code to work
/// with any compatible environment configuration.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvTypes {
    /// Oracle environment implementation for system contract storage queries.
    type OracleEnv: OracleEnv;
}

/// Bundle of external environment instances for a specific execution context.
///
/// This struct holds concrete Oracle implementations that are used during
/// EVM execution. Typically created by [`ExternalEnvFactory::external_envs`] at the
/// start of block processing.
#[derive(Debug, Clone)]
pub struct ExternalEnvs<T: ExternalEnvTypes> {
    /// Oracle environment for reading system contract storage.
    pub oracle_env: T::OracleEnv,
}

impl Default for ExternalEnvs<EmptyExternalEnv> {
    fn default() -> Self {
        Self { oracle_env: EmptyExternalEnv }
    }
}

/// No-op external environment for testing or when oracle functionality is disabled.
///
/// This implementation:
/// - Returns `None` for all Oracle storage queries
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyExternalEnv;

impl ExternalEnvTypes for EmptyExternalEnv {
    type OracleEnv = Self;
}

impl ExternalEnvFactory for EmptyExternalEnv {
    type EnvTypes = Self;

    fn external_envs(&self) -> ExternalEnvs<Self::EnvTypes> {
        ExternalEnvs { oracle_env: *self }
    }
}
