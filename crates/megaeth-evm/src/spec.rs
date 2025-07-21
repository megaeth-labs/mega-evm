use op_revm::OpSpecId;
use revm::primitives::hardfork::{SpecId, UnknownHardfork};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// `MegaETH` spec id type.
#[repr(u8)]
#[derive(
    Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms, missing_docs)]
pub enum MegaethSpecId {
    /// This is the spec of `MegaethEvm` when no harfork is enabled.
    #[default]
    EQUIVALENCE,
    MINI_RAX,
}

/// Constants for the `MegaethSpecId` spec.
pub mod constants {
    /// Constants for the `MINI_RAX` spec.
    pub mod mini_rax {
        /// The maximum contract size for the `MINI_RAX` spec.
        pub const MAX_CONTRACT_SIZE: usize = 512 * 1024;
        /// The additional initcode size for the `MINI_RAX` spec. The initcode size is limited to
        /// `MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE`.
        pub const ADDITIONAL_INITCODE_SIZE: usize = 24 * 1024;
        /// The maximum initcode size for the `MINI_RAX` spec.
        pub const MAX_INITCODE_SIZE: usize = MAX_CONTRACT_SIZE + ADDITIONAL_INITCODE_SIZE;
        /// The cost of a log topic for the `MINI_RAX` spec.
        pub const LOG_TOPIC_COST: u64 = 10000;
    }
}

/// String identifiers for Optimism hardforks
#[allow(missing_docs)]
pub mod name {
    pub const EQUIVALENCE: &str = "Equivalence";
    pub const MINI_RAX: &str = "MiniRax";
}

impl MegaethSpecId {
    /// Converts the [`MegaethSpecId`] into a [`SpecId`].
    pub const fn into_eth_spec(self) -> SpecId {
        self.into_op_spec().into_eth_spec()
    }

    /// Converts the [`MegaethSpecId`] into a [`OpSpecId`].
    pub const fn into_op_spec(self) -> OpSpecId {
        match self {
            Self::EQUIVALENCE => OpSpecId::GRANITE,
            Self::MINI_RAX => OpSpecId::ISTHMUS,
        }
    }

    /// Checks if one [`MegaethSpecId`] is enabled in another.
    ///
    /// Evm versions are backward compatible, so a higher version is always enabled in a lower
    /// version.
    pub const fn is_enabled_in(self, other: Self) -> bool {
        other as u8 <= self as u8
    }
}

impl From<MegaethSpecId> for &'static str {
    fn from(spec_id: MegaethSpecId) -> Self {
        match spec_id {
            MegaethSpecId::EQUIVALENCE => name::EQUIVALENCE,
            MegaethSpecId::MINI_RAX => name::MINI_RAX,
        }
    }
}

impl FromStr for MegaethSpecId {
    type Err = UnknownHardfork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            name::EQUIVALENCE => Ok(Self::EQUIVALENCE),
            name::MINI_RAX => Ok(Self::MINI_RAX),
            _ => Err(UnknownHardfork),
        }
    }
}

impl From<MegaethSpecId> for SpecId {
    fn from(spec_id: MegaethSpecId) -> Self {
        spec_id.into_eth_spec()
    }
}

impl From<MegaethSpecId> for OpSpecId {
    fn from(spec_id: MegaethSpecId) -> Self {
        spec_id.into_op_spec()
    }
}
