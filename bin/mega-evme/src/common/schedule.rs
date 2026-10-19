//! The hardfork schedule a Satin run executes under, and the protocol limits it is held to.

use std::sync::OnceLock;

use mega_evm::{
    ChainActivation, MegaHardforkConfig, MegaHardforks, MegaSpecId, ProtocolLimits,
    SatinChainConfig,
};
use serde_json::{Map, Value};

use super::{EvmeError, Result};

/// The hardfork schedule a Satin run for chain `chain_id` at `timestamp` executes under.
///
/// A block the chain's own schedule runs on Satin runs under that schedule, with the parameters
/// it carries. A schedule that cannot run the block — its table schedules Satin without the
/// parameters Satin requires — is refused rather than filled in: the tool does not know the
/// chain's registry roles, and a placeholder in their place would report results the chain does
/// not produce.
///
/// Any other run is a counterfactual and executes under the schedule the Satin engine gives a chain
/// it does not know ([`mega_evm::all_activated_hardforks`]): Satin from genesis, and placeholder
/// registry parameters, which only a registry deployed from scratch reads (a chain whose registry
/// is already deployed keeps the roles in its storage).
///
/// The limits are not the schedule's: they are fixed per spec ([`satin_limits`]).
///
/// The chain's own schedule is the one its genesis file gives, when the run was given the file
/// (`--genesis`), and otherwise the Satin engine's table ([`mega_evm::hardfork_schedule`]). A run
/// given the file makes no counterfactual: one the file does not run on Satin — before its
/// `satinTime`, or on a file without Satin keys — is refused, since the file was given to
/// configure the run and cannot.
pub fn satin_schedule(chain_id: u64, timestamp: u64) -> Result<MegaHardforkConfig> {
    let chain = match GENESIS.get() {
        Some(genesis) => genesis_schedule(genesis, chain_id, timestamp)?,
        None => mega_evm::hardfork_schedule(chain_id),
    };
    schedule_for(chain, timestamp)
}

/// The protocol limits a Satin run is held to: Satin's ([`ProtocolLimits::for_spec`]), the ones
/// every chain's Satin blocks are held to, with the fields `limits_override` names replaced, a
/// counterfactual.
pub fn satin_limits(limits_override: Option<&LimitsOverride>) -> Result<ProtocolLimits> {
    let limits = ProtocolLimits::for_spec(MegaSpecId::SATIN);
    match limits_override {
        Some(limits_override) => limits_override.apply(limits),
        None => Ok(limits),
    }
}

/// The genesis file this run was given (`--genesis`), set once before the command runs.
static GENESIS: OnceLock<GenesisChain> = OnceLock::new();

/// A chain as its genesis file configures it: its chain id, and its Satin keys, read by the
/// parser every reader of a genesis file shares ([`SatinChainConfig::from_genesis_config`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenesisChain {
    /// The genesis file's `chainId`.
    pub chain_id: u64,
    /// The chain's Satin configuration; `None` when the file does not activate Satin.
    pub satin: Option<SatinChainConfig>,
}

/// Makes `genesis` the chain configuration of every run of its chain from now on: its schedule is
/// the one [`satin_schedule`] starts from, and its Satin activation the one the engine of a block
/// is picked by ([`chain_activation`]).
///
/// # Errors
///
/// When the process already uses another genesis file: it holds one for its lifetime.
pub fn use_genesis(genesis: GenesisChain) -> Result<()> {
    let in_use = GENESIS.get_or_init(|| genesis.clone());
    if *in_use != genesis {
        return Err(EvmeError::InvalidInput(
            "--genesis: this process already runs another genesis file".to_string(),
        ));
    }
    Ok(())
}

/// Whether the run was given a genesis file (`--genesis`).
pub fn genesis_in_use() -> bool {
    GENESIS.get().is_some()
}

/// The activation table of `chain_id`: the genesis file's when the run was given one for that
/// chain, and otherwise the Satin engine's ([`mega_evm::chain_activation`]).
pub fn chain_activation(chain_id: u64) -> Option<ChainActivation> {
    match GENESIS.get() {
        Some(genesis) if genesis.chain_id == chain_id => Some(ChainActivation {
            chain_id,
            satin: genesis.satin.map(|satin| satin.activation_time),
        }),
        _ => mega_evm::chain_activation(chain_id),
    }
}

/// Refuses a run on `chain_id` when the run was given the genesis file of another chain.
///
/// A command checks this before it picks its engine, where it knows its chain first, so that a
/// run on the wrong chain is refused for its chain rather than for a spec the file cannot
/// configure.
pub fn check_genesis_chain(chain_id: u64) -> Result<()> {
    GENESIS.get().map_or(Ok(()), |genesis| genesis.check_chain(chain_id))
}

impl GenesisChain {
    /// Refuses a run on `chain_id` unless it is this file's chain.
    fn check_chain(&self, chain_id: u64) -> Result<()> {
        if self.chain_id != chain_id {
            return Err(EvmeError::InvalidInput(format!(
                "--genesis configures chain {}, and the run is on chain {chain_id}",
                self.chain_id
            )));
        }
        Ok(())
    }
}

/// The schedule a Satin run of `chain_id` at `timestamp` takes from `genesis`: refused when the
/// file is another chain's, has no Satin keys, or activates Satin after `timestamp`.
fn genesis_schedule(
    genesis: &GenesisChain,
    chain_id: u64,
    timestamp: u64,
) -> Result<MegaHardforkConfig> {
    genesis.check_chain(chain_id)?;
    let Some(satin) = &genesis.satin else {
        return Err(EvmeError::InvalidInput(format!(
            "--genesis: the file has no Satin keys, so chain {chain_id} never runs Satin on it; \
             a Satin run needs a file that activates Satin"
        )));
    };
    if timestamp < satin.activation_time {
        return Err(EvmeError::InvalidInput(format!(
            "--genesis activates Satin at timestamp {}, and the run is at timestamp {timestamp}: \
             the file configures no Satin run before it",
            satin.activation_time
        )));
    }
    satin.hardforks().map_err(|e| {
        EvmeError::InvalidInput(format!("--genesis: the schedule it makes is refused: {e}"))
    })
}

/// Parses `--genesis`: the path of a genesis file, whose `config` object carries the chain id and
/// the Satin keys. A file that is the `config` object itself is read as one.
pub fn parse_genesis(path: &str) -> std::result::Result<GenesisChain, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let file: Value = serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))?;
    let config = file.get("config").unwrap_or(&file);
    let chain_id = config
        .get("chainId")
        .and_then(Value::as_u64)
        .ok_or("the chain configuration has no integer `chainId`")?;
    let satin = SatinChainConfig::from_genesis_config(config).map_err(|e| e.to_string())?;
    Ok(GenesisChain { chain_id, satin })
}

/// [`satin_schedule`] for a chain whose schedule is `chain`.
fn schedule_for(chain: MegaHardforkConfig, timestamp: u64) -> Result<MegaHardforkConfig> {
    let runs_satin = chain.spec_id(timestamp) == Some(MegaSpecId::SATIN);
    let schedule = if runs_satin { chain } else { mega_evm::all_activated_hardforks() };
    schedule.validate_schedule().map_err(|e| {
        EvmeError::InvalidInput(format!(
            "the chain's schedule runs Satin at timestamp {timestamp} but cannot run it: {e}"
        ))
    })?;
    Ok(schedule)
}

/// Protocol limits to run under in place of the spec's (`--override.limits`): a JSON object in
/// [`ProtocolLimits`]' shape, naming only the fields it replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitsOverride(Map<String, Value>);

impl LimitsOverride {
    /// The limits `base` becomes with the fields this names replaced, recursively.
    ///
    /// The result is refused when it names a field [`ProtocolLimits`] does not have, or a value
    /// of the wrong type. Any value of the right type is run: an override is a counterfactual, and
    /// a limit of zero, or `u64::MAX` for both detention caps, which turns detention off, is one
    /// a tool may ask for.
    pub fn apply(&self, base: ProtocolLimits) -> Result<ProtocolLimits> {
        let invalid =
            |message: String| EvmeError::InvalidInput(format!("--override.limits: {message}"));
        let mut merged = serde_json::to_value(base).expect("limits serialize");
        merge(&mut merged, &self.0);
        serde_json::from_value(merged).map_err(|e| invalid(e.to_string()))
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
        system::SequencerRegistryConfig, ChainActivation, MAINNET_CHAIN_ID, TESTNET_CHAIN_ID,
    };

    const ACTIVATION: u64 = 1_800_000_000;

    fn scheduled() -> MegaHardforkConfig {
        ChainActivation { chain_id: MAINNET_CHAIN_ID, satin: Some(ACTIVATION) }.hardforks()
    }

    /// A counterfactual runs on the engine's schedule for an unknown chain, whatever the chain:
    /// Satin from genesis and the placeholder registry.
    #[test]
    fn test_a_counterfactual_runs_on_the_engines_fallback_schedule() {
        for chain_id in [MAINNET_CHAIN_ID, TESTNET_CHAIN_ID, 6342, 1] {
            let schedule = satin_schedule(chain_id, 1_700_000_000).unwrap();
            assert_eq!(schedule.spec_id(0), Some(MegaSpecId::SATIN), "chain {chain_id}");
            assert_eq!(
                schedule.fork_params::<SequencerRegistryConfig>(),
                Some(&SequencerRegistryConfig::placeholder())
            );
            assert_eq!(schedule.validate_schedule(), Ok(()));
        }
    }

    /// Before a chain's scheduled activation a Satin run is a counterfactual on the fallback
    /// schedule; from it on, the run takes the chain's schedule and its registry roles.
    #[test]
    fn test_a_real_satin_block_runs_on_the_chains_own_schedule() {
        let roles = SequencerRegistryConfig {
            initial_system_address: alloy_primitives::Address::repeat_byte(0x11),
            ..SequencerRegistryConfig::placeholder()
        };
        let chain = scheduled().with_params(roles);

        let before = schedule_for(chain.clone(), ACTIVATION - 1).unwrap();
        assert_eq!(
            before.fork_params::<SequencerRegistryConfig>(),
            Some(&SequencerRegistryConfig::placeholder())
        );
        let from = schedule_for(chain, ACTIVATION).unwrap();
        assert_eq!(from.fork_params::<SequencerRegistryConfig>(), Some(&roles));
        assert_eq!(from.spec_id(ACTIVATION - 1), None, "the chain's own activation holds");
    }

    /// A chain whose table schedules Satin without its registry roles is refused, not run on a
    /// placeholder.
    #[test]
    fn test_a_real_satin_block_without_the_chains_params_is_refused() {
        let error = schedule_for(scheduled(), ACTIVATION).unwrap_err().to_string();
        assert!(error.contains("SequencerRegistryConfig params are not configured"), "{error}");
        assert!(error.contains(&ACTIVATION.to_string()), "{error}");
        assert!(error.contains("cannot run it"), "{error}");
    }

    /// Without an override a run is held to Satin's limits, whatever the chain.
    #[test]
    fn test_a_run_is_held_to_the_specs_limits() {
        assert_eq!(satin_limits(None).unwrap(), ProtocolLimits::for_spec(MegaSpecId::SATIN));
    }

    fn overriding(json: &str) -> LimitsOverride {
        parse_limits_override(json).unwrap()
    }

    /// An override replaces the fields it names and keeps every other one of Satin's.
    #[test]
    fn test_an_override_replaces_only_the_fields_it_names() {
        let limits = satin_limits(Some(&overriding(
            r#"{"txRuntimeLimits":{"txDataSizeLimit":350},"blockTxsDataLimit":7}"#,
        )))
        .unwrap();
        let satin = ProtocolLimits::for_spec(MegaSpecId::SATIN);
        let expected = satin
            .with_tx_runtime_limits(satin.tx_runtime_limits.with_tx_data_size_limit(350))
            .with_block_txs_data_limit(7);
        assert_eq!(limits, expected);
    }

    /// An override is held to the limits' shape: no unknown field, no value of the wrong type.
    /// Any value of the right type is a counterfactual the tool runs: a limit of zero, or
    /// detention turned off.
    #[test]
    fn test_an_override_is_held_to_the_limits_shape() {
        let refused = |json: &str| satin_limits(Some(&overriding(json))).unwrap_err().to_string();
        let unknown = refused(r#"{"txDataSizeLimit":350}"#);
        assert!(unknown.contains("unknown field `txDataSizeLimit`"), "{unknown}");
        let not_a_number = refused(r#"{"blockTxsDataLimit":"1"}"#);
        assert!(not_a_number.starts_with("Invalid input: --override.limits:"), "{not_a_number}");

        let zero =
            satin_limits(Some(&overriding(r#"{"txRuntimeLimits":{"frameDataSizeLimit":0}}"#)))
                .unwrap();
        assert_eq!(zero.tx_runtime_limits.frame_data_size_limit, 0);
        let undetained = serde_json::to_string(&ProtocolLimits::no_limits()).unwrap();
        assert_eq!(
            satin_limits(Some(&overriding(&undetained))).unwrap(),
            ProtocolLimits::no_limits()
        );
    }

    /// A genesis file whose `config` carries `chainId` and the Satin keys: Satin at
    /// [`ACTIVATION`], and registry seeds that are not the placeholder's.
    fn genesis_file(dir: &tempfile::TempDir, chain_id: u64) -> (String, SatinChainConfig) {
        let satin = SatinChainConfig {
            activation_time: ACTIVATION,
            sequencer_registry: SequencerRegistryConfig {
                initial_system_address: alloy_primitives::Address::repeat_byte(0x11),
                initial_sequencer: alloy_primitives::Address::repeat_byte(0x22),
                initial_admin: alloy_primitives::Address::repeat_byte(0x33),
                min_rotation_delay: 100,
            },
        };
        let mut config = serde_json::to_value(satin).unwrap();
        config["chainId"] = serde_json::json!(chain_id);
        config["optimism"] = serde_json::json!({ "eip1559Elasticity": 6 });
        let path = dir.path().join("genesis.json");
        std::fs::write(&path, serde_json::json!({ "config": config, "alloc": {} }).to_string())
            .unwrap();
        (path.to_str().unwrap().to_string(), satin)
    }

    /// A genesis file's schedule is its chain's: Satin from its `satinTime` and its registry seeds,
    /// in place of the table's, which for a chain the engine does not know is the placeholder
    /// seeds.
    #[test]
    fn test_a_genesis_file_gives_its_chain_its_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let (path, satin) = genesis_file(&dir, 6342);
        let genesis = parse_genesis(&path).unwrap();
        assert_eq!(genesis, GenesisChain { chain_id: 6342, satin: Some(satin) });

        for timestamp in [ACTIVATION, u64::MAX] {
            let chain = genesis_schedule(&genesis, 6342, timestamp).unwrap();
            let schedule = schedule_for(chain, timestamp).unwrap();
            assert_eq!(schedule.spec_id(ACTIVATION - 1), None);
            assert_eq!(
                schedule.fork_params::<SequencerRegistryConfig>(),
                Some(&satin.sequencer_registry)
            );
        }
        let table = mega_evm::hardfork_schedule(6342);
        let table = schedule_for(table, ACTIVATION).unwrap();
        assert_eq!(
            table.fork_params::<SequencerRegistryConfig>(),
            Some(&SequencerRegistryConfig::placeholder()),
            "without the file, the unknown-chain fallback"
        );

        // Before its activation the file configures no Satin run: the run is refused, not made
        // the counterfactual a run without the file would be.
        for timestamp in [0, ACTIVATION - 1] {
            let error = genesis_schedule(&genesis, 6342, timestamp).unwrap_err().to_string();
            assert!(
                error.contains(&format!(
                    "--genesis activates Satin at timestamp {ACTIVATION}, and the run is at \
                     timestamp {timestamp}"
                )),
                "{error}"
            );
        }
    }

    /// A genesis file is its own chain's: a run on another chain is refused rather than run on
    /// either schedule, and so is a Satin run on a file without Satin keys, at any timestamp.
    #[test]
    fn test_a_genesis_file_of_another_chain_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = genesis_file(&dir, 6342);
        let genesis = parse_genesis(&path).unwrap();
        for timestamp in [0, ACTIVATION] {
            let error = genesis_schedule(&genesis, 6343, timestamp).unwrap_err().to_string();
            assert!(error.contains("--genesis configures chain 6342"), "{error}");
            assert!(genesis.check_chain(6343).is_err());
        }
        assert!(genesis.check_chain(6342).is_ok());

        let without_satin = GenesisChain { chain_id: 6342, satin: None };
        for timestamp in [0, u64::MAX] {
            let error = genesis_schedule(&without_satin, 6342, timestamp).unwrap_err().to_string();
            assert!(error.contains("--genesis: the file has no Satin keys"), "{error}");
        }
        let error = genesis_schedule(&without_satin, 6343, 0).unwrap_err().to_string();
        assert!(error.contains("--genesis configures chain 6342"), "the chain first: {error}");
    }

    /// The file is read as a genesis file or as its `config` object; one without a chain id, or
    /// whose Satin keys the shared parser refuses, is refused with the parser's reason.
    #[test]
    fn test_the_genesis_file_is_parsed_by_the_shared_parser() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, json: Value| {
            let path = dir.path().join(name);
            std::fs::write(&path, json.to_string()).unwrap();
            path.to_str().unwrap().to_string()
        };
        let bare = write("config.json", serde_json::json!({ "chainId": 7 }));
        assert_eq!(parse_genesis(&bare), Ok(GenesisChain { chain_id: 7, satin: None }));
        let no_chain_id = write("no-id.json", serde_json::json!({ "config": {} }));
        assert!(parse_genesis(&no_chain_id).unwrap_err().contains("chainId"));
        let partial = write("partial.json", serde_json::json!({ "chainId": 7, "satinTime": 0 }));
        let error = parse_genesis(&partial).unwrap_err();
        assert!(error.contains("lacks `satinInitialSystemAddress`"), "{error}");
        assert!(parse_genesis(dir.path().join("absent").to_str().unwrap())
            .unwrap_err()
            .contains("reading"));
    }

    /// The flag takes an object inline or from a file, and nothing but an object.
    #[test]
    fn test_the_override_parses_inline_or_from_a_file() {
        let inline = r#" {"blockTxsDataLimit": 7}"#;
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
