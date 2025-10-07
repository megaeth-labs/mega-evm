//! External environment oracles for the EVM.

use core::{convert::Infallible, fmt::Debug};
use std::sync::Arc;

use auto_impl::auto_impl;

mod salt;
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

    /// Returns the SALT environment.
    fn salt_env(&self) -> Self::SaltEnv;
}

/// Default implementation of [`ExternalEnvs`] that provides no-op implementations for all
/// external environments.
///
/// This is useful when the EVM does not need to access any additional information from an
/// external environment.
#[derive(derive_more::Debug)]
pub struct DefaultExternalEnvs<Error = Infallible> {
    #[debug(ignore)]
    _phantom: core::marker::PhantomData<Error>,
}

impl<Error> Clone for DefaultExternalEnvs<Error> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Error> Copy for DefaultExternalEnvs<Error> {}

impl Default for DefaultExternalEnvs {
    fn default() -> Self {
        Self::new()
    }
}

impl<Error: Unpin + Send + Sync + 'static> DefaultExternalEnvs<Error> {
    /// Creates a new [`DefaultExternalEnvs`].
    pub fn new() -> Self {
        Self { _phantom: core::marker::PhantomData }
    }

    /// Consumes and wraps `self` into an Arc-wrapped boxed instance of the [`ExternalEnvs`]
    /// trait.
    pub fn boxed_arc(self) -> Arc<Box<dyn ExternalEnvs<SaltEnv = Self>>> {
        Arc::new(self.boxed())
    }

    /// Consumes and wraps `self` into a boxed instance of the [`ExternalEnvs`] trait.
    pub fn boxed(self) -> Box<dyn ExternalEnvs<SaltEnv = Self>> {
        Box::new(self)
    }
}

impl<Error: Unpin + Send + Sync + 'static> ExternalEnvs for DefaultExternalEnvs<Error> {
    type SaltEnv = Self;

    fn salt_env(&self) -> Self::SaltEnv {
        *self
    }
}

/// Type alias for backwards compatibility.
#[deprecated(note = "Use `DefaultExternalEnvs` instead")]
pub type NoOpOracle<Error = Infallible> = DefaultExternalEnvs<Error>;
