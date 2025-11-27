use alloy_primitives::BlockNumber;
use auto_impl::auto_impl;

use crate::{ExternalEnvTypes, ExternalEnvs};

/// Factory for creating block-specific external environment instances.
///
/// This trait is responsible for instantiating external oracles at a specific block height,
/// ensuring all oracle queries during EVM execution operate on a consistent snapshot of state.
///
/// # Design Pattern
///
/// External environments (SALT and Oracle) do not take block parameters in their methods.
/// Instead, the factory creates block-aware instances that encapsulate the block context,
/// allowing implementations to:
/// - Read state from the appropriate block height
/// - Cache block-specific data for the execution lifetime
/// - Ensure deterministic behavior across repeated executions
///
/// # Usage
///
/// This factory is typically called once per block when initializing the EVM. The returned
/// [`ExternalEnvs`] are then used throughout transaction execution within that block.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvFactory {
    /// The concrete types for SALT and Oracle environments this factory produces.
    type EnvTypes: ExternalEnvTypes;

    /// Creates external environment instances for executing EVM operations at the specified block.
    ///
    /// # Arguments
    ///
    /// * `block` - The block number at which EVM execution will occur. This is the block height
    ///   specified in [`BlockEnv`](revm::context::BlockEnv), and oracle queries should read
    ///   state from the parent block (block - 1).
    ///
    /// # Returns
    ///
    /// A container with SALT and Oracle environment instances configured for the given block.
    fn external_envs(&self, block: BlockNumber) -> ExternalEnvs<Self::EnvTypes>;
}
