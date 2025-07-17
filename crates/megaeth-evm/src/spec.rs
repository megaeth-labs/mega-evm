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
    BURROW,
}

/// String identifiers for Optimism hardforks
#[allow(missing_docs)]
pub mod name {
    pub const EQUIVALENCE: &str = "Equivalence";
    pub const BURROW: &str = "Burrow";
}

impl MegaethSpecId {
    /// Converts the [`MegaethSpecId`] into a [`SpecId`].
    pub const fn into_eth_spec(self) -> SpecId {
        match self {
            Self::EQUIVALENCE => todo!(),
            Self::BURROW => todo!(),
        }
    }

    /// Converts the [`MegaethSpecId`] into a [`OpSpecId`].
    pub const fn into_op_spec(self) -> OpSpecId {
        match self {
            Self::EQUIVALENCE => OpSpecId::GRANITE,
            Self::BURROW => OpSpecId::ISTHMUS,
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
            MegaethSpecId::BURROW => name::BURROW,
        }
    }
}

impl FromStr for MegaethSpecId {
    type Err = UnknownHardfork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            name::EQUIVALENCE => Ok(Self::EQUIVALENCE),
            name::BURROW => Ok(Self::BURROW),
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
