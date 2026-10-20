//! The hardfork schedule a Satin run executes under, and the protocol limits it is held to.

use std::sync::OnceLock;

use mega_evm::{
    alloy_hardforks::ForkCondition, ChainActivation, MegaHardfork, MegaHardforkConfig,
    MegaHardforks, MegaSpecId, ProtocolLimits, SatinChainConfig,
};
use serde_json::{Map, Value};

use super::{EvmeError, Result};

/// The hardfork schedule a Satin run for chain `chain_id` at `timestamp` executes under.
///
/// A block the chain's own schedule runs on Satin runs under that schedule. A schedule the engine
/// refuses ([`MegaHardforks::validate_schedule`]) is refused rather than replaced: a run under
/// another schedule would report results the chain does not produce.
///
/// Any other run is a counterfactual — `--override.spec Satin`, or `run` and `tx` with
/// `--spec Satin` — and executes under the chain's own schedule with Satin from genesis
/// ([`counterfactual`]), so no block of the run is Satin's activation block. Satin carries no
/// parameter: the `SequencerRegistry`'s roles are what the state the run executes on holds.
///
/// The limits are not the schedule's: they are fixed per spec ([`satin_limits`]).
///
/// The chain's own schedule is the one its genesis file gives, when the run was given the file
/// (`--genesis`), and otherwise the Satin engine's table ([`mega_evm::hardfork_schedule`]), which
/// knows `MegaETH` mainnet and testnet and no other chain. A run given the file makes no
/// counterfactual: one the file does not run on Satin — before its `satinTime`, or on a file
/// without Satin keys — is refused, since the file was given to configure the run and cannot.
pub fn satin_schedule(chain_id: u64, timestamp: u64) -> Result<MegaHardforkConfig> {
    let chain = match GENESIS.get() {
        Some(genesis) => Some(genesis_schedule(genesis, chain_id, timestamp)?),
        None => mega_evm::hardfork_schedule(chain_id),
    };
    schedule_for(chain, timestamp)
}

/// The schedule of a Satin counterfactual on the chain whose schedule is `chain`: that schedule
/// with Satin from genesis, or, for a chain neither the engine's table nor a genesis file names,
/// the engine's base forks with Satin from genesis.
fn counterfactual(chain: Option<MegaHardforkConfig>) -> MegaHardforkConfig {
    chain.unwrap_or_default().with(MegaHardfork::Satin, ForkCondition::Timestamp(0))
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

/// A chain as its genesis file configures it: its chain id, and its Satin key, read by the parser
/// every reader of a genesis file shares ([`SatinChainConfig::from_genesis_config`]).
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
/// the Satin key. A file that is the `config` object itself is read as one.
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

/// [`satin_schedule`] for a chain whose schedule is `chain`, `None` for a chain nothing names.
fn schedule_for(chain: Option<MegaHardforkConfig>, timestamp: u64) -> Result<MegaHardforkConfig> {
    let schedule = match chain {
        Some(chain) if chain.spec_id(timestamp) == Some(MegaSpecId::SATIN) => chain,
        chain => counterfactual(chain),
    };
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
    use mega_evm::{ChainActivation, MAINNET_CHAIN_ID, TESTNET_CHAIN_ID};

    const ACTIVATION: u64 = 1_800_000_000;

    fn scheduled() -> MegaHardforkConfig {
        ChainActivation { chain_id: MAINNET_CHAIN_ID, satin: Some(ACTIVATION) }.hardforks()
    }

    /// A counterfactual runs on the chain's own schedule with Satin from genesis, whatever the
    /// chain; a chain the engine does not know gets the engine's base forks with Satin from
    /// genesis. No block of a counterfactual is Satin's activation block.
    #[test]
    fn test_a_counterfactual_runs_on_the_chains_schedule_with_satin_from_genesis() {
        for chain_id in [MAINNET_CHAIN_ID, TESTNET_CHAIN_ID, 6342, 1] {
            let schedule = satin_schedule(chain_id, 1_700_000_000).unwrap();
            assert_eq!(schedule.spec_id(0), Some(MegaSpecId::SATIN), "chain {chain_id}");
            assert!(!schedule.is_satin_activation_block(0, 1_700_000_000), "chain {chain_id}");
            assert_eq!(schedule.validate_schedule(), Ok(()));
        }
        let known = mega_evm::hardfork_schedule(MAINNET_CHAIN_ID).unwrap();
        let counterfactual = satin_schedule(MAINNET_CHAIN_ID, 1_700_000_000).unwrap();
        let osaka = mega_evm::alloy_hardforks::EthereumHardfork::Osaka;
        assert_eq!(counterfactual.get(osaka), known.get(osaka), "the chain's own forks");
    }

    /// Before a chain's scheduled activation a Satin run is a counterfactual on the chain's
    /// schedule with Satin from genesis; from it on, the run takes the chain's schedule, which
    /// needs no parameter.
    #[test]
    fn test_a_real_satin_block_runs_on_the_chains_own_schedule() {
        let before = schedule_for(Some(scheduled()), ACTIVATION - 1).unwrap();
        assert_eq!(before.spec_id(0), Some(MegaSpecId::SATIN), "Satin from genesis");
        let from = schedule_for(Some(scheduled()), ACTIVATION).unwrap();
        assert_eq!(from.spec_id(ACTIVATION - 1), None, "the chain's own activation holds");
        assert_eq!(from.spec_id(ACTIVATION), Some(MegaSpecId::SATIN));
    }

    /// A chain whose schedule the engine refuses is refused, not run on another schedule: one
    /// that activates Satin before a fork Satin runs on.
    #[test]
    fn test_a_real_satin_block_on_a_refused_schedule_is_refused() {
        use mega_evm::alloy_hardforks::{EthereumHardfork, ForkCondition};
        let late = ForkCondition::Timestamp(ACTIVATION + 1);
        let chain = scheduled().with(EthereumHardfork::Osaka, late);
        let error = schedule_for(Some(chain), ACTIVATION).unwrap_err().to_string();
        assert!(error.contains("the Osaka fork it runs on is not active"), "{error}");
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

    /// A genesis file whose `config` carries `chainId` and the Satin key: Satin at
    /// [`ACTIVATION`].
    fn genesis_file(dir: &tempfile::TempDir, chain_id: u64) -> (String, SatinChainConfig) {
        let satin = SatinChainConfig { activation_time: ACTIVATION };
        let mut config = serde_json::to_value(satin).unwrap();
        config["chainId"] = serde_json::json!(chain_id);
        config["optimism"] = serde_json::json!({ "eip1559Elasticity": 6 });
        let path = dir.path().join("genesis.json");
        std::fs::write(&path, serde_json::json!({ "config": config, "alloc": {} }).to_string())
            .unwrap();
        (path.to_str().unwrap().to_string(), satin)
    }

    /// A genesis file's schedule is its chain's: Satin from its `satinTime`. Without the file the
    /// engine's table does not know the chain, and a Satin run on it is a counterfactual.
    #[test]
    fn test_a_genesis_file_gives_its_chain_its_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let (path, satin) = genesis_file(&dir, 6342);
        let genesis = parse_genesis(&path).unwrap();
        assert_eq!(genesis, GenesisChain { chain_id: 6342, satin: Some(satin) });

        for timestamp in [ACTIVATION, u64::MAX] {
            let chain = genesis_schedule(&genesis, 6342, timestamp).unwrap();
            let schedule = schedule_for(Some(chain), timestamp).unwrap();
            assert_eq!(schedule.spec_id(ACTIVATION - 1), None);
            assert_eq!(schedule.spec_id(ACTIVATION), Some(MegaSpecId::SATIN));
        }
        assert!(mega_evm::hardfork_schedule(6342).is_none(), "the engine table does not know it");
        let counterfactual = schedule_for(None, ACTIVATION).unwrap();
        assert_eq!(
            counterfactual.spec_id(0),
            Some(MegaSpecId::SATIN),
            "without the file, a counterfactual from genesis"
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
    /// whose Satin keys the shared parser refuses, is refused with the parser's reason: an old
    /// file that still names the registry's roles among them.
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
        let satin = write("satin.json", serde_json::json!({ "chainId": 7, "satinTime": 0 }));
        let expected =
            GenesisChain { chain_id: 7, satin: Some(SatinChainConfig { activation_time: 0 }) };
        assert_eq!(parse_genesis(&satin), Ok(expected));
        let old = write(
            "old.json",
            serde_json::json!({ "chainId": 7, "satinTime": 0, "satinInitialAdmin": "0x33" }),
        );
        let error = parse_genesis(&old).unwrap_err();
        assert!(
            error.contains("`satinInitialAdmin` is not a Satin chain-configuration key"),
            "{error}"
        );
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
