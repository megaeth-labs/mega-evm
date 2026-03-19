use auto_impl::auto_impl;

use crate::{ExternalEnvTypes, ExternalEnvs};

/// Factory for creating external environment instances.
///
/// Produces [`ExternalEnvs`] containing SALT and Oracle environments for EVM execution.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvFactory {
    /// The concrete types for SALT and Oracle environments this factory produces.
    type EnvTypes: ExternalEnvTypes;

    /// Creates external environment instances for executing EVM operations.
    fn external_envs(&self) -> ExternalEnvs<Self::EnvTypes>;
}
