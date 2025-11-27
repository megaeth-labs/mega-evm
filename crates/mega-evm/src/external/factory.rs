use alloy_primitives::BlockNumber;
use auto_impl::auto_impl;

use crate::{ExternalEnvTypes, ExternalEnvs};

/// A factory for creating external environments at a specific block.
#[auto_impl(&, Box, Arc)]
pub trait ExternalEnvFactory {
    /// The external environment types.
    type EnvTypes: ExternalEnvTypes;

    /// Creates new external environments for a specific block. The given block is the block that
    /// the EVM will execute on (i.e., the block specified in the `BlockEnv`).
    fn external_envs(&self, block: BlockNumber) -> ExternalEnvs<Self::EnvTypes>;
}
