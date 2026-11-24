//! The fixture forks the runner executes.

use core::{fmt, str::FromStr};

use mega_evm::EthSpecId;
use serde::Serialize;

use crate::types::SpecName;

/// A fork whose fixture entries the runner executes.
///
/// Satin's base spec is Osaka, so these are the forks it has a neutral configuration for
/// (`mega_evm::test_utils::neutral_cfg`): Osaka, which the main fixture release is written for,
/// and Amsterdam, which the glamsterdam devnet release is. A fixture file carries entries for
/// several forks; a run executes the entries of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Fork {
    /// Osaka: the main fixture release.
    Osaka,
    /// Amsterdam: the glamsterdam devnet fixture release.
    Amsterdam,
}

impl Fork {
    /// Every fork the runner executes.
    pub const ALL: [Self; 2] = [Self::Osaka, Self::Amsterdam];

    /// The Ethereum spec of this fork.
    pub const fn spec_id(self) -> EthSpecId {
        match self {
            Self::Osaka => EthSpecId::OSAKA,
            Self::Amsterdam => EthSpecId::AMSTERDAM,
        }
    }

    /// The key of this fork's entries in a fixture's `post` map.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Osaka => "Osaka",
            Self::Amsterdam => "Amsterdam",
        }
    }

    /// Whether a fixture's `post` key names this fork.
    pub fn is(self, spec: &SpecName) -> bool {
        matches!(
            (self, spec),
            (Self::Osaka, SpecName::Osaka) | (Self::Amsterdam, SpecName::Amsterdam)
        )
    }
}

impl fmt::Display for Fork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error returned when a string names no fork the runner executes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown fork {0:?}; expected one of: Osaka, Amsterdam")]
pub struct UnknownFork(pub String);

impl FromStr for Fork {
    type Err = UnknownFork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|fork| fork.name() == s).ok_or_else(|| UnknownFork(s.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fork_names_round_trip() {
        for fork in Fork::ALL {
            assert_eq!(fork.name().parse::<Fork>(), Ok(fork));
            assert_eq!(fork.to_string(), fork.name());
        }
        assert!("Prague".parse::<Fork>().is_err());
        assert!("osaka".parse::<Fork>().is_err());
    }

    #[test]
    fn test_fork_matches_its_post_key_only() {
        let key = |s: &str| serde_json::from_str::<SpecName>(&format!("\"{s}\"")).unwrap();
        assert!(Fork::Osaka.is(&key("Osaka")));
        assert!(!Fork::Osaka.is(&key("Amsterdam")));
        assert!(!Fork::Osaka.is(&key("Prague")));
        assert!(Fork::Amsterdam.is(&key("Amsterdam")));
        assert!(!Fork::Amsterdam.is(&key("Osaka")));
    }

    #[test]
    fn test_fork_spec_ids() {
        assert_eq!(Fork::Osaka.spec_id(), EthSpecId::OSAKA);
        assert_eq!(Fork::Amsterdam.spec_id(), EthSpecId::AMSTERDAM);
    }
}
