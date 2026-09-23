//! The two configurations the runner executes a fixture under.

use core::{fmt, str::FromStr};

use mega_evm::{
    alloy_evm::Database,
    revm::{
        context::{BlockEnv, CfgEnv},
        inspector::NoOpInspector,
    },
    test_utils::{neutral_cfg, neutralize_evm, zero_fee_l1_block_info},
    MegaContext, MegaEvm, MegaSpecId,
};
use serde::Serialize;

use crate::Fork;

/// The most blobs a transaction may carry under the fixture forks, as the reference runner
/// configures it from Osaka on.
pub const MAX_BLOBS_PER_TX: u64 = 6;

/// How the runner configures `MegaEvm` for a fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Satin's machinery priced as the fixture's fork prices it: the neutral configuration
    /// (`MegaContext::with_neutral_cfg`) built for the fork, and the fork's own precompile set and
    /// static opcode prices (`test_utils::neutralize_evm`).
    ///
    /// What is left to differ from the fixture is what the machinery does, which is why this mode
    /// is a gate.
    Equivalence,
    /// Satin's own configuration, whatever the fixture's fork: its schedule, EIP-8037 and
    /// EIP-2780, its execution cap and code-size limits, history gas and its precompile set.
    /// The fixture's chain id and blob limit are kept.
    Satin,
}

impl Mode {
    /// Both modes.
    pub const ALL: [Self; 2] = [Self::Equivalence, Self::Satin];

    /// The name the command line takes.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Equivalence => "equivalence",
            Self::Satin => "satin",
        }
    }

    /// The configuration a test of `fork` runs under, on chain `chain_id`.
    pub fn cfg(self, fork: Fork, chain_id: u64) -> CfgEnv<MegaSpecId> {
        let mut cfg = match self {
            Self::Equivalence => {
                neutral_cfg(fork.spec_id()).expect("every runner fork has a neutral configuration")
            }
            Self::Satin => CfgEnv::new_with_spec(MegaSpecId::SATIN),
        };
        cfg.chain_id = chain_id;
        cfg.max_blobs_per_tx = Some(MAX_BLOBS_PER_TX);
        cfg
    }

    /// The context a test of `fork` runs in: this mode's configuration, `block`, and L1 block
    /// info that charges no L1 fee and is already the block's, so op-revm reads nothing to price
    /// the transaction's L1 cost.
    pub fn context<DB: Database>(
        self,
        fork: Fork,
        db: DB,
        block: BlockEnv,
        chain_id: u64,
    ) -> MegaContext<DB> {
        let ctx = MegaContext::new(db, MegaSpecId::SATIN);
        let cfg = self.cfg(fork, chain_id);
        let ctx = match self {
            Self::Equivalence => ctx.with_neutral_cfg(cfg),
            Self::Satin => ctx.with_cfg(cfg),
        };
        let mut l1 = zero_fee_l1_block_info();
        l1.l2_block = Some(block.number);
        ctx.with_block(block).with_chain(l1)
    }

    /// The EVM a test of `fork` runs on: [`context`](Self::context), with the fork's precompile
    /// set and static opcode prices in equivalence mode and Satin's in Satin mode.
    pub fn evm<DB: Database>(
        self,
        fork: Fork,
        db: DB,
        block: BlockEnv,
        chain_id: u64,
    ) -> MegaEvm<DB, NoOpInspector> {
        let mut evm = MegaEvm::new(self.context(fork, db, block, chain_id));
        if self == Self::Equivalence {
            neutralize_evm(&mut evm, fork.spec_id())
                .expect("every runner fork has a neutral configuration");
        }
        evm
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error returned when a string names no mode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown mode {0:?}; expected equivalence or satin")]
pub struct UnknownMode(pub String);

impl FromStr for Mode {
    type Err = UnknownMode;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|mode| mode.name() == s).ok_or_else(|| UnknownMode(s.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::{
        constants::TX_GAS_LIMIT_CAP,
        revm::{context::ContextTr, database::EmptyDB, primitives::U256},
        satin_gas_params,
    };

    #[test]
    fn test_mode_names_round_trip() {
        for mode in Mode::ALL {
            assert_eq!(mode.name().parse::<Mode>(), Ok(mode));
        }
        assert!("Equivalence".parse::<Mode>().is_err());
    }

    /// Equivalence mode runs the fork's configuration, neutrally; Satin mode runs Satin's. Both
    /// keep the fixture's chain id and the blob limit.
    #[test]
    fn test_modes_configure_the_context() {
        let block = BlockEnv { number: U256::from(9), ..Default::default() };
        for fork in Fork::ALL {
            let ctx = Mode::Equivalence.context(fork, EmptyDB::default(), block.clone(), 7);
            assert!(ctx.is_neutral());
            assert!(!ctx.prices_history());
            assert_eq!(
                ctx.cfg().gas_params.table(),
                neutral_cfg(fork.spec_id()).unwrap().gas_params.table()
            );
            assert_eq!(ctx.cfg().chain_id, 7);
            assert_eq!(ctx.cfg().max_blobs_per_tx, Some(MAX_BLOBS_PER_TX));

            let ctx = Mode::Satin.context(fork, EmptyDB::default(), block.clone(), 7);
            assert!(!ctx.is_neutral());
            assert_eq!(ctx.cfg().gas_params.table(), satin_gas_params().table());
            assert_eq!(ctx.cfg().tx_gas_limit_cap, Some(TX_GAS_LIMIT_CAP));
            assert_eq!(ctx.cfg().chain_id, 7);
            assert_eq!(ctx.chain().l2_block, Some(U256::from(9)), "no L1 info read");
        }
    }
}
