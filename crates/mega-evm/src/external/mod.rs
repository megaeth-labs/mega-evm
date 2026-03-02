//! External environment for EVM execution.
//!
//! This module provides interfaces for accessing external data sources during EVM execution:
//! - **SALT**: Bucket capacity information for dynamic gas pricing (now provided by `MegaDatabase`)
//! - **Oracle**: Storage from the `MegaETH` oracle contract
//!
//! # Architecture
//!
//! External environments follow a factory pattern:
//! 1. [`ExternalEnvFactory`] creates environment instances
//! 2. [`ExternalEnvs`] bundles Oracle implementations
//! 3. Individual oracle methods (e.g., [`OracleEnv::get_oracle_storage`]) provide data
//!
//! Block context is established at factory creation time, not per oracle call, ensuring
//! consistent state snapshots throughout execution.

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
/// This trait associates concrete Oracle types, allowing generic code to work
/// with any compatible environment configuration.
///
/// Note: SALT environment is now provided by the database itself through the
/// `MegaDatabase` trait, so only Oracle environment needs to be specified here.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvTypes {
    /// Oracle environment implementation for system contract storage queries.
    type OracleEnv: OracleEnv;
}

/// Tuple implementation for convenient pairing of Oracle environments.
/// Note: The first type parameter is kept for backward compatibility but is no longer used.
impl<A, B: OracleEnv> ExternalEnvTypes for (A, B) {
    type OracleEnv = B;
}

/// Bundle of external environment instances for a specific execution context.
///
/// This struct holds concrete Oracle implementation that is used during
/// EVM execution. Typically created by [`ExternalEnvFactory::external_envs`] at the
/// start of block processing.
///
/// Note: SALT environment is now provided by the database itself through the
/// `MegaDatabase` trait.
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
///
/// Note: SALT functionality is now provided by `EmptyMegaDB` which implements `MegaDatabase`.
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

/// Wrapper around `EmptyDB` that implements `MegaDatabase`.
///
/// This type is used as the default database for `MegaContext` when no specific
/// database is provided. It implements both `Database` and `SaltEnv` traits,
/// satisfying the `MegaDatabase` requirement.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyMegaDB(revm::database::EmptyDB);

impl revm::Database for EmptyMegaDB {
    type Error = core::convert::Infallible;

    fn basic(
        &mut self,
        address: alloy_primitives::Address,
    ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        self.0.basic(address)
    }

    fn code_by_hash(
        &mut self,
        code_hash: alloy_primitives::B256,
    ) -> Result<revm::state::Bytecode, Self::Error> {
        self.0.code_by_hash(code_hash)
    }

    fn storage(
        &mut self,
        address: alloy_primitives::Address,
        index: alloy_primitives::U256,
    ) -> Result<alloy_primitives::U256, Self::Error> {
        self.0.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<alloy_primitives::B256, Self::Error> {
        self.0.block_hash(number)
    }
}

impl SaltEnv for EmptyMegaDB {
    type Error = core::convert::Infallible;

    fn get_bucket_capacity(&self, _bucket_id: BucketId) -> Result<u64, Self::Error> {
        Ok(MIN_BUCKET_SIZE as u64)
    }
}
