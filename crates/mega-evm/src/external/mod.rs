//! External environment oracles for the EVM.

use core::{convert::Infallible, fmt::Debug};
use std::sync::{Arc, RwLock};

use alloy_primitives::{BlockNumber, U256};
use auto_impl::auto_impl;
use revm::primitives::HashMap;

mod gas;
mod oracle;
mod salt;

pub use gas::*;
pub use oracle::*;
pub use salt::*;

/// A collection trait that aggregates all external environments needed by the EVM.
///
/// This trait provides a unified interface to access different external environments,
/// such as SALT bucket capacity information and potentially other external data sources in the
/// future. By using this umbrella trait, the EVM can access multiple external data sources
/// through a single type parameter without needing additional generic parameters.
///
/// # Design
///
/// Each external environment concern is exposed through an accessor method that returns a
/// reference to the specific external environment trait (e.g., [`SaltEnv`]). This allows:
/// - Independent error types for each external environment
/// - Easy addition of new external environment types without breaking existing code
/// - Clear separation of concerns
///
/// # Example
///
/// ```rust,ignore
/// // Future extensibility example:
/// trait ExternalEnvs {
///     type SaltEnv: SaltEnv;
///     type OtherEnv: OtherEnv;
///
///     fn salt_env(&self) -> Self::SaltEnv;
///     fn other_env(&self) -> Self::OtherEnv;
/// }
/// ```
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvs: Debug + Send + Sync + Unpin {
    /// The SALT environment type.
    type SaltEnv: SaltEnv;
    /// The Oracle environment type.
    type OracleEnv: OracleEnv;

    /// Returns the SALT environment.
    fn salt_env(&self) -> Self::SaltEnv;
    /// Returns the Oracle environment.
    fn oracle_env(&self) -> Self::OracleEnv;
}

/// Default implementation of [`ExternalEnvs`] that provides no-op implementations for all
/// external environments.
///
/// This is useful when the EVM does not need to access any additional information from an
/// external environment.
#[derive(derive_more::Debug, Clone)]
pub struct DefaultExternalEnvs<Error = Infallible> {
    #[debug(ignore)]
    _phantom: core::marker::PhantomData<Error>,
    /// Oracle storage for testing purposes. Maps storage slots to their values.
    oracle_storage: Arc<RwLock<HashMap<U256, U256>>>,
    /// Bucket capacity storage for testing purposes. Maps (`bucket_id`, `block_number`) to
    /// capacity.
    bucket_capacity: Arc<RwLock<HashMap<(BucketId, BlockNumber), u64>>>,
}

impl Default for DefaultExternalEnvs {
    fn default() -> Self {
        Self::new()
    }
}

impl<Error: Unpin + Send + Sync + Clone + 'static> DefaultExternalEnvs<Error> {
    /// Creates a new [`DefaultExternalEnvs`].
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
            oracle_storage: Arc::new(RwLock::new(HashMap::default())),
            bucket_capacity: Arc::new(RwLock::new(HashMap::default())),
        }
    }

    /// Consumes and wraps `self` into an Arc-wrapped boxed instance of the [`ExternalEnvs`]
    /// trait.
    pub fn boxed_arc(self) -> Arc<Box<dyn ExternalEnvs<SaltEnv = Self, OracleEnv = Self>>> {
        Arc::new(self.boxed())
    }

    /// Consumes and wraps `self` into a boxed instance of the [`ExternalEnvs`] trait.
    pub fn boxed(self) -> Box<dyn ExternalEnvs<SaltEnv = Self, OracleEnv = Self>> {
        Box::new(self)
    }
}

impl<Error: Unpin + Send + Sync + Clone + 'static> ExternalEnvs for DefaultExternalEnvs<Error> {
    type SaltEnv = Self;
    type OracleEnv = Self;

    fn salt_env(&self) -> Self::SaltEnv {
        self.clone()
    }

    fn oracle_env(&self) -> Self::OracleEnv {
        self.clone()
    }
}

/// Type alias for backwards compatibility.
#[deprecated(note = "Use `DefaultExternalEnvs` instead")]
pub type NoOpOracle<Error = Infallible> = DefaultExternalEnvs<Error>;
