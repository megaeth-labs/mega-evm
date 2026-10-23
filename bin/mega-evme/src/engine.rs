//! Which engine runs a command, and the hand-off to the legacy one.
//!
//! Every spec name belongs to exactly one engine, so the spec a command runs is the engine it runs
//! on: `Satin` is the Satin engine, `Equivalence` through `Rex6` are the legacy engine. The legacy
//! engine is the released 1.7.1 line, and a command on it runs the released 1.7.1 CLI, so what a
//! legacy spec prints is what the 1.7.1 tool printed.

use std::ffi::OsString;

use mega_evm::{ChainActivation, MegaSpecId, ParseMegaSpecError};

use crate::common::{chain_activation, EvmeError, Result};

/// The spec `run` and `tx` default to: the chain's current spec, on the legacy engine.
pub const DEFAULT_SPEC: &str = "Rex6";

/// The engine a command runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// The Satin engine, the workspace's `mega-evm`.
    Satin,
    /// The legacy engine: `mega-evm` 1.7.1 and the `mega-evme` 1.7.1 CLI that drives it.
    Legacy,
}

impl Engine {
    /// The engine that runs the spec named `name`.
    ///
    /// `Rex7` is refused: it exists only on the legacy line, it never activated on a chain, and
    /// Satin supersedes it.
    pub fn of_spec(name: &str) -> Result<Self> {
        match name.parse::<MegaSpecId>() {
            Ok(_) => Ok(Self::Satin),
            Err(ParseMegaSpecError::Legacy(_)) => Ok(Self::Legacy),
            Err(ParseMegaSpecError::Unknown) if name == "Rex7" => Err(EvmeError::InvalidInput(
                "Rex7 never activated on a chain and Satin supersedes it; use `Satin`, or a spec \
                 from `Equivalence` to `Rex6`"
                    .to_string(),
            )),
            Err(ParseMegaSpecError::Unknown) => Err(EvmeError::InvalidInput(format!(
                "Invalid spec name: {name:?}; possible values: `Equivalence`, `MiniRex`, \
                 `MiniRex1`, `MiniRex2`, `Rex`, `Rex1`, `Rex2`, `Rex3`, `Rex4`, `Rex5`, `Rex6`, \
                 `Satin`"
            ))),
        }
    }

    /// The engine a block of `chain_id` with timestamp `timestamp` executed on, or will execute
    /// on.
    ///
    /// A known chain runs Satin from the timestamp its activation table gives, and the legacy
    /// engine before it, or throughout while Satin is not scheduled. A chain the run was given the
    /// genesis file of (`--genesis`) is known, by its file's `satinTime`.
    ///
    /// # Errors
    ///
    /// For a chain that is not known ([`unknown_chain`]).
    pub fn of_block(chain_id: u64, timestamp: u64) -> Result<Self> {
        Self::under(chain_activation(chain_id), timestamp).ok_or_else(|| unknown_chain(chain_id))
    }

    /// The engine a block with timestamp `timestamp` runs on under `activation`, the activation
    /// table of its chain; `None` for a chain without one.
    fn under(activation: Option<ChainActivation>, timestamp: u64) -> Option<Self> {
        match activation? {
            ChainActivation { satin: Some(satin), .. } if timestamp >= satin => Some(Self::Satin),
            _ => Some(Self::Legacy),
        }
    }

    /// The engine every block of `chain_id` runs on, if the chain's schedule decides it without
    /// a timestamp: `None` when the chain switches engines at a scheduled timestamp.
    ///
    /// # Errors
    ///
    /// For a chain that is not known ([`unknown_chain`]).
    pub fn of_chain(chain_id: u64) -> Result<Option<Self>> {
        match chain_activation(chain_id) {
            None => Err(unknown_chain(chain_id)),
            Some(ChainActivation { satin: None, .. }) => Ok(Some(Self::Legacy)),
            Some(ChainActivation { satin: Some(_), .. }) => Ok(None),
        }
    }

    /// The engine's name in output.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Satin => "satin",
            Self::Legacy => "legacy",
        }
    }
}

/// The refusal of a chain neither the tool's table nor a genesis file names: the tool cannot tell
/// which engine its blocks run on, so the run names it.
pub fn unknown_chain(chain_id: u64) -> EvmeError {
    EvmeError::InvalidInput(format!(
        "chain {chain_id} is not a MegaETH chain this tool knows (mainnet {}, testnet {}): give its \
         genesis file (--genesis) or name the spec to run (--override.spec)",
        mega_evm::MAINNET_CHAIN_ID,
        mega_evm::TESTNET_CHAIN_ID
    ))
}

/// Runs `args` on the released 1.7.1 CLI.
///
/// `add_default_spec` adds `--spec Rex6` for a `run` or `tx` whose `--spec` was left out:
/// the 1.7.1 parser's own default is `Rex7`, which this tool refuses.
#[cfg(feature = "legacy")]
pub async fn run_legacy(mut args: Vec<OsString>, add_default_spec: bool) -> Result<()> {
    use clap::Parser;

    if add_default_spec {
        args = with_default_spec(args);
    }
    if let Err(e) = mega_evme_legacy::cmd::MainCmd::parse_from(args).run().await {
        // The released CLI's `main` prints this on stderr (`Error:` plus the error's `Debug`)
        // and also echoes the `Debug` form to stdout. The text comes back unchanged; this
        // process prints it once, with every other command error.
        return Err(EvmeError::ReleasedCli(format!("Error: {e:?}")));
    }
    Ok(())
}

/// `args` with `--spec Rex6` added where the 1.7.1 parser reads it as the option: before the first
/// `--`, which ends the options, or at the end when there is none.
///
/// After a `--` every argument is a positional, so a flag appended behind one would be read as
/// the code or the raw transaction. No option of either CLI takes a value starting with a hyphen,
/// so the first `--` after the program name is the separator.
#[cfg(feature = "legacy")]
fn with_default_spec(mut args: Vec<OsString>) -> Vec<OsString> {
    let at = args.iter().skip(1).position(|arg| arg == "--").map_or(args.len(), |i| i + 1);
    args.splice(at..at, [OsString::from("--spec"), OsString::from(DEFAULT_SPEC)]);
    args
}

/// Refuses a legacy spec in a build without the legacy leg.
#[cfg(not(feature = "legacy"))]
pub async fn run_legacy(_args: Vec<OsString>, _add_default_spec: bool) -> Result<()> {
    Err(EvmeError::InvalidInput(
        "this build runs Satin only (built without the `legacy` feature); a spec from \
         `Equivalence` to `Rex6` needs the legacy leg"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::{MAINNET_CHAIN_ID, TESTNET_CHAIN_ID};

    #[test]
    fn test_each_spec_names_its_engine() {
        assert_eq!(Engine::of_spec("Satin").unwrap(), Engine::Satin);
        for legacy in mega_evm::LEGACY_SPEC_NAMES {
            assert_eq!(Engine::of_spec(legacy).unwrap(), Engine::Legacy, "{legacy}");
        }
        assert_eq!(Engine::of_spec(DEFAULT_SPEC).unwrap(), Engine::Legacy);
    }

    /// The default spec goes where the 1.7.1 parser reads it as the option: at the end, or
    /// before the first `--`, after which every argument is a positional.
    #[cfg(feature = "legacy")]
    #[test]
    fn test_the_default_spec_goes_before_the_options_end() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            with_default_spec(args(&["mega-evme", "run", "--json", "0x00"])),
            args(&["mega-evme", "run", "--json", "0x00", "--spec", "Rex6"])
        );
        assert_eq!(
            with_default_spec(args(&["mega-evme", "run", "--json", "--", "0x00"])),
            args(&["mega-evme", "run", "--json", "--spec", "Rex6", "--", "0x00"])
        );
        // A `--` after the separator is a positional, and the program name is no argument.
        assert_eq!(
            with_default_spec(args(&["--", "tx", "--", "--"])),
            args(&["--", "tx", "--spec", "Rex6", "--", "--"])
        );
    }

    #[test]
    fn test_rex7_and_unknown_names_are_refused() {
        let rex7 = Engine::of_spec("Rex7").unwrap_err().to_string();
        assert!(rex7.contains("Satin supersedes it"), "{rex7}");
        let unknown = Engine::of_spec("Prague").unwrap_err().to_string();
        assert!(unknown.contains("Invalid spec name"), "{unknown}");
        // Spec names are case-sensitive, as both engines' parsers are.
        assert!(Engine::of_spec("satin").is_err());
    }

    /// Mainnet and testnet have no Satin timestamp yet: every block of theirs is legacy, and the
    /// schedule says so without a timestamp. A chain the tool does not know is refused, naming
    /// how to run it.
    #[test]
    fn test_the_chain_schedule_picks_the_engine() {
        for chain_id in [MAINNET_CHAIN_ID, TESTNET_CHAIN_ID] {
            assert_eq!(Engine::of_chain(chain_id).unwrap(), Some(Engine::Legacy));
            assert_eq!(Engine::of_block(chain_id, 0).unwrap(), Engine::Legacy);
            assert_eq!(Engine::of_block(chain_id, u64::MAX).unwrap(), Engine::Legacy);
        }
        for error in [
            Engine::of_chain(1_337).unwrap_err().to_string(),
            Engine::of_block(1_337, 0).unwrap_err().to_string(),
        ] {
            assert!(error.contains("chain 1337 is not a MegaETH chain this tool knows"), "{error}");
            assert!(error.contains("--genesis") && error.contains("--override.spec"), "{error}");
        }
    }

    /// Once a known chain schedules Satin, its blocks before the timestamp stay legacy and the
    /// block at the timestamp is the first Satin one. A chain without a table has no engine.
    #[test]
    fn test_a_scheduled_chain_switches_at_the_timestamp() {
        let scheduled = ChainActivation { chain_id: MAINNET_CHAIN_ID, satin: Some(1_800_000_000) };
        assert_eq!(Engine::under(Some(scheduled), 1_799_999_999), Some(Engine::Legacy));
        assert_eq!(Engine::under(Some(scheduled), 1_800_000_000), Some(Engine::Satin));
        assert_eq!(Engine::under(None, 0), None);
    }
}
