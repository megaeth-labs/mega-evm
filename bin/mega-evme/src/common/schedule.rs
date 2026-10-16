//! The hardfork schedule a Satin run executes under, and so the protocol limits it is held to.

use mega_evm::{HardforkParams, MegaHardforkConfig, MegaHardforks, MegaSpecId, ProtocolLimits};
use serde_json::{Map, Value};

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
/// is already deployed keeps the roles in its storage), and [`ProtocolLimits::DEFAULT`].
///
/// `limits_override` replaces the fields it names of the limits that schedule carries, a
/// counterfactual either way.
pub fn satin_schedule(
    chain_id: u64,
    timestamp: u64,
    limits_override: Option<&LimitsOverride>,
) -> Result<MegaHardforkConfig> {
    schedule_for(mega_evm::hardfork_schedule(chain_id), timestamp, limits_override)
}

/// [`satin_schedule`] for a chain whose schedule is `chain`.
fn schedule_for(
    chain: MegaHardforkConfig,
    timestamp: u64,
    limits_override: Option<&LimitsOverride>,
) -> Result<MegaHardforkConfig> {
    let runs_satin = chain.spec_id(timestamp) == Some(MegaSpecId::SATIN);
    let schedule = if runs_satin { chain } else { mega_evm::all_activated_hardforks() };
    let schedule = match limits_override {
        Some(limits) => {
            let limits = limits.apply(schedule.protocol_limits(timestamp))?;
            schedule.with_params(limits)
        }
        None => schedule,
    };
    schedule.validate_schedule().map_err(|e| {
        EvmeError::InvalidInput(format!(
            "the chain's schedule runs Satin at timestamp {timestamp} but cannot run it: {e}"
        ))
    })?;
    Ok(schedule)
}

/// Protocol limits to run under in place of the chain's (`--override.limits`): a JSON object in
/// the shape a chain configuration carries [`ProtocolLimits`] in, naming only the fields it
/// replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitsOverride(Map<String, Value>);

impl LimitsOverride {
    /// The limits `base` becomes with the fields this names replaced, recursively. With no `base`
    /// the override must name every field.
    ///
    /// The result is refused when it names a field [`ProtocolLimits`] does not have, lacks one, or
    /// is a value no chain may carry ([`HardforkParams::validate`]).
    pub fn apply(&self, base: Option<ProtocolLimits>) -> Result<ProtocolLimits> {
        let invalid =
            |message: String| EvmeError::InvalidInput(format!("--override.limits: {message}"));
        let mut merged = match base {
            Some(base) => serde_json::to_value(base).expect("limits serialize"),
            None => Value::Object(Map::new()),
        };
        merge(&mut merged, &self.0);
        let limits: ProtocolLimits =
            serde_json::from_value(merged).map_err(|e| invalid(e.to_string()))?;
        limits.validate().map_err(|e| invalid(e.message))?;
        Ok(limits)
    }
}

/// Writes every field of `patch` into `target`, merging the objects both hold.
fn merge(target: &mut Value, patch: &Map<String, Value>) {
    let Value::Object(target) = target else { unreachable!("limits serialize to an object") };
    for (key, value) in patch {
        match (target.get_mut(key), value) {
            (Some(existing @ Value::Object(_)), Value::Object(patch)) => merge(existing, patch),
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Parses `--override.limits`: a JSON object given inline, or the path of a file that holds one.
/// An argument that starts like JSON (`{` or `[`) is inline.
pub fn parse_limits_override(arg: &str) -> std::result::Result<LimitsOverride, String> {
    let text = if arg.trim_start().starts_with(['{', '[']) {
        arg.to_string()
    } else {
        std::fs::read_to_string(arg).map_err(|e| format!("reading {arg}: {e}"))?
    };
    match serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))? {
        Value::Object(fields) => Ok(LimitsOverride(fields)),
        _ => Err("must be a JSON object".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mega_evm::{
        system::SequencerRegistryConfig, ChainActivation, EvmTxRuntimeLimits, MAINNET_CHAIN_ID,
        TESTNET_CHAIN_ID,
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
            let schedule = satin_schedule(chain_id, 1_700_000_000, None).unwrap();
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

        let before = schedule_for(chain.clone(), ACTIVATION - 1, None).unwrap();
        assert_eq!(before.protocol_limits(ACTIVATION - 1), Some(ProtocolLimits::DEFAULT));
        let from = schedule_for(chain, ACTIVATION, None).unwrap();
        assert_eq!(from.protocol_limits(ACTIVATION), Some(own));
        assert_eq!(from.spec_id(ACTIVATION - 1), None, "the chain's own activation holds");
    }

    /// A chain whose table schedules Satin without the limits is refused, not run on a default.
    #[test]
    fn test_a_real_satin_block_without_the_chains_limits_is_refused() {
        let chain = scheduled().with_params(SequencerRegistryConfig::placeholder());
        let error = schedule_for(chain, ACTIVATION, None).unwrap_err().to_string();
        assert!(error.contains("ProtocolLimits params are not configured"), "{error}");
        assert!(error.contains(&ACTIVATION.to_string()), "{error}");

        let error = schedule_for(scheduled(), ACTIVATION, None).unwrap_err().to_string();
        assert!(error.contains("cannot run it"), "{error}");
    }

    fn overriding(json: &str) -> LimitsOverride {
        parse_limits_override(json).unwrap()
    }

    /// An override replaces the fields it names and keeps every other one the schedule carries.
    #[test]
    fn test_an_override_replaces_only_the_fields_it_names() {
        let schedule = satin_schedule(
            MAINNET_CHAIN_ID,
            0,
            Some(&overriding(
                r#"{"txRuntimeLimits":{"txDataSizeLimit":350},"blockKvUpdateLimit":7}"#,
            )),
        )
        .unwrap();
        let expected = ProtocolLimits::DEFAULT
            .with_tx_runtime_limits(
                ProtocolLimits::DEFAULT.tx_runtime_limits.with_tx_data_size_limit(350),
            )
            .with_block_kv_update_limit(7);
        assert_eq!(schedule.protocol_limits(0), Some(expected));
    }

    /// An override applies to a chain's own Satin schedule too, and there it replaces the chain's
    /// limits; a chain whose table carries none needs the override to name every field.
    #[test]
    fn test_an_override_supplies_limits_a_chain_does_not_carry() {
        let chain = scheduled().with_params(SequencerRegistryConfig::placeholder());
        let partial = overriding(r#"{"txRuntimeLimits":{"txDataSizeLimit":350}}"#);
        let error = schedule_for(chain.clone(), ACTIVATION, Some(&partial)).unwrap_err();
        assert!(error.to_string().contains("missing field"), "{error}");

        let loosest = serde_json::to_string(&ProtocolLimits::loosest()).unwrap();
        let schedule = schedule_for(chain, ACTIVATION, Some(&overriding(&loosest))).unwrap();
        assert_eq!(schedule.protocol_limits(ACTIVATION), Some(ProtocolLimits::loosest()));
    }

    /// An override is held to what a chain configuration is: no unknown field, no value the
    /// limits' own check refuses.
    #[test]
    fn test_an_override_no_chain_may_carry_is_refused() {
        let refused = |json: &str| {
            satin_schedule(MAINNET_CHAIN_ID, 0, Some(&overriding(json))).unwrap_err().to_string()
        };
        let unknown = refused(r#"{"txDataSizeLimit":350}"#);
        assert!(unknown.contains("unknown field `txDataSizeLimit`"), "{unknown}");
        let zero = refused(r#"{"txRuntimeLimits":{"txKvUpdateLimit":0}}"#);
        assert!(zero.contains("tx_kv_update_limit must not be zero"), "{zero}");
        let uncapped =
            refused(r#"{"txRuntimeLimits":{"oracleAccessComputeGasLimit":18446744073709551615}}"#);
        assert!(uncapped.contains("oracle_access_compute_gas_limit"), "{uncapped}");
        let not_a_number = refused(r#"{"blockTxsDataLimit":"1"}"#);
        assert!(not_a_number.starts_with("Invalid input: --override.limits:"), "{not_a_number}");
    }

    /// The flag takes an object inline or from a file, and nothing but an object.
    #[test]
    fn test_the_override_parses_inline_or_from_a_file() {
        let inline = r#" {"blockKvUpdateLimit": 7}"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limits.json");
        std::fs::write(&path, inline).unwrap();
        assert_eq!(parse_limits_override(inline), parse_limits_override(path.to_str().unwrap()));
        assert!(parse_limits_override("[1]").unwrap_err().contains("must be a JSON object"));
        assert!(parse_limits_override("{").unwrap_err().contains("not JSON"));
        let missing = dir.path().join("absent.json");
        assert!(parse_limits_override(missing.to_str().unwrap()).unwrap_err().contains("reading"));
    }
}
