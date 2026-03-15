use auto_impl::auto_impl;

use crate::{ExternalEnvTypes, ExternalEnvs};

/// Factory for creating external environment instances.
///
/// This trait is responsible for instantiating external oracles,
/// ensuring all oracle queries during EVM execution operate on a consistent snapshot of state.
///
/// # Design Pattern
///
/// External environments (Oracle) do not take block parameters in their methods.
/// Instead, the factory creates instances that encapsulate the necessary context,
/// allowing implementations to:
/// - Read state from the appropriate source
/// - Cache data for the execution lifetime
/// - Ensure deterministic behavior across repeated executions
///
/// # Usage
///
/// This factory is typically called when initializing the EVM. The returned
/// [`ExternalEnvs`] are then used throughout transaction execution.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvFactory {
    /// The concrete types for Oracle environments this factory produces.
    type EnvTypes: ExternalEnvTypes;

    /// Creates external environment instances for executing EVM operations.
    ///
    /// # Returns
    ///
    /// A container with Oracle environment instances.
    fn external_envs(&self) -> ExternalEnvs<Self::EnvTypes>;
}
