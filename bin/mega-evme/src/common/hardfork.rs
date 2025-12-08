use mega_evm::{
    alloy_hardforks::{EthereumHardfork, ForkCondition},
    alloy_op_hardforks::{EthereumHardforks, OpHardfork, OpHardforks},
    MegaHardfork, MegaHardforks, MegaSpecId,
};

/// Fixed hardfork configuration for replay
#[derive(Debug, Clone, Copy)]
pub struct FixedHardfork {
    spec: MegaSpecId,
}

impl FixedHardfork {
    /// Create a new [`FixedHardfork`] with the given `spec`
    pub fn new(spec: MegaSpecId) -> Self {
        Self { spec }
    }
}

impl EthereumHardforks for FixedHardfork {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        if fork <= EthereumHardfork::Prague {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}

impl OpHardforks for FixedHardfork {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        if fork <= OpHardfork::Isthmus {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}

impl MegaHardforks for FixedHardfork {
    fn mega_fork_activation(&self, fork: MegaHardfork) -> ForkCondition {
        let mapped_spec = fork.spec_id();
        if mapped_spec <= self.spec {
            ForkCondition::Timestamp(0)
        } else {
            ForkCondition::Never
        }
    }
}
