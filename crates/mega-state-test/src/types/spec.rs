use mega_evm::MegaSpecId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error returned when a [`SpecName`] has no [`MegaSpecId`] mapping.
///
/// Produced for [`SpecName::Unknown`], which is what any unrecognized spec
/// string in a fixture's `post` map (or a future spec this crate does not know
/// yet) deserializes to. The original string is not retained by serde's
/// `other` fallback, so callers should add the surrounding fixture context.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown or unsupported spec name (an unrecognized spec string deserializes to `SpecName::Unknown`)")]
pub struct UnknownSpecError;

/// Ethereum specification names
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub enum SpecName {
    /// Frontier hardfork (Ethereum launch, July 2015)
    Frontier,
    /// Transition from Frontier to Homestead at block 5
    FrontierToHomesteadAt5,
    /// Homestead hardfork (March 2016)
    Homestead,
    /// Transition from Homestead to DAO fork at block 5
    HomesteadToDaoAt5,
    /// Transition from Homestead to EIP-150 at block 5
    HomesteadToEIP150At5,
    /// EIP-150 hardfork (Tangerine Whistle, October 2016)
    EIP150,
    /// EIP-158/EIP-161 hardfork (Spurious Dragon, November 2016)
    EIP158, // EIP-161: State trie clearing
    /// Transition from EIP-158 to Byzantium at block 5
    EIP158ToByzantiumAt5,
    /// Byzantium hardfork (October 2017)
    Byzantium,
    /// Transition from Byzantium to Constantinople at block 5 (skipped)
    ByzantiumToConstantinopleAt5, // SKIPPED
    /// Transition from Byzantium to Constantinople Fix at block 5
    ByzantiumToConstantinopleFixAt5,
    /// Constantinople hardfork (skipped due to reentrancy bug)
    Constantinople, // SKIPPED
    /// Constantinople Fix hardfork (Petersburg, February 2019)
    ConstantinopleFix,
    /// Istanbul hardfork (December 2019)
    Istanbul,
    /// Berlin hardfork (April 2021)
    Berlin,
    /// Transition from Berlin to London at block 5
    BerlinToLondonAt5,
    /// London hardfork (August 2021, includes EIP-1559)
    London,
    /// Paris hardfork (part of The Merge)
    Paris,
    /// The Merge (September 2022, `PoW` to `PoS` transition)
    Merge,
    /// Shanghai hardfork (April 2023, includes withdrawals)
    Shanghai,
    /// Cancun hardfork (March 2024, includes EIP-4844)
    Cancun,
    /// Prague hardfork (future)
    Prague,
    /// Osaka hardfork (skipped)
    Osaka, // SKIPPED
    /// `MegaETH` `MiniRex` hardfork
    MiniRex,
    /// `MegaETH` `MiniRex1` alias spec (behavior: `Equivalence`)
    MiniRex1,
    /// `MegaETH` `MiniRex2` alias spec (behavior: `MiniRex`)
    MiniRex2,
    /// `MegaETH` `Equivalence` spec (Ethereum-equivalent baseline)
    Equivalence,
    /// `MegaETH` `Rex` spec
    Rex,
    /// `MegaETH` `Rex1` spec
    Rex1,
    /// `MegaETH` `Rex2` spec
    Rex2,
    /// `MegaETH` `Rex3` spec
    Rex3,
    /// `MegaETH` `Rex4` spec
    Rex4,
    /// `MegaETH` `Rex5` spec
    Rex5,
    /// `MegaETH` `Rex6` spec
    Rex6,
    /// `MegaETH` `Rex7` spec
    Rex7,
    /// Unknown or unsupported specification
    #[serde(other)]
    Unknown,
}

impl SpecName {
    /// Converts to a [`MegaSpecId`].
    /// All Ethereum spc is converted to [`MegaSpecId::EQUIVALENCE`]. Some tests may fail to
    /// execute. We will skip them case by case.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownSpecError`] for [`SpecName::Unknown`], i.e. a fixture
    /// spec string that did not match any known name.
    pub fn to_spec_id(&self) -> Result<MegaSpecId, UnknownSpecError> {
        match self {
            Self::MiniRex => Ok(MegaSpecId::MINI_REX),
            Self::MiniRex1 => Ok(MegaSpecId::MINI_REX_1),
            Self::MiniRex2 => Ok(MegaSpecId::MINI_REX_2),
            Self::Rex => Ok(MegaSpecId::REX),
            Self::Rex1 => Ok(MegaSpecId::REX1),
            Self::Rex2 => Ok(MegaSpecId::REX2),
            Self::Rex3 => Ok(MegaSpecId::REX3),
            Self::Rex4 => Ok(MegaSpecId::REX4),
            Self::Rex5 => Ok(MegaSpecId::REX5),
            Self::Rex6 => Ok(MegaSpecId::REX6),
            Self::Rex7 => Ok(MegaSpecId::REX7),
            Self::Unknown => Err(UnknownSpecError),
            // All Ethereum specs (and `Equivalence`) map to the equivalent baseline.
            _ => Ok(MegaSpecId::EQUIVALENCE),
        }
    }

    /// Returns the [`SpecName`] corresponding to a [`MegaSpecId`].
    ///
    /// Used when dumping a replay fixture so the `post` map is keyed by the spec
    /// the transaction actually executed under.
    pub fn from_mega_spec(spec: MegaSpecId) -> Self {
        match spec {
            MegaSpecId::MINI_REX => Self::MiniRex,
            MegaSpecId::MINI_REX_1 => Self::MiniRex1,
            MegaSpecId::MINI_REX_2 => Self::MiniRex2,
            MegaSpecId::EQUIVALENCE => Self::Equivalence,
            MegaSpecId::REX => Self::Rex,
            MegaSpecId::REX1 => Self::Rex1,
            MegaSpecId::REX2 => Self::Rex2,
            MegaSpecId::REX3 => Self::Rex3,
            MegaSpecId::REX4 => Self::Rex4,
            MegaSpecId::REX5 => Self::Rex5,
            MegaSpecId::REX6 => Self::Rex6,
            MegaSpecId::REX7 => Self::Rex7,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_spec_id_known_specs_succeed() {
        // MegaETH specs map to their own ids.
        assert_eq!(SpecName::MiniRex.to_spec_id(), Ok(MegaSpecId::MINI_REX));
        assert_eq!(SpecName::MiniRex1.to_spec_id(), Ok(MegaSpecId::MINI_REX_1));
        assert_eq!(SpecName::MiniRex2.to_spec_id(), Ok(MegaSpecId::MINI_REX_2));
        assert_eq!(SpecName::Rex.to_spec_id(), Ok(MegaSpecId::REX));
        assert_eq!(SpecName::Rex1.to_spec_id(), Ok(MegaSpecId::REX1));
        assert_eq!(SpecName::Rex2.to_spec_id(), Ok(MegaSpecId::REX2));
        assert_eq!(SpecName::Rex3.to_spec_id(), Ok(MegaSpecId::REX3));
        assert_eq!(SpecName::Rex4.to_spec_id(), Ok(MegaSpecId::REX4));
        assert_eq!(SpecName::Rex5.to_spec_id(), Ok(MegaSpecId::REX5));
        assert_eq!(SpecName::Rex6.to_spec_id(), Ok(MegaSpecId::REX6));
        // Every Ethereum spec (and `Equivalence`) maps to the equivalent
        // baseline — sample the oldest, a transition, and the newest.
        for eth in [
            SpecName::Frontier,
            SpecName::HomesteadToDaoAt5,
            SpecName::Constantinople,
            SpecName::Cancun,
            SpecName::Osaka,
            SpecName::Equivalence,
        ] {
            assert_eq!(eth.to_spec_id(), Ok(MegaSpecId::EQUIVALENCE));
        }
    }

    #[test]
    fn test_to_spec_id_unknown_is_a_clean_error() {
        assert_eq!(SpecName::Unknown.to_spec_id(), Err(UnknownSpecError));
    }

    #[test]
    fn test_unrecognized_spec_string_deserializes_to_unknown_and_errors() {
        // An unrecognized fixture spec string takes serde's `other` fallback
        // and must then convert to a structured error, never a panic.
        let spec: SpecName = serde_json::from_str("\"FutureFork9000\"").expect("deserialize");
        assert_eq!(spec, SpecName::Unknown);
        assert!(spec.to_spec_id().is_err());
    }

    #[test]
    fn test_from_mega_spec_round_trips_known_specs() {
        for spec in [
            MegaSpecId::MINI_REX,
            MegaSpecId::MINI_REX_1,
            MegaSpecId::MINI_REX_2,
            MegaSpecId::EQUIVALENCE,
            MegaSpecId::REX,
            MegaSpecId::REX1,
            MegaSpecId::REX2,
            MegaSpecId::REX3,
            MegaSpecId::REX4,
            MegaSpecId::REX5,
            MegaSpecId::REX6,
            MegaSpecId::REX7,
        ] {
            assert_eq!(SpecName::from_mega_spec(spec).to_spec_id(), Ok(spec));
        }
    }

    #[test]
    fn test_alias_spec_names_match_mega_evm_names() {
        // A fixture dumped under an alias spec keys its `post` map by the
        // serialized `SpecName`, and `--override.spec` parses the same string
        // through `MegaSpecId`'s `FromStr` — the two name surfaces must agree.
        for (spec_name, mega_name) in [
            (SpecName::MiniRex1, mega_evm::name::MINI_REX_1),
            (SpecName::MiniRex2, mega_evm::name::MINI_REX_2),
        ] {
            let serialized = serde_json::to_string(&spec_name).expect("serialize");
            assert_eq!(serialized, format!("\"{mega_name}\""));
        }
    }

    #[test]
    fn test_from_mega_spec_maps_the_latest_spec() {
        // `MegaSpecId` is `#[non_exhaustive]`, so `from_mega_spec` needs a wildcard
        // arm and a newly introduced spec silently falls through to `Unknown`
        // instead of failing to compile. `MegaSpecId::default()` always tracks the
        // latest spec, so pinning it here turns that silent mis-mapping — which
        // would key a dumped replay fixture's `post` map by `Unknown` — into a test
        // failure.
        let latest = MegaSpecId::default();
        assert_ne!(SpecName::from_mega_spec(latest), SpecName::Unknown);
        assert_eq!(SpecName::from_mega_spec(latest).to_spec_id(), Ok(latest));
    }
}
