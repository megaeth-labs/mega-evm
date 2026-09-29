//! The hardfork schedule a Satin run executes under, and so the protocol limits it is held to.

use mega_evm::{MegaHardforkConfig, MegaHardforks, MegaSpecId};

use super::{EvmeError, Result};

/// The hardfork schedule a Satin run for chain `chain_id` at `timestamp` executes under.
///
/// A block the chain's own schedule runs on Satin runs under that schedule, with the parameters
/// it carries: the chain's protocol limits among them. A schedule that cannot run the block — its
/// table schedules Satin without the parameters Satin requires — is refused rather than filled in:
/// the tool does not know the chain's limits, and a default in their place would report results
/// the chain does not produce.
///
/// Any other run is a counterfactual and executes under the schedule the Satin engine gives a chain
/// it does not know ([`mega_evm::all_activated_hardforks`]): Satin from genesis, placeholder
/// registry parameters, which only a registry deployed from scratch reads (a chain whose registry
/// is already deployed keeps the roles in its storage), and
/// [`ProtocolLimits::DEFAULT`](mega_evm::ProtocolLimits::DEFAULT).
pub fn satin_schedule(chain_id: u64, timestamp: u64) -> Result<MegaHardforkConfig> {
    schedule_for(mega_evm::hardfork_schedule(chain_id), timestamp)
}

/// [`satin_schedule`] for a chain whose schedule is `chain`.
fn schedule_for(chain: MegaHardforkConfig, timestamp: u64) -> Result<MegaHardforkConfig> {
    if chain.spec_id(timestamp) != Some(MegaSpecId::SATIN) {
        return Ok(mega_evm::all_activated_hardforks());
    }
    chain.validate_schedule().map_err(|e| {
        EvmeError::InvalidInput(format!(
            "the chain's schedule runs Satin at timestamp {timestamp} but cannot run it: {e}"
        ))
    })?;
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::{
        system::SequencerRegistryConfig, ChainActivation, EvmTxRuntimeLimits, ProtocolLimits,
        MAINNET_CHAIN_ID, TESTNET_CHAIN_ID,
    };

    const ACTIVATION: u64 = 1_800_000_000;

    fn scheduled() -> MegaHardforkConfig {
        ChainActivation { chain_id: MAINNET_CHAIN_ID, satin: Some(ACTIVATION) }.hardforks()
    }

    /// A counterfactual runs on the engine's schedule for an unknown chain, whatever the chain:
    /// Satin from genesis, the placeholder registry, the protocol's default limits.
    #[test]
    fn test_a_counterfactual_runs_on_the_engines_fallback_schedule() {
        for chain_id in [MAINNET_CHAIN_ID, TESTNET_CHAIN_ID, 6342, 1] {
            let schedule = satin_schedule(chain_id, 1_700_000_000).unwrap();
            assert_eq!(schedule.spec_id(0), Some(MegaSpecId::SATIN), "chain {chain_id}");
            assert_eq!(schedule.protocol_limits(0), Some(ProtocolLimits::DEFAULT));
            assert_eq!(
                schedule.fork_params::<SequencerRegistryConfig>(),
                Some(&SequencerRegistryConfig::placeholder())
            );
            assert_eq!(schedule.validate_schedule(), Ok(()));
        }
    }

    /// Before a chain's scheduled activation a Satin run is a counterfactual; from it on, the run
    /// takes the chain's schedule, and its limits are the chain's, not the default.
    #[test]
    fn test_a_real_satin_block_runs_under_the_chains_own_limits() {
        let own = ProtocolLimits::DEFAULT
            .with_tx_runtime_limits(EvmTxRuntimeLimits::default().with_tx_data_size_limit(350));
        let chain =
            scheduled().with_params(SequencerRegistryConfig::placeholder()).with_params(own);

        let before = schedule_for(chain.clone(), ACTIVATION - 1).unwrap();
        assert_eq!(before.protocol_limits(ACTIVATION - 1), Some(ProtocolLimits::DEFAULT));
        let from = schedule_for(chain, ACTIVATION).unwrap();
        assert_eq!(from.protocol_limits(ACTIVATION), Some(own));
        assert_eq!(from.spec_id(ACTIVATION - 1), None, "the chain's own activation holds");
    }

    /// A chain whose table schedules Satin without the limits is refused, not run on a default.
    #[test]
    fn test_a_real_satin_block_without_the_chains_limits_is_refused() {
        let chain = scheduled().with_params(SequencerRegistryConfig::placeholder());
        let error = schedule_for(chain, ACTIVATION).unwrap_err().to_string();
        assert!(error.contains("ProtocolLimits params are not configured"), "{error}");
        assert!(error.contains(&ACTIVATION.to_string()), "{error}");

        let error = schedule_for(scheduled(), ACTIVATION).unwrap_err().to_string();
        assert!(error.contains("cannot run it"), "{error}");
    }
}
