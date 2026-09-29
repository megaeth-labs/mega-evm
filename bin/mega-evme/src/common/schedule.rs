//! The hardfork schedule a Satin run executes under, and so the protocol limits it is held to.

use std::sync::OnceLock;

use mega_evm::{
    ChainActivation, HardforkParams, MegaHardforkConfig, MegaHardforks, MegaSpecId, ProtocolLimits,
    SatinChainConfig,
};
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
///
/// The chain's own schedule is the one its genesis file gives, when the run was given the file
/// (`--genesis`), and otherwise the Satin engine's table ([`mega_evm::hardfork_schedule`]).
pub fn satin_schedule(
    chain_id: u64,
    timestamp: u64,
    limits_override: Option<&LimitsOverride>,
) -> Result<MegaHardforkConfig> {
    schedule_for(chain_schedule(GENESIS.get(), chain_id)?, timestamp, limits_override)
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

/// The schedule `chain_id` runs on: `genesis`'s, which must be the same chain's, or the Satin
/// engine's table without one.
fn chain_schedule(genesis: Option<&GenesisChain>, chain_id: u64) -> Result<MegaHardforkConfig> {
    let Some(genesis) = genesis else { return Ok(mega_evm::hardfork_schedule(chain_id)) };
    if genesis.chain_id != chain_id {
        return Err(EvmeError::InvalidInput(format!(
            "--genesis configures chain {}, and the run is on chain {chain_id}",
            genesis.chain_id
        )));
    }
    match &genesis.satin {
        Some(satin) => satin.hardforks().map_err(|e| {
            EvmeError::InvalidInput(format!("--genesis: the schedule it makes is refused: {e}"))
        }),
        None => Ok(MegaHardforkConfig::new()),
    }
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

    /// A genesis file whose `config` carries `chainId` and the Satin keys: Satin at
    /// [`ACTIVATION`], registry seeds that are not the placeholder's, and a KV limit of one.
    fn genesis_file(dir: &tempfile::TempDir, chain_id: u64) -> (String, SatinChainConfig) {
        let satin = SatinChainConfig {
            activation_time: ACTIVATION,
            sequencer_registry: SequencerRegistryConfig {
                initial_system_address: alloy_primitives::Address::repeat_byte(0x11),
                initial_sequencer: alloy_primitives::Address::repeat_byte(0x22),
                initial_admin: alloy_primitives::Address::repeat_byte(0x33),
                initial_from_block: 1,
                min_rotation_delay: 100,
            },
            protocol_limits: ProtocolLimits::DEFAULT.with_tx_runtime_limits(
                ProtocolLimits::DEFAULT.tx_runtime_limits.with_tx_kv_update_limit(1),
            ),
        };
        let mut config = serde_json::to_value(satin).unwrap();
        config["chainId"] = serde_json::json!(chain_id);
        config["optimism"] = serde_json::json!({ "eip1559Elasticity": 6 });
        let path = dir.path().join("genesis.json");
        std::fs::write(&path, serde_json::json!({ "config": config, "alloc": {} }).to_string())
            .unwrap();
        (path.to_str().unwrap().to_string(), satin)
    }

    /// A genesis file's schedule is its chain's: Satin from its `satinTime`, its registry seeds
    /// and its limits, in place of the table's, which for a chain the engine does not know is
    /// the placeholder seeds and the default limits.
    #[test]
    fn test_a_genesis_file_gives_its_chain_its_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let (path, satin) = genesis_file(&dir, 6342);
        let genesis = parse_genesis(&path).unwrap();
        assert_eq!(genesis, GenesisChain { chain_id: 6342, satin: Some(satin) });

        let chain = chain_schedule(Some(&genesis), 6342).unwrap();
        let schedule = schedule_for(chain, ACTIVATION, None).unwrap();
        assert_eq!(schedule.spec_id(ACTIVATION - 1), None);
        assert_eq!(schedule.protocol_limits(ACTIVATION), Some(satin.protocol_limits));
        assert_eq!(
            schedule.fork_params::<SequencerRegistryConfig>(),
            Some(&satin.sequencer_registry)
        );
        let table = schedule_for(chain_schedule(None, 6342).unwrap(), ACTIVATION, None).unwrap();
        assert_eq!(
            table.fork_params::<SequencerRegistryConfig>(),
            Some(&SequencerRegistryConfig::placeholder()),
            "without the file, the unknown-chain fallback"
        );

        // Before its activation the file's chain does not run Satin: a Satin run there is the
        // same counterfactual a chain without Satin gets.
        let before = schedule_for(chain_schedule(Some(&genesis), 6342).unwrap(), 0, None).unwrap();
        assert_eq!(before.protocol_limits(0), Some(ProtocolLimits::DEFAULT));
    }

    /// A genesis file is its own chain's: a run on another chain is refused rather than run on
    /// either schedule. A file without Satin keys gives a schedule without Satin.
    #[test]
    fn test_a_genesis_file_of_another_chain_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = genesis_file(&dir, 6342);
        let genesis = parse_genesis(&path).unwrap();
        let error = chain_schedule(Some(&genesis), 6343).unwrap_err().to_string();
        assert!(error.contains("--genesis configures chain 6342"), "{error}");

        let without_satin = GenesisChain { chain_id: 6342, satin: None };
        assert_eq!(chain_schedule(Some(&without_satin), 6342).unwrap().spec_id(u64::MAX), None);
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
