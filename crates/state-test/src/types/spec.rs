use mega_evm::MegaSpecId;
use serde::{Deserialize, Serialize};

/// Ethereum specification names
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
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
    /// Unknown or unsupported specification
    #[serde(other)]
    Unknown,
}

impl SpecName {
    /// Converts to a [`MegaSpecId`].
    /// All Ethereum spc is converted to [`MegaSpecId::EQUIVALENCE`]. Some tests may fail to
    /// execute. We will skip them case by case.
    pub fn to_spec_id(&self) -> MegaSpecId {
        match self {
            Self::MiniRex => MegaSpecId::MINI_REX,
            Self::Rex => MegaSpecId::REX,
            Self::Rex1 => MegaSpecId::REX1,
            Self::Rex2 => MegaSpecId::REX2,
            Self::Rex3 => MegaSpecId::REX3,
            Self::Rex4 => MegaSpecId::REX4,
            Self::Rex5 => MegaSpecId::REX5,
            Self::Unknown => panic!("Unknown spec"),
            // All Ethereum specs (and `Equivalence`) map to the equivalent baseline.
            _ => MegaSpecId::EQUIVALENCE,
        }
    }

    /// Returns the [`SpecName`] corresponding to a [`MegaSpecId`].
    ///
    /// Used when dumping a replay fixture so the `post` map is keyed by the spec
    /// the transaction actually executed under.
    pub fn from_mega_spec(spec: MegaSpecId) -> Self {
        match spec {
            MegaSpecId::MINI_REX => Self::MiniRex,
            MegaSpecId::EQUIVALENCE => Self::Equivalence,
            MegaSpecId::REX => Self::Rex,
            MegaSpecId::REX1 => Self::Rex1,
            MegaSpecId::REX2 => Self::Rex2,
            MegaSpecId::REX3 => Self::Rex3,
            MegaSpecId::REX4 => Self::Rex4,
            MegaSpecId::REX5 => Self::Rex5,
            _ => Self::Unknown,
        }
    }
}
