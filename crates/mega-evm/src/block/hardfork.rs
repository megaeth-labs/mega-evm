use alloy_hardforks::{hardfork, EthereumHardfork, EthereumHardforks, ForkCondition, Hardfork};
use alloy_op_hardforks::{OpHardfork, OpHardforks};
use alloy_primitives::{BlockNumber, BlockTimestamp, U256};
use auto_impl::auto_impl;

use crate::MegaSpecId;

hardfork! {
    /// The name of MegaETH hardforks. It is expected to mix with [`EthereumHardfork`] and
    /// [`OpHardfork`].
    #[derive(serde::Serialize, serde::Deserialize)]
    MegaHardfork {
        /// The first hardfork.
        MiniRex,
        /// The second hardfork.
        Rex,
    }
}

impl MegaHardfork {
    /// Gets the `MegaSpecId` associated with this hardfork.
    pub fn spec_id(&self) -> MegaSpecId {
        match self {
            MegaHardfork::MiniRex => MegaSpecId::MINI_REX,
            MegaHardfork::Rex => MegaSpecId::REX,
        }
    }
}

/// Extends [`OpHardforks`] with MegaETH helper methods.
#[auto_impl(&, Box, Arc)]
pub trait MegaHardforks: OpHardforks {
    /// Retrieves [`ForkCondition`] by a [`MegaethHardfork`]. If `fork` is not present, returns
    /// [`ForkCondition::Never`].
    fn megaeth_fork_activation(&self, fork: MegaHardfork) -> ForkCondition;

    /// Returns `true` if the given [`MegaethHardfork`] is the hardfork to be activated at the
    /// given timestamp. One special case is that if the current block is the first block of the
    /// chain and it activates the hardfork, we should return `true`.
    ///
    /// If the block is the first block of the hardfork, some hardfork
    /// initialization logic should be applied. This helper method is used for this purpose.
    fn first_hardfork_block(
        &self,
        fork: MegaHardfork,
        parent_timestamp: BlockTimestamp,
        current_number_and_timestamp: (BlockNumber, BlockTimestamp),
    ) -> bool {
        let (current_number, current_timestamp) = current_number_and_timestamp;
        self.megaeth_fork_activation(fork).active_at_timestamp(current_timestamp) &&
            (current_number == 1 ||
                !self.megaeth_fork_activation(fork).active_at_timestamp(parent_timestamp))
    }

    /// Gets the expected `MegaSpecId` for a block with the given timestamp.
    fn spec_id(&self, timestamp: BlockTimestamp) -> MegaSpecId {
        // Newer hardforks should be checked first
        if self.is_rex_active_at_timestamp(timestamp) {
            MegaSpecId::REX
        } else if self.is_mini_rex_active_at_timestamp(timestamp) {
            MegaSpecId::MINI_REX
        } else {
            MegaSpecId::EQUIVALENCE
        }
    }

    /// Returns `true` if [`MegaethHardfork::MiniRex`] is active at given block timestamp.
    fn is_mini_rex_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.megaeth_fork_activation(MegaHardfork::MiniRex).active_at_timestamp(timestamp)
    }

    /// Returns `true` if [`MegaethHardfork::Rex`] is active at given block timestamp.
    fn is_rex_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.megaeth_fork_activation(MegaHardfork::Rex).active_at_timestamp(timestamp)
    }
}

/// Configuration of the hardforks for MegaETH. It by default includes no `MegaHardfork` but
/// includes all hardforks before and including Optimism Isthmus. Optimism Isthmus is the hardfork
/// where MegaETH is established.
#[derive(Debug, Clone)]
pub struct MegaHardforkConfig {
    hardforks: Vec<(Box<dyn Hardfork>, ForkCondition)>,
}

impl Default for MegaHardforkConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, H> From<I> for MegaHardforkConfig
where
    I: Iterator<Item = (H, ForkCondition)>,
    H: Hardfork + 'static,
{
    fn from(iter: I) -> Self {
        Self { hardforks: iter.map(|(h, c)| (Box::new(h) as Box<dyn Hardfork>, c)).collect() }
    }
}

impl MegaHardforkConfig {
    /// Creates a new hardfork configuration with the default hardforks, i.e., all hardforks before
    /// and including Optimism Isthmus are enabled. Optimism Isthmus is the hardfork where
    /// MegaETH is established.
    pub fn new() -> Self {
        Self {
            hardforks: vec![
                (EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Dao.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)),
                (EthereumHardfork::London.boxed(), ForkCondition::Block(0)),
                (
                    EthereumHardfork::Paris.boxed(),
                    ForkCondition::TTD {
                        activation_block_number: 0,
                        fork_block: None,
                        total_difficulty: U256::ZERO,
                    },
                ),
                (OpHardfork::Bedrock.boxed(), ForkCondition::Block(0)),
                (OpHardfork::Regolith.boxed(), ForkCondition::Timestamp(0)),
                (EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Canyon.boxed(), ForkCondition::Timestamp(0)),
                (EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Ecotone.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Fjord.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Granite.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Holocene.boxed(), ForkCondition::Timestamp(0)),
                (EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(0)),
                (OpHardfork::Isthmus.boxed(), ForkCondition::Timestamp(0)),
            ],
        }
    }

    /// Creates a new hardfork configuration with the given hardfork and condition.
    pub fn with(mut self, hardfork: impl Hardfork, condition: ForkCondition) -> Self {
        self.insert(hardfork, condition);
        self
    }

    /// Inserts a new hardfork into the configuration. If the hardfork is already present, it will
    /// be overwritten.
    pub fn insert(&mut self, hardfork: impl Hardfork, condition: ForkCondition) {
        let index = self.hardforks.iter().position(|(h, _)| h.name() == hardfork.name());
        if let Some(index) = index {
            self.hardforks[index] = (Box::new(hardfork), condition);
        } else {
            self.hardforks.push((Box::new(hardfork), condition));
        }
    }

    /// Gets `ForkCondition` by a [`Hardfork`]. If the hardfork is not present, returns `None`.
    pub fn get(&self, hardfork: impl Hardfork) -> Option<&ForkCondition> {
        self.hardforks
            .iter()
            .find(|(h, _)| h.name() == hardfork.name())
            .map(|(_, condition)| condition)
    }
}

impl EthereumHardforks for MegaHardforkConfig {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }
}

impl OpHardforks for MegaHardforkConfig {
    fn op_fork_activation(&self, fork: OpHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }
}

impl MegaHardforks for MegaHardforkConfig {
    fn megaeth_fork_activation(&self, fork: MegaHardfork) -> ForkCondition {
        match self.get(fork) {
            Some(condition) => *condition,
            None => ForkCondition::Never,
        }
    }
}
