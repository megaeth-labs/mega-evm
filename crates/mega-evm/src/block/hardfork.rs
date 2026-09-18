//! `MegaETH` hardforks of the Satin engine.

use alloy_hardforks::hardfork;

use crate::MegaSpecId;

hardfork! {
    /// `MegaETH` network upgrades that schedule a Satin-engine spec. It is expected to mix with
    /// `EthereumHardfork` and `OpHardfork`.
    ///
    /// The forks of the legacy engine (`MiniRex` through `Rex7`) are not variants here; a chain
    /// runs them on the legacy engine up to the Satin activation.
    #[derive(serde::Serialize, serde::Deserialize)]
    MegaHardfork {
        /// Activates [`MegaSpecId::SATIN`].
        Satin,
    }
}

impl MegaHardfork {
    /// The spec this hardfork activates.
    pub const fn spec_id(&self) -> MegaSpecId {
        match self {
            Self::Satin => MegaSpecId::SATIN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MegaSpecId;
    use alloy_hardforks::Hardfork;
    use core::str::FromStr;

    #[test]
    fn test_satin_fork_schedules_the_satin_spec() {
        assert_eq!(MegaHardfork::VARIANTS, &[MegaHardfork::Satin]);
        assert_eq!(MegaHardfork::Satin.spec_id(), MegaSpecId::SATIN);
        assert_eq!(Hardfork::name(&MegaHardfork::Satin), "Satin");
        assert_eq!(MegaHardfork::Satin.to_string(), "Satin");
    }

    #[test]
    fn test_legacy_fork_names_do_not_parse() {
        assert_eq!(MegaHardfork::from_str("Satin").unwrap(), MegaHardfork::Satin);
        for legacy in ["MiniRex", "MiniRex1", "MiniRex2", "Rex", "Rex1", "Rex6", "Rex7"] {
            assert!(MegaHardfork::from_str(legacy).is_err(), "{legacy} must not parse");
        }
    }
}
