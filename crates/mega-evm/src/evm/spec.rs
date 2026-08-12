//! Definitions of the `MegaETH` EVM versions (`SpecId`).

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
/// - [`SpecId::REX`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::REX1`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::REX2`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::REX3`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::REX4`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::REX5`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::REX6`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::REX7`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
///
/// The `Default` variant tracks the latest spec, which may still be unstable;
/// callers that need a stable spec must select it explicitly instead of
/// relying on `Default::default()`.
#[repr(u8)]
#[derive(
    Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms, missing_docs)]
#[non_exhaustive]
pub enum MegaSpecId {
    /// The EVM version when no `MegaETH` harfork is enabled. The behavior of the EVM
    /// should be equivalent to the [`OpSpecId::ISTHMUS`] of the Optimism EVM.
    EQUIVALENCE,
    /// The EVM version for the *Mini-Rex* hardfork of `MegaETH`.
    MINI_REX,
    /// The EVM version for the *Rex* hardfork of `MegaETH`.
    REX,
    /// The EVM version for the *Rex1* hardfork of `MegaETH`.
    REX1,
    /// The EVM version for the *Rex2* hardfork of `MegaETH`.
    REX2,
    /// The EVM version for the *Rex3* hardfork of `MegaETH`.
    REX3,
    /// The EVM version for the *Rex4* hardfork of `MegaETH`.
    REX4,
    /// The EVM version for the *Rex5* hardfork of `MegaETH`.
    REX5,
    /// The EVM version for the *Rex6* hardfork of `MegaETH`.
    REX6,
    /// The EVM version for the *Rex7* hardfork of `MegaETH`.
    #[default]
    REX7,
}

/// String identifiers for `MegaETH` EVM versions.
#[allow(missing_docs)]
pub mod name {
    /// The string identifier for the *Equivalence* version of the `MegaETH` EVM.
    pub const EQUIVALENCE: &str = "Equivalence";
    /// The string identifier for the *Mini-Rex* version of the `MegaETH` EVM.
    pub const MINI_REX: &str = "MiniRex";
    /// The string identifier for the *Rex* version of the `MegaETH` EVM.
    pub const REX: &str = "Rex";
    /// The string identifier for the *Rex1* version of the `MegaETH` EVM.
    pub const REX1: &str = "Rex1";
    /// The string identifier for the *Rex2* version of the `MegaETH` EVM.
    pub const REX2: &str = "Rex2";
    /// The string identifier for the *Rex3* version of the `MegaETH` EVM.
    pub const REX3: &str = "Rex3";
    /// The string identifier for the *Rex4* version of the `MegaETH` EVM.
    pub const REX4: &str = "Rex4";
    /// The string identifier for the *Rex5* version of the `MegaETH` EVM.
    pub const REX5: &str = "Rex5";
    /// The string identifier for the *Rex6* version of the `MegaETH` EVM.
    pub const REX6: &str = "Rex6";
    /// The string identifier for the *Rex7* version of the `MegaETH` EVM.
    pub const REX7: &str = "Rex7";
}

impl MegaSpecId {
    /// Converts the [`SpecId`] into its corresponding [`EthSpecId`].
    pub const fn into_eth_spec(self) -> EthSpecId {
        self.into_op_spec().into_eth_spec()
    }

    /// Converts the [`SpecId`] into its corresponding [`OpSpecId`].
    pub const fn into_op_spec(self) -> OpSpecId {
        match self {
            Self::MINI_REX |
            Self::EQUIVALENCE |
            Self::REX |
            Self::REX1 |
            Self::REX2 |
            Self::REX3 |
            Self::REX4 |
            Self::REX5 |
            Self::REX6 |
            Self::REX7 => OpSpecId::ISTHMUS,
        }
    }

    /// Returns `true` if `other` is enabled under `self` — i.e. `other` is at or below `self`
    /// in [`SpecId`] order.
    ///
    /// Evm versions are backward compatible: the current spec (`self`) enables every version at
    /// or below it, so a lower-or-equal version is always enabled under a higher one.
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
            MegaSpecId::REX => name::REX,
            MegaSpecId::REX1 => name::REX1,
            MegaSpecId::REX2 => name::REX2,
            MegaSpecId::REX3 => name::REX3,
            MegaSpecId::REX4 => name::REX4,
            MegaSpecId::REX5 => name::REX5,
            MegaSpecId::REX6 => name::REX6,
            MegaSpecId::REX7 => name::REX7,
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
            name::REX => Ok(Self::REX),
            name::REX1 => Ok(Self::REX1),
            name::REX2 => Ok(Self::REX2),
            name::REX3 => Ok(Self::REX3),
            name::REX4 => Ok(Self::REX4),
            name::REX5 => Ok(Self::REX5),
            name::REX6 => Ok(Self::REX6),
            name::REX7 => Ok(Self::REX7),
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

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_SPECS: [(MegaSpecId, &str); 10] = [
        (MegaSpecId::EQUIVALENCE, name::EQUIVALENCE),
        (MegaSpecId::MINI_REX, name::MINI_REX),
        (MegaSpecId::REX, name::REX),
        (MegaSpecId::REX1, name::REX1),
        (MegaSpecId::REX2, name::REX2),
        (MegaSpecId::REX3, name::REX3),
        (MegaSpecId::REX4, name::REX4),
        (MegaSpecId::REX5, name::REX5),
        (MegaSpecId::REX6, name::REX6),
        (MegaSpecId::REX7, name::REX7),
    ];

    #[test]
    fn test_spec_names_roundtrip_and_display() {
        for (spec, expected_name) in ALL_SPECS {
            assert_eq!(<&'static str>::from(spec), expected_name);
            assert_eq!(MegaSpecId::from_str(expected_name).unwrap(), spec);
            assert_eq!(spec.to_string(), expected_name);
        }

        assert_eq!(MegaSpecId::default(), MegaSpecId::REX7);
        assert_eq!(MegaSpecId::from_str("unknown"), Err(UnknownHardfork));
    }

    /// Behavior elsewhere is written against this mapping, so advancing it is not a one-line
    /// change. What breaks, and where to look:
    ///
    /// - Amsterdam turns on EIP-7708, which emits logs outside every frame checkpoint. The
    ///   transaction result seam drops logs from non-`Success` results unconditionally on the
    ///   strength of that never happening, and would start swallowing legitimate ones.
    /// - Osaka caps a transaction's gas limit at 2^24, below `MegaETH`'s own system-call and
    ///   compute-gas figures, so every system call and many user transactions would be rejected
    ///   during validation unless the cap is pinned first.
    ///
    /// Neither is a reason not to advance the mapping — they are the work that comes with it.
    #[test]
    fn test_all_specs_map_to_isthmus_and_prague() {
        for (spec, _) in ALL_SPECS {
            assert_eq!(spec.into_op_spec(), OpSpecId::ISTHMUS);
            assert_eq!(spec.into_eth_spec(), EthSpecId::PRAGUE);
            assert_eq!(revm::primitives::hardfork::SpecId::from(spec), EthSpecId::PRAGUE);
            assert_eq!(OpSpecId::from(spec), OpSpecId::ISTHMUS);
        }
    }

    #[test]
    fn test_spec_order_is_backward_compatible() {
        assert!(MegaSpecId::REX4.is_enabled(MegaSpecId::REX3));
        assert!(MegaSpecId::REX4.is_enabled(MegaSpecId::EQUIVALENCE));
        assert!(MegaSpecId::MINI_REX.is_enabled(MegaSpecId::EQUIVALENCE));
        assert!(MegaSpecId::REX2.is_enabled(MegaSpecId::REX1));
        assert!(MegaSpecId::REX5.is_enabled(MegaSpecId::REX4));
        assert!(MegaSpecId::REX5.is_enabled(MegaSpecId::EQUIVALENCE));
        assert!(MegaSpecId::REX6.is_enabled(MegaSpecId::REX5));
        assert!(MegaSpecId::REX6.is_enabled(MegaSpecId::EQUIVALENCE));
        assert!(MegaSpecId::REX7.is_enabled(MegaSpecId::REX6));
        assert!(MegaSpecId::REX7.is_enabled(MegaSpecId::EQUIVALENCE));

        assert!(!MegaSpecId::EQUIVALENCE.is_enabled(MegaSpecId::MINI_REX));
        assert!(!MegaSpecId::REX1.is_enabled(MegaSpecId::REX2));
        assert!(!MegaSpecId::REX3.is_enabled(MegaSpecId::REX4));
        assert!(!MegaSpecId::REX4.is_enabled(MegaSpecId::REX5));
        assert!(!MegaSpecId::REX5.is_enabled(MegaSpecId::REX6));
        assert!(!MegaSpecId::REX6.is_enabled(MegaSpecId::REX7));
    }
}
