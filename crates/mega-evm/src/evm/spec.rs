//! The `MegaETH` EVM spec of the Satin engine (`MegaSpecId`).

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{
    fmt::{self, Display},
    str::FromStr,
};
use std::string::String;

pub use op_revm::OpSpecId;
pub use revm::primitives::hardfork::SpecId as EthSpecId;
use serde::{Deserialize, Serialize};

/// `MegaETH` spec id: the EVM behavior the Satin engine executes.
///
/// Satin is a single-spec engine. The specs of the legacy engine (`Equivalence` through `Rex7`)
/// are not variants here and never parse to one; they are executed by the legacy engine.
///
/// Each spec runs on top of an Optimism spec, which in turn runs on top of an Ethereum spec:
/// - [`MegaSpecId::SATIN`] -> [`OpSpecId::KARST`] -> [`EthSpecId::OSAKA`]
///
/// The discriminants continue after the legacy ladder (`EQUIVALENCE` = 0 through `REX7` = 11),
/// so a raw `as u8` of a Satin spec never aliases a legacy one.
#[repr(u8)]
#[derive(
    Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[non_exhaustive]
pub enum MegaSpecId {
    /// The Satin spec: Karst / Osaka base with EIP-8037 state gas and the EIP-2780 intrinsic
    /// cost, under a 200M execution cap.
    #[default]
    SATIN = 12,
}

/// String identifiers of the `MegaETH` specs.
pub mod name {
    /// The string identifier of [`MegaSpecId::SATIN`](super::MegaSpecId::SATIN).
    pub const SATIN: &str = "Satin";
}

/// The string identifiers of the legacy engine's specs.
///
/// Parsing one of them fails with [`ParseMegaSpecError::Legacy`]: the Satin engine does not
/// execute legacy specs, and a legacy name is never mapped to a Satin spec.
pub const LEGACY_SPEC_NAMES: &[&str] = &[
    "Equivalence",
    "MiniRex",
    "MiniRex1",
    "MiniRex2",
    "Rex",
    "Rex1",
    "Rex2",
    "Rex3",
    "Rex4",
    "Rex5",
    "Rex6",
    "Rex7",
];

impl MegaSpecId {
    /// Every spec, oldest first.
    pub const ALL: &'static [Self] = &[Self::SATIN];

    /// The Optimism spec this spec runs on.
    pub const fn into_op_spec(self) -> OpSpecId {
        match self {
            Self::SATIN => OpSpecId::KARST,
        }
    }

    /// The Ethereum spec this spec runs on.
    pub const fn into_eth_spec(self) -> EthSpecId {
        self.into_op_spec().into_eth_spec()
    }

    /// The string identifier of this spec.
    pub const fn name(self) -> &'static str {
        match self {
            Self::SATIN => name::SATIN,
        }
    }
}

/// Error returned when a string does not name a Satin spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseMegaSpecError {
    /// The string names a spec of the legacy engine.
    Legacy(String),
    /// The string names no known spec.
    Unknown,
}

impl Display for ParseMegaSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Legacy(name) => {
                write!(f, "{name} is a spec of the legacy engine; Satin does not execute it")
            }
            Self::Unknown => f.write_str("unknown MegaETH spec name"),
        }
    }
}

impl core::error::Error for ParseMegaSpecError {}

impl FromStr for MegaSpecId {
    type Err = ParseMegaSpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            name::SATIN => Ok(Self::SATIN),
            legacy if LEGACY_SPEC_NAMES.contains(&legacy) => {
                Err(ParseMegaSpecError::Legacy(legacy.into()))
            }
            _ => Err(ParseMegaSpecError::Unknown),
        }
    }
}

impl From<MegaSpecId> for &'static str {
    fn from(spec: MegaSpecId) -> Self {
        spec.name()
    }
}

impl From<MegaSpecId> for OpSpecId {
    fn from(spec: MegaSpecId) -> Self {
        spec.into_op_spec()
    }
}

impl From<MegaSpecId> for EthSpecId {
    fn from(spec: MegaSpecId) -> Self {
        spec.into_eth_spec()
    }
}

impl Display for MegaSpecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::str::FromStr;

    /// Every spec name of the legacy engine, written out by hand so the rejection check does
    /// not read the list it is checking.
    const LEGACY_NAMES: [&str; 12] = [
        "Equivalence",
        "MiniRex",
        "MiniRex1",
        "MiniRex2",
        "Rex",
        "Rex1",
        "Rex2",
        "Rex3",
        "Rex4",
        "Rex5",
        "Rex6",
        "Rex7",
    ];

    #[test]
    fn test_spec_names_roundtrip_and_display() {
        assert_eq!(MegaSpecId::SATIN.to_string(), "Satin");
        assert_eq!(<&'static str>::from(MegaSpecId::SATIN), "Satin");
        assert_eq!(MegaSpecId::from_str("Satin"), Ok(MegaSpecId::SATIN));
        assert_eq!(MegaSpecId::from_str("satin"), Err(ParseMegaSpecError::Unknown));
        assert_eq!(MegaSpecId::from_str(""), Err(ParseMegaSpecError::Unknown));
    }

    #[test]
    fn test_legacy_spec_names_are_rejected_not_mapped() {
        for name in LEGACY_NAMES {
            assert_eq!(
                MegaSpecId::from_str(name),
                Err(ParseMegaSpecError::Legacy(name.into())),
                "{name} must not parse to a Satin spec"
            );
        }
        assert_eq!(LEGACY_SPEC_NAMES, LEGACY_NAMES.as_slice());
    }

    #[test]
    fn test_parse_error_names_the_legacy_engine() {
        let err = MegaSpecId::from_str("Rex6").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Rex6 is a spec of the legacy engine; Satin does not execute it"
        );
        assert_eq!(
            MegaSpecId::from_str("Rex8").unwrap_err().to_string(),
            "unknown MegaETH spec name"
        );
    }

    #[test]
    fn test_serde_uses_the_variant_name() {
        assert_eq!(serde_json::to_string(&MegaSpecId::SATIN).unwrap(), "\"SATIN\"");
        assert_eq!(serde_json::from_str::<MegaSpecId>("\"SATIN\"").unwrap(), MegaSpecId::SATIN);
        for legacy in ["\"REX6\"", "\"REX7\"", "\"EQUIVALENCE\"", "\"Satin\""] {
            assert!(
                serde_json::from_str::<MegaSpecId>(legacy).is_err(),
                "{legacy} must not deserialize"
            );
        }
    }

    #[test]
    fn test_all_ends_at_the_latest_spec() {
        assert_eq!(MegaSpecId::ALL, &[MegaSpecId::SATIN]);
        assert_eq!(*MegaSpecId::ALL.last().unwrap(), MegaSpecId::default());
    }

    /// A raw `as u8` of a Satin spec never aliases a legacy rung (`EQUIVALENCE` = 0 through
    /// `REX7` = 11).
    #[test]
    fn test_discriminant_continues_after_the_legacy_ladder() {
        assert_eq!(MegaSpecId::SATIN as u8, 12);
    }

    #[test]
    fn test_all_specs_map_to_karst_and_osaka() {
        assert_eq!(MegaSpecId::SATIN.into_op_spec(), OpSpecId::KARST);
        assert_eq!(MegaSpecId::SATIN.into_eth_spec(), EthSpecId::OSAKA);
        assert_eq!(OpSpecId::from(MegaSpecId::SATIN), OpSpecId::KARST);
        assert_eq!(EthSpecId::from(MegaSpecId::SATIN), EthSpecId::OSAKA);
    }
}
