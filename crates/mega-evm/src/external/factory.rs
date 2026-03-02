use auto_impl::auto_impl;

use crate::{ExternalEnvTypes, ExternalEnvs};

/// Factory for creating block-specific external environment instances.
///
/// This trait is responsible for instantiating external oracles,
/// ensuring all oracle queries during EVM execution operate on a consistent snapshot of state.
///
/// # Design Pattern
///
/// External environments (Oracle) do not take block parameters in their methods.
/// Instead, the factory creates instances that encapsulate the necessary context,
/// allowing implementations to:
/// - Read state from the appropriate block height (configured when creating the factory)
/// - Cache block-specific data for the execution lifetime
/// - Ensure deterministic behavior across repeated executions
///
/// Note: Block context should be established when creating the factory itself,
/// not when calling `external_envs()`. SALT environment is now provided by the
/// database itself through the `MegaDatabase` trait.
///
/// # Usage
///
/// This factory is typically called once per block when initializing the EVM. The returned
/// [`ExternalEnvs`] are then used throughout transaction execution within that block.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvFactory {
    /// The concrete types for Oracle environments this factory produces.
    type EnvTypes: ExternalEnvTypes;

    /// Creates external environment instances for executing EVM operations.
    ///
    /// # Returns
    ///
    /// A container with Oracle environment instance configured for the execution context.
    fn external_envs(&self) -> ExternalEnvs<Self::EnvTypes>;
}
