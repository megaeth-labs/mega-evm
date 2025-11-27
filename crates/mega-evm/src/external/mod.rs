//! External environment oracles for the EVM.

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

/// A trait for external environment types.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvTypes {
    /// The SALT environment type.
    type SaltEnv: SaltEnv;
    /// The oracle environment type.
    type OracleEnv: OracleEnv;
}

/// Container for external environment implementations.
#[derive(Debug, Clone)]
pub struct ExternalEnvs<T: ExternalEnvTypes> {
    /// The SALT environment implementation.
    pub salt_env: T::SaltEnv,
    /// The oracle environment implementation.
    pub oracle_env: T::OracleEnv,
}

impl Default for ExternalEnvs<EmptyExternalEnv> {
    fn default() -> Self {
        Self { salt_env: EmptyExternalEnv, oracle_env: EmptyExternalEnv }
    }
}

/// An empty external environment that provides no-op implementations for all external environments.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmptyExternalEnv;

impl ExternalEnvTypes for EmptyExternalEnv {
    type SaltEnv = Self;
    type OracleEnv = Self;
}
