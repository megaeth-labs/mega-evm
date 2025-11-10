//! Definitions of the `MegaETH` EVM versions (`SpecId`).

use clap::ValueEnum;
use core::{
    fmt::{self, Display},
    str::FromStr,
};
pub use op_revm::OpSpecId;
pub use revm::primitives::hardfork::{SpecId as EthSpecId, UnknownHardfork};
use serde::{Deserialize, Serialize};

/// `MegaETH` spec id, defining different versions of the `MegaETH` EVM.
///
/// Each `MegaETH` EVM version corresponds to a version of the Optimism EVM, which means the
/// behavior of the `MegaETH` EVM inherits and is customized on top of that version of the Optimism
/// EVM. Similarly, each Optimism EVM version also corresponds to a Ethereum EVM version. The
/// corresponding relations are as follows:
/// - [`SpecId::EQUIVALENCE`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::MINI_REX`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    ValueEnum,
)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms, missing_docs)]
pub enum MegaSpecId {
    /// The EVM version when no `MegaETH` harfork is enabled. The behavior of the EVM
    /// should be equivalent to the [`OpSpecId::ISTHMUS`] of the Optimism EVM.
    #[default]
    #[value(name = "Equivalence")]
    EQUIVALENCE,
    /// The EVM version for the *Mini-Rex* hardfork of `MegaETH`.
    #[value(name = "MiniRex")]
    MINI_REX,
}

/// String identifiers for `MegaETH` EVM versions.
#[allow(missing_docs)]
pub mod name {
    /// The string identifier for the *Equivalence* version of the `MegaETH` EVM.
    pub const EQUIVALENCE: &str = "Equivalence";
    /// The string identifier for the *Mini-Rex* version of the `MegaETH` EVM.
    pub const MINI_REX: &str = "MiniRex";
}

impl MegaSpecId {
    /// Converts the [`SpecId`] into its corresponding [`EthSpecId`].
    pub const fn into_eth_spec(self) -> EthSpecId {
        self.into_op_spec().into_eth_spec()
    }

    /// Converts the [`SpecId`] into its corresponding [`OpSpecId`].
    pub const fn into_op_spec(self) -> OpSpecId {
        match self {
            Self::MINI_REX | Self::EQUIVALENCE => OpSpecId::ISTHMUS,
        }
    }

    /// Checks if one given [`SpecId`] is enabled in the current [`SpecId`].
    ///
    /// Evm versions are backward compatible, so a higher version is always enabled in a lower
    /// version.
    pub const fn is_enabled(self, other: Self) -> bool {
        other as u8 <= self as u8
    }
}

impl From<MegaSpecId> for &'static str {
    /// Converts the [`SpecId`] into its corresponding string identifier.
    fn from(spec_id: MegaSpecId) -> Self {
        match spec_id {
            MegaSpecId::EQUIVALENCE => name::EQUIVALENCE,
            MegaSpecId::MINI_REX => name::MINI_REX,
        }
    }
}

impl FromStr for MegaSpecId {
    type Err = UnknownHardfork;

    /// Converts the string identifier into its corresponding [`SpecId`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            name::EQUIVALENCE => Ok(Self::EQUIVALENCE),
            name::MINI_REX => Ok(Self::MINI_REX),
            _ => Err(UnknownHardfork),
        }
    }
}

impl From<MegaSpecId> for revm::primitives::hardfork::SpecId {
    /// Converts the [`SpecId`] into its corresponding [`EthSpecId`].
    fn from(spec_id: MegaSpecId) -> Self {
        spec_id.into_eth_spec()
    }
}

impl From<MegaSpecId> for OpSpecId {
    /// Converts the [`SpecId`] into its corresponding [`OpSpecId`].
    fn from(spec_id: MegaSpecId) -> Self {
        spec_id.into_op_spec()
    }
}

impl Display for MegaSpecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: &'static str = (*self).into();
        write!(f, "{}", s)
    }
}
