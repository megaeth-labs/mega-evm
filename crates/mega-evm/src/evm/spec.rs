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
/// - [`SpecId::MINI_REX_1`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
/// - [`SpecId::MINI_REX_2`] -> [`OpSpecId::ISTHMUS`] -> [`EthSpecId::PRAGUE`]
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
    /// Alias spec for the `MiniRex1` hardfork: scheduled as its own rung, executes
    /// [`MegaSpecId::EQUIVALENCE`] behavior (`behavior()` projects there).
    MINI_REX_1,
    /// Alias spec for the `MiniRex2` hardfork: scheduled as its own rung, executes
    /// [`MegaSpecId::MINI_REX`] behavior (`behavior()` projects there).
    MINI_REX_2,
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
    /// The string identifier for the `MiniRex1` alias spec.
    pub const MINI_REX_1: &str = "MiniRex1";
    /// The string identifier for the `MiniRex2` alias spec.
    pub const MINI_REX_2: &str = "MiniRex2";
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
    /// Every spec in the progression, oldest first.
    ///
    /// The single point to enumerate specs from: sweeps and tables that list specs by hand
    /// drift silently when a variant is added. Completeness is enforced in two steps —
    /// introducing a variant is a compile error in `ladder_index`'s exhaustive match (and the
    /// `is_ladder_prefix` const assertion ties each entry here to its ladder position), and
    /// `test_all_ends_at_the_latest_spec` fails until the new spec is appended here.
    pub const ALL: &'static [Self] = &[
        Self::EQUIVALENCE,
        Self::MINI_REX,
        Self::MINI_REX_1,
        Self::MINI_REX_2,
        Self::REX,
        Self::REX1,
        Self::REX2,
        Self::REX3,
        Self::REX4,
        Self::REX5,
        Self::REX6,
        Self::REX7,
    ];

    /// Converts the [`SpecId`] into its corresponding [`EthSpecId`].
    pub const fn into_eth_spec(self) -> EthSpecId {
        self.into_op_spec().into_eth_spec()
    }

    /// Converts the [`SpecId`] into its corresponding [`OpSpecId`].
    pub const fn into_op_spec(self) -> OpSpecId {
        match self {
            Self::MINI_REX |
            Self::MINI_REX_1 |
            Self::MINI_REX_2 |
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

    /// The behavior this spec executes: alias specs project to the spec whose behavior they
    /// reuse; every other spec is its own behavior.
    pub const fn behavior(self) -> Self {
        match self {
            Self::MINI_REX_1 => Self::EQUIVALENCE,
            Self::MINI_REX_2 => Self::MINI_REX,
            other => other,
        }
    }

    /// Whether this spec is an alias — a rung whose behavior belongs to another spec.
    /// Derived from [`behavior`](Self::behavior), so a future alias needs only its projection
    /// arm; there is no second list to extend.
    pub const fn is_alias(self) -> bool {
        self.behavior() as u8 != self as u8
    }

    /// Returns `true` if `other`'s BEHAVIOR is enabled under `self` — the gate for execution
    /// semantics. Both sides project through [`behavior`](Self::behavior) first, so an alias
    /// spec enables exactly what its behavior target enables (`MINI_REX_1` does NOT enable
    /// `MINI_REX`).
    pub const fn is_enabled(self, other: Self) -> bool {
        other.behavior() as u8 <= self.behavior() as u8
    }

    /// Returns `true` if the ladder has REACHED `other`'s rung — the gate for one-way chain
    /// setup. Position comparison, no behavior projection: during an alias window the ladder
    /// stands above the specs it rolled back from, so their setup stays in place.
    pub const fn reaches(self, other: Self) -> bool {
        other as u8 <= self as u8
    }
}

/// Position on the spec progression, 0 = `EQUIVALENCE`.
///
/// Not an API — the `u8` discriminant already carries the ordinal. This exists to be an
/// exhaustive match: introducing a spec fails compilation here until the variant is placed,
/// and the const assertion below fails until [`MegaSpecId::ALL`] lists it at that position.
const fn ladder_index(spec: MegaSpecId) -> usize {
    match spec {
        MegaSpecId::EQUIVALENCE => 0,
        MegaSpecId::MINI_REX => 1,
        MegaSpecId::MINI_REX_1 => 2,
        MegaSpecId::MINI_REX_2 => 3,
        MegaSpecId::REX => 4,
        MegaSpecId::REX1 => 5,
        MegaSpecId::REX2 => 6,
        MegaSpecId::REX3 => 7,
        MegaSpecId::REX4 => 8,
        MegaSpecId::REX5 => 9,
        MegaSpecId::REX6 => 10,
        MegaSpecId::REX7 => 11,
    }
}

/// Whether `list` is a prefix of the spec ladder: entry `i` is exactly the spec at ladder
/// position `i` — in order, without gaps, starting from `EQUIVALENCE`.
///
/// Shared by the compile-time assertion on [`MegaSpecId::ALL`] below and by the test that
/// feeds it malformed lists, so the checker itself is exercised — a weakened guard here would
/// otherwise pass silently, since the real `ALL` always satisfies the property it checks.
const fn is_ladder_prefix(list: &[MegaSpecId]) -> bool {
    let mut i = 0;
    while i < list.len() {
        if ladder_index(list[i]) != i {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = assert!(
    is_ladder_prefix(MegaSpecId::ALL),
    "MegaSpecId::ALL must list every spec in ladder order, without gaps"
);

impl From<MegaSpecId> for &'static str {
    /// Converts the [`SpecId`] into its corresponding string identifier.
    fn from(spec_id: MegaSpecId) -> Self {
        match spec_id {
            MegaSpecId::EQUIVALENCE => name::EQUIVALENCE,
            MegaSpecId::MINI_REX => name::MINI_REX,
            MegaSpecId::MINI_REX_1 => name::MINI_REX_1,
            MegaSpecId::MINI_REX_2 => name::MINI_REX_2,
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
            name::MINI_REX_1 => Ok(Self::MINI_REX_1),
            name::MINI_REX_2 => Ok(Self::MINI_REX_2),
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

    const ALL_SPECS: [(MegaSpecId, &str); 12] = [
        (MegaSpecId::EQUIVALENCE, name::EQUIVALENCE),
        (MegaSpecId::MINI_REX, name::MINI_REX),
        (MegaSpecId::MINI_REX_1, name::MINI_REX_1),
        (MegaSpecId::MINI_REX_2, name::MINI_REX_2),
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
        // The golden pairs stay hand-written — deriving the expected names from the code under
        // test would make the round-trip vacuous — but the spec column must be exactly
        // `MegaSpecId::ALL`, so a newly introduced spec cannot be forgotten here.
        assert!(ALL_SPECS.iter().map(|(spec, _)| *spec).eq(MegaSpecId::ALL.iter().copied()));

        for (spec, expected_name) in ALL_SPECS {
            assert_eq!(<&'static str>::from(spec), expected_name);
            assert_eq!(MegaSpecId::from_str(expected_name).unwrap(), spec);
            assert_eq!(spec.to_string(), expected_name);
        }

        assert_eq!(MegaSpecId::default(), MegaSpecId::REX7);
        assert_eq!(MegaSpecId::from_str("unknown"), Err(UnknownHardfork));
    }

    /// The completeness anchor for [`MegaSpecId::ALL`]: `Default` tracks the latest spec (its
    /// own assertion above pins which), so a variant added without extending `ALL` fails here
    /// once the default advances. The const assertion on `ladder_index` covers order and gaps;
    /// this covers the tail.
    #[test]
    fn test_all_ends_at_the_latest_spec() {
        assert_eq!(*MegaSpecId::ALL.last().unwrap(), MegaSpecId::default());
    }

    /// The compile-time checker is itself exercised with malformed lists: the real `ALL`
    /// always satisfies the property, so only rejection cases can detect a weakened guard
    /// inside the checker.
    #[test]
    fn test_is_ladder_prefix_rejects_malformed_lists() {
        assert!(is_ladder_prefix(MegaSpecId::ALL));
        assert!(is_ladder_prefix(&[]), "the empty prefix is a ladder prefix");
        assert!(is_ladder_prefix(&[MegaSpecId::EQUIVALENCE, MegaSpecId::MINI_REX]));

        assert!(
            !is_ladder_prefix(&[MegaSpecId::MINI_REX, MegaSpecId::EQUIVALENCE]),
            "reordered entries must be rejected"
        );
        assert!(
            !is_ladder_prefix(&[MegaSpecId::MINI_REX]),
            "a list not starting at the ladder base must be rejected"
        );
        assert!(
            !is_ladder_prefix(&[MegaSpecId::EQUIVALENCE, MegaSpecId::REX]),
            "a skipped rung must be rejected"
        );
    }

    #[test]
    fn test_all_specs_map_to_isthmus_and_prague() {
        for spec in MegaSpecId::ALL.iter().copied() {
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
