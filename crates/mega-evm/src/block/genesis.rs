//! The Satin keys of a chain configuration: when Satin activates, and the parameters it carries.
//!
//! [`SatinChainConfig`] reads them and states the format, which is provisional.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::{fmt, str::FromStr};
use std::{
    borrow::ToOwned,
    collections::BTreeMap,
    string::{String, ToString},
};

use alloy_hardforks::ForkCondition;
use alloy_primitives::Address;
use serde::{
    de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor},
    ser::SerializeMap,
    Deserialize, Deserializer, Serialize, Serializer,
};

use crate::{
    system::SequencerRegistryConfig, EvmTxRuntimeLimits, HardforkParams, MegaHardfork,
    MegaHardforkConfig, MegaHardforks, ProtocolLimits, ScheduleError,
};

/// The key of Satin's activation timestamp.
pub const SATIN_TIME_KEY: &str = "satinTime";

/// Every Satin key of a chain configuration, in the order [`SatinChainConfig`] writes them.
pub const SATIN_CHAIN_CONFIG_KEYS: [&str; 17] = [
    SATIN_TIME_KEY,
    "satinInitialSystemAddress",
    "satinInitialSequencer",
    "satinInitialAdmin",
    "satinInitialFromBlock",
    "satinMinRotationDelay",
    "satinTxDataSizeLimit",
    "satinFrameDataSizeLimit",
    "satinTxKvUpdateLimit",
    "satinFrameKvUpdateLimit",
    "satinTxStateGasLimit",
    "satinBlockEnvAccessComputeGasLimit",
    "satinOracleAccessComputeGasLimit",
    "satinBlockExecutionGasLimit",
    "satinBlockStateGasLimit",
    "satinBlockTxsDataLimit",
    "satinBlockKvUpdateLimit",
];

/// The prefix every Satin key starts with, compared without regard to case.
const PREFIX: &str = "satin";

/// The key `serde_json` built with `arbitrary_precision` hands a number over under.
const ARBITRARY_PRECISION_NUMBER: &str = "$serde_json::private::Number";

/// A chain's Satin configuration, as its genesis file's `config` object carries it: when Satin
/// activates, and the two params types it requires.
///
/// **The key format is provisional.** It is fixed when a network first publishes a genesis file
/// that carries it; until then a change to it is a change to this parser and to every genesis
/// file written against it.
///
/// Every reader of a chain's Satin parameters — a node, a stateless validator, a genesis
/// generator, a replay tool — parses them here ([`SatinChainConfig::from_genesis_config`]), so
/// the key names and the rules cannot drift apart between them: a reader that took another value
/// for one key would execute another chain.
///
/// The keys sit at the top level of the genesis file's `config` object, beside the other forks'
/// keys, flat and camelCase, each prefixed `satin`:
///
/// | Key                                  | Value              | Field                                                         |
/// | ------------------------------------ | ------------------ | ------------------------------------------------------------- |
/// | `satinTime`                          | integer            | the activation timestamp                                      |
/// | `satinInitialSystemAddress`          | address            | [`SequencerRegistryConfig::initial_system_address`]           |
/// | `satinInitialSequencer`              | address            | [`SequencerRegistryConfig::initial_sequencer`]                |
/// | `satinInitialAdmin`                  | address            | [`SequencerRegistryConfig::initial_admin`]                    |
/// | `satinInitialFromBlock`              | integer            | [`SequencerRegistryConfig::initial_from_block`]               |
/// | `satinMinRotationDelay`              | integer            | [`SequencerRegistryConfig::min_rotation_delay`]               |
/// | `satinTxDataSizeLimit`               | integer            | [`EvmTxRuntimeLimits::tx_data_size_limit`]                    |
/// | `satinFrameDataSizeLimit`            | integer            | [`EvmTxRuntimeLimits::frame_data_size_limit`]                 |
/// | `satinTxKvUpdateLimit`               | integer            | [`EvmTxRuntimeLimits::tx_kv_update_limit`]                    |
/// | `satinFrameKvUpdateLimit`            | integer            | [`EvmTxRuntimeLimits::frame_kv_update_limit`]                 |
/// | `satinTxStateGasLimit`               | integer            | [`EvmTxRuntimeLimits::tx_state_gas_limit`]                    |
/// | `satinBlockEnvAccessComputeGasLimit` | integer            | [`EvmTxRuntimeLimits::block_env_access_compute_gas_limit`]    |
/// | `satinOracleAccessComputeGasLimit`   | integer            | [`EvmTxRuntimeLimits::oracle_access_compute_gas_limit`]       |
/// | `satinBlockExecutionGasLimit`        | integer            | [`ProtocolLimits::block_execution_gas_limit`]                 |
/// | `satinBlockStateGasLimit`            | integer            | [`ProtocolLimits::block_state_gas_limit`]                     |
/// | `satinBlockTxsDataLimit`             | integer            | [`ProtocolLimits::block_txs_data_limit`]                      |
/// | `satinBlockKvUpdateLimit`            | integer            | [`ProtocolLimits::block_kv_update_limit`]                     |
///
/// Each key is the `satin` prefix before the field's key in the JSON of its params type, so the
/// flat names follow the types' own serde names. An integer is a JSON number that fits in 64
/// unsigned bits, `u64::MAX` included, which a writer must emit exactly; an address is a hex
/// string.
///
/// The rules:
///
/// - A configuration with no key starting with `satin` does not activate Satin.
/// - One that has `satinTime` carries every other key: a key left out is refused, never read as a
///   default. A `satin` key without `satinTime` is refused too.
/// - A key starting with `satin`, in any case, that is not in the table is refused, so a misspelled
///   key cannot stand in for the one it meant. Every other key is another fork's and is not read.
/// - A key given twice is refused wherever the deserializer shows both; a `serde_json::Value` has
///   already kept the last one.
/// - A value of the wrong type is refused, naming its key; a value its params type refuses
///   ([`HardforkParams::validate`]) is refused, and so is a schedule
///   [`validate_schedule`](MegaHardforks::validate_schedule) refuses. One object is read as an
///   integer: `{"$serde_json::private::Number": "<digits>"}`, the form `serde_json` hands a number
///   over in when its `arbitrary_precision` feature is on. A deserializer shows that form and the
///   same object written into the file alike, so both are read as the number; every reader parses
///   here, so every reader reads it the same.
///
/// The parser reads no other fork's key. Satin runs on the OP Karst fork and the Ethereum forks
/// Karst includes, Osaka among them, and [`hardforks`](Self::hardforks) schedules those at genesis;
/// a node, whose own schedule times those forks, must check when it loads the configuration that
/// every one of them is active at `satinTime`, and refuse it otherwise. The engine executes every
/// Satin block on Karst, so a configuration that activates Satin before one of them has the node
/// and the engine apply different rules to the same block.
///
/// A flat key is one entry of the object, so a checker that freezes a configuration key by key
/// freezes each parameter on its own and names it when it changes. It does not stop a key being
/// added later; a rule that refuses a new key starting with `satin` once the configuration was
/// first loaded does.
///
/// It serializes to the flat keys it is read from, so a generator that writes a genesis file with
/// it writes what every reader parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SatinChainConfig {
    /// The timestamp Satin activates at.
    pub activation_time: u64,
    /// The `SequencerRegistry`'s bootstrap roles.
    pub sequencer_registry: SequencerRegistryConfig,
    /// The limits the protocol holds blocks and transactions to.
    pub protocol_limits: ProtocolLimits,
}

impl SatinChainConfig {
    /// Reads the Satin keys of a genesis file's `config` object from `config`, which any
    /// self-describing deserializer of the object can supply: a `&serde_json::Value`, or the
    /// object's text through a `serde_json::Deserializer`, whose caller checks that the input ends
    /// with the object. A node's own chain-config type supplies one by serializing to a
    /// `serde_json::Value`.
    ///
    /// `Ok(None)` when the configuration does not activate Satin; otherwise the configuration,
    /// with both params types valid and the schedule it makes one
    /// [`validate_schedule`](MegaHardforks::validate_schedule) accepts.
    ///
    /// # Errors
    ///
    /// Every rule of the [format](Self) that the configuration breaks, the first one found.
    pub fn from_genesis_config<'de, D: Deserializer<'de>>(
        config: D,
    ) -> Result<Option<Self>, SatinChainConfigError> {
        let Entries { entries, duplicate } = config
            .deserialize_map(EntriesVisitor)
            .map_err(|error| SatinChainConfigError::Malformed(error.to_string()))?;
        if let Some(key) = duplicate {
            return Err(SatinChainConfigError::DuplicateKey(key));
        }
        let mut keys = Keys::new(entries)?;
        let Some(activation_time) = keys.optional_u64(SATIN_TIME_KEY)? else {
            return match keys.first_remaining() {
                None => Ok(None),
                Some(_) => Err(SatinChainConfigError::MissingKey(SATIN_TIME_KEY)),
            };
        };
        let sequencer_registry = SequencerRegistryConfig {
            initial_system_address: keys.address("satinInitialSystemAddress")?,
            initial_sequencer: keys.address("satinInitialSequencer")?,
            initial_admin: keys.address("satinInitialAdmin")?,
            initial_from_block: keys.u64("satinInitialFromBlock")?,
            min_rotation_delay: keys.u64("satinMinRotationDelay")?,
        };
        let protocol_limits = ProtocolLimits {
            tx_runtime_limits: EvmTxRuntimeLimits {
                tx_data_size_limit: keys.u64("satinTxDataSizeLimit")?,
                frame_data_size_limit: keys.u64("satinFrameDataSizeLimit")?,
                tx_kv_update_limit: keys.u64("satinTxKvUpdateLimit")?,
                frame_kv_update_limit: keys.u64("satinFrameKvUpdateLimit")?,
                tx_state_gas_limit: keys.u64("satinTxStateGasLimit")?,
                block_env_access_compute_gas_limit: keys
                    .u64("satinBlockEnvAccessComputeGasLimit")?,
                oracle_access_compute_gas_limit: keys.u64("satinOracleAccessComputeGasLimit")?,
            },
            block_execution_gas_limit: keys.u64("satinBlockExecutionGasLimit")?,
            block_state_gas_limit: keys.u64("satinBlockStateGasLimit")?,
            block_txs_data_limit: keys.u64("satinBlockTxsDataLimit")?,
            block_kv_update_limit: keys.u64("satinBlockKvUpdateLimit")?,
        };
        let config = Self { activation_time, sequencer_registry, protocol_limits };
        config.hardforks()?;
        Ok(Some(config))
    }

    /// The hardfork schedule this configuration makes: [`MegaHardforkConfig::new`]'s base forks,
    /// Satin at [`activation_time`](Self::activation_time), and both params types attached.
    ///
    /// The base forks, every OP and Ethereum fork up to Karst, are at genesis here whatever the
    /// genesis file times them at: a node checks the file's own times against `satinTime` (see
    /// the [format](Self)).
    ///
    /// # Errors
    ///
    /// A params value its type refuses ([`HardforkParams::validate`]), or a schedule
    /// [`validate_schedule`](MegaHardforks::validate_schedule) refuses.
    pub fn hardforks(&self) -> Result<MegaHardforkConfig, ScheduleError> {
        checked(&self.sequencer_registry)?;
        checked(&self.protocol_limits)?;
        let schedule = MegaHardforkConfig::new()
            .with(MegaHardfork::Satin, ForkCondition::Timestamp(self.activation_time))
            .with_params(self.sequencer_registry)
            .with_params(self.protocol_limits);
        schedule.validate_schedule()?;
        Ok(schedule)
    }
}

/// Refuses `params` its type refuses, as [`MegaHardforks::require_params`] reports it.
fn checked<P: HardforkParams>(params: &P) -> Result<(), ScheduleError> {
    params.validate().map_err(|error| ScheduleError::InvalidParams {
        fork: P::FORK,
        params: P::NAME,
        message: error.message,
    })
}

impl Serialize for SatinChainConfig {
    /// Writes the flat keys, in [`SATIN_CHAIN_CONFIG_KEYS`] order.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Self { activation_time, sequencer_registry, protocol_limits } = self;
        let SequencerRegistryConfig {
            initial_system_address,
            initial_sequencer,
            initial_admin,
            initial_from_block,
            min_rotation_delay,
        } = sequencer_registry;
        let ProtocolLimits {
            tx_runtime_limits,
            block_execution_gas_limit,
            block_state_gas_limit,
            block_txs_data_limit,
            block_kv_update_limit,
        } = protocol_limits;
        let EvmTxRuntimeLimits {
            tx_data_size_limit,
            frame_data_size_limit,
            tx_kv_update_limit,
            frame_kv_update_limit,
            tx_state_gas_limit,
            block_env_access_compute_gas_limit,
            oracle_access_compute_gas_limit,
        } = tx_runtime_limits;
        let [time, system_address, sequencer, admin, from_block, rotation_delay, tx_data, frame_data, tx_kv, frame_kv, tx_state, block_env, oracle, block_execution, block_state, block_data, block_kv] =
            SATIN_CHAIN_CONFIG_KEYS;
        let mut map = serializer.serialize_map(Some(SATIN_CHAIN_CONFIG_KEYS.len()))?;
        map.serialize_entry(time, activation_time)?;
        map.serialize_entry(system_address, initial_system_address)?;
        map.serialize_entry(sequencer, initial_sequencer)?;
        map.serialize_entry(admin, initial_admin)?;
        map.serialize_entry(from_block, initial_from_block)?;
        map.serialize_entry(rotation_delay, min_rotation_delay)?;
        map.serialize_entry(tx_data, tx_data_size_limit)?;
        map.serialize_entry(frame_data, frame_data_size_limit)?;
        map.serialize_entry(tx_kv, tx_kv_update_limit)?;
        map.serialize_entry(frame_kv, frame_kv_update_limit)?;
        map.serialize_entry(tx_state, tx_state_gas_limit)?;
        map.serialize_entry(block_env, block_env_access_compute_gas_limit)?;
        map.serialize_entry(oracle, oracle_access_compute_gas_limit)?;
        map.serialize_entry(block_execution, block_execution_gas_limit)?;
        map.serialize_entry(block_state, block_state_gas_limit)?;
        map.serialize_entry(block_data, block_txs_data_limit)?;
        map.serialize_entry(block_kv, block_kv_update_limit)?;
        map.end()
    }
}

/// Why a chain configuration's Satin keys cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SatinChainConfigError {
    /// The configuration is not an object with string keys.
    Malformed(String),
    /// A key that starts with `satin` but is none of the Satin keys.
    UnknownKey(String),
    /// A key that starts with `satin`, given twice.
    DuplicateKey(String),
    /// A key the configuration must carry: every Satin key once `satinTime` is there, and
    /// `satinTime` once any other is.
    MissingKey(&'static str),
    /// A key whose value is not of its type.
    InvalidValue {
        /// The key.
        key: &'static str,
        /// What its value must be.
        expected: &'static str,
    },
    /// Values the params types or the schedule refuse.
    Schedule(ScheduleError),
}

impl fmt::Display for SatinChainConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(message) => {
                write!(f, "the chain configuration is malformed: {message}")
            }
            Self::UnknownKey(key) => write!(f, "`{key}` is not a Satin chain-configuration key"),
            Self::DuplicateKey(key) => write!(f, "`{key}` is given twice"),
            Self::MissingKey(key) => write!(f, "the chain configuration lacks `{key}`"),
            Self::InvalidValue { key, expected } => write!(f, "`{key}` must be {expected}"),
            Self::Schedule(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for SatinChainConfigError {}

impl From<ScheduleError> for SatinChainConfigError {
    fn from(error: ScheduleError) -> Self {
        Self::Schedule(error)
    }
}

/// The value of a key, as far as reading the Satin keys needs it: an unsigned integer, a string,
/// or anything else.
#[derive(Debug)]
enum Entry {
    Unsigned(u64),
    Text(String),
    Other,
}

/// A configuration object's entries whose key starts with [`PREFIX`], and the first such key
/// given twice, if one was.
struct Entries {
    entries: BTreeMap<String, Entry>,
    duplicate: Option<String>,
}

/// Collects a configuration object's entries whose key starts with [`PREFIX`], and skips the
/// others' values without keeping them.
struct EntriesVisitor;

impl<'de> Visitor<'de> for EntriesVisitor {
    type Value = Entries;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a chain configuration object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = BTreeMap::new();
        let mut duplicate = None;
        while let Some(key) = map.next_key::<String>()? {
            if is_satin_key(&key) {
                let entry = map.next_value::<Entry>()?;
                if entries.contains_key(&key) {
                    duplicate.get_or_insert_with(|| key.clone());
                }
                entries.insert(key, entry);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(Entries { entries, duplicate })
    }
}

/// Whether `key` starts with [`PREFIX`], in any case.
fn is_satin_key(key: &str) -> bool {
    key.get(..PREFIX.len()).is_some_and(|start| start.eq_ignore_ascii_case(PREFIX))
}

impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(EntryVisitor)
    }
}

struct EntryVisitor;

impl<'de> Visitor<'de> for EntryVisitor {
    type Value = Entry;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Entry, E> {
        Ok(Entry::Unsigned(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Entry, E> {
        Ok(u64::try_from(value).map_or(Entry::Other, Entry::Unsigned))
    }

    fn visit_u128<E: de::Error>(self, value: u128) -> Result<Entry, E> {
        Ok(u64::try_from(value).map_or(Entry::Other, Entry::Unsigned))
    }

    fn visit_i128<E: de::Error>(self, value: i128) -> Result<Entry, E> {
        Ok(u64::try_from(value).map_or(Entry::Other, Entry::Unsigned))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Entry, E> {
        Ok(Entry::Other)
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Entry, E> {
        Ok(Entry::Other)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Entry, E> {
        Ok(Entry::Text(value.to_owned()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Entry, E> {
        Ok(Entry::Other)
    }

    fn visit_none<E: de::Error>(self) -> Result<Entry, E> {
        Ok(Entry::Other)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Entry, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Entry, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Entry::Other)
    }

    /// A map is no value a key takes, but for one shape: `serde_json` built with its
    /// `arbitrary_precision` feature, which a node's dependencies may switch on, hands every number
    /// over as a one-entry map from [`ARBITRARY_PRECISION_NUMBER`] to the number's text. That
    /// number is read as the same number is read without the feature, so the feature changes
    /// nothing a reader accepts.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Entry, A::Error> {
        let mut entry = Entry::Other;
        if let Some(key) = map.next_key::<String>()? {
            if key == ARBITRARY_PRECISION_NUMBER {
                let text = map.next_value::<String>()?;
                entry = text.parse::<u64>().map_or(Entry::Other, Entry::Unsigned);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        if map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
            entry = Entry::Other;
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        }
        Ok(entry)
    }
}

/// The Satin entries of a configuration, taken one key at a time.
struct Keys(BTreeMap<String, Entry>);

impl Keys {
    /// The entries, once every one of them is known to be a Satin key.
    fn new(entries: BTreeMap<String, Entry>) -> Result<Self, SatinChainConfigError> {
        match entries.keys().find(|key| !SATIN_CHAIN_CONFIG_KEYS.contains(&key.as_str())) {
            Some(unknown) => Err(SatinChainConfigError::UnknownKey(unknown.clone())),
            None => Ok(Self(entries)),
        }
    }

    /// The first key not taken yet.
    fn first_remaining(&self) -> Option<&String> {
        self.0.keys().next()
    }

    /// The integer at `key`, which may be absent.
    fn optional_u64(&mut self, key: &'static str) -> Result<Option<u64>, SatinChainConfigError> {
        match self.0.remove(key) {
            None => Ok(None),
            Some(Entry::Unsigned(value)) => Ok(Some(value)),
            Some(_) => Err(SatinChainConfigError::InvalidValue {
                key,
                expected: "an integer from 0 to 2^64 - 1",
            }),
        }
    }

    /// The integer at `key`.
    fn u64(&mut self, key: &'static str) -> Result<u64, SatinChainConfigError> {
        self.optional_u64(key)?.ok_or(SatinChainConfigError::MissingKey(key))
    }

    /// The address at `key`: a hex string of twenty bytes.
    fn address(&mut self, key: &'static str) -> Result<Address, SatinChainConfigError> {
        let invalid = SatinChainConfigError::InvalidValue { key, expected: "a hex address" };
        match self.0.remove(key) {
            None => Err(SatinChainConfigError::MissingKey(key)),
            Some(Entry::Text(text)) => Address::from_str(&text).map_err(|_| invalid),
            Some(_) => Err(invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{constants::MAX_TX_COMPUTE_GAS, MegaSpecId, TX_BODY_SIZE};
    use alloy_primitives::address;
    use serde_json::{json, Value};

    const ACTIVATION: u64 = 1_800_000_000;

    /// A configuration whose every value differs from the others', so a key read into the wrong
    /// field shows.
    fn config() -> SatinChainConfig {
        SatinChainConfig {
            activation_time: ACTIVATION,
            sequencer_registry: SequencerRegistryConfig {
                initial_system_address: address!("0x1111111111111111111111111111111111111111"),
                initial_sequencer: address!("0x2222222222222222222222222222222222222222"),
                initial_admin: address!("0x3333333333333333333333333333333333333333"),
                initial_from_block: 4,
                min_rotation_delay: 5,
            },
            protocol_limits: ProtocolLimits {
                tx_runtime_limits: EvmTxRuntimeLimits {
                    tx_data_size_limit: 1_000_006,
                    frame_data_size_limit: 7,
                    tx_kv_update_limit: 8,
                    frame_kv_update_limit: 9,
                    tx_state_gas_limit: 10,
                    block_env_access_compute_gas_limit: 11,
                    oracle_access_compute_gas_limit: 12,
                },
                block_execution_gas_limit: 13,
                block_state_gas_limit: 14,
                block_txs_data_limit: 15,
                block_kv_update_limit: u64::MAX,
            },
        }
    }

    /// A genesis `config` object carrying [`config`] among other forks' keys.
    fn genesis_config() -> Value {
        let mut object = json!({
            "chainId": 6342,
            "homesteadBlock": 0,
            "shanghaiTime": 0,
            "rex6Time": 0,
            "rex5InitialSequencer": "0x4444444444444444444444444444444444444444",
            "optimism": { "eip1559Elasticity": 6, "eip1559Denominator": 50 },
            "blobSchedule": { "cancun": { "target": 3, "max": 6 } },
            "terminalTotalDifficultyPassed": true,
            "depositContractAddress": null,
        });
        let keys = serde_json::to_value(config()).unwrap();
        object.as_object_mut().unwrap().extend(keys.as_object().unwrap().clone());
        object
    }

    fn parse(config: &Value) -> Result<Option<SatinChainConfig>, SatinChainConfigError> {
        SatinChainConfig::from_genesis_config(config)
    }

    /// The keys a configuration writes, in order, and what reading them back gives: the same
    /// configuration, from a `serde_json::Value` and from the object's text alike.
    #[test]
    fn test_the_keys_round_trip() {
        let json = serde_json::to_string(&config()).unwrap();
        assert_eq!(
            json,
            r#"{"satinTime":1800000000,"satinInitialSystemAddress":"0x1111111111111111111111111111111111111111","satinInitialSequencer":"0x2222222222222222222222222222222222222222","satinInitialAdmin":"0x3333333333333333333333333333333333333333","satinInitialFromBlock":4,"satinMinRotationDelay":5,"satinTxDataSizeLimit":1000006,"satinFrameDataSizeLimit":7,"satinTxKvUpdateLimit":8,"satinFrameKvUpdateLimit":9,"satinTxStateGasLimit":10,"satinBlockEnvAccessComputeGasLimit":11,"satinOracleAccessComputeGasLimit":12,"satinBlockExecutionGasLimit":13,"satinBlockStateGasLimit":14,"satinBlockTxsDataLimit":15,"satinBlockKvUpdateLimit":18446744073709551615}"#
        );
        let mut written: Vec<String> =
            serde_json::from_str::<serde_json::Map<String, Value>>(&json)
                .unwrap()
                .keys()
                .cloned()
                .collect();
        written.sort();
        let mut sorted = SATIN_CHAIN_CONFIG_KEYS.map(String::from).to_vec();
        sorted.sort();
        assert_eq!(written, sorted, "every key, once");

        assert_eq!(parse(&genesis_config()), Ok(Some(config())));
        let text = genesis_config().to_string();
        assert_eq!(
            SatinChainConfig::from_genesis_config(&mut serde_json::Deserializer::from_str(&text)),
            Ok(Some(config()))
        );
    }

    /// Each flat key is `satin` before the key its field has in its params type's own JSON, so the
    /// two cannot name a field differently, and a field added to a params type fails this until the
    /// module reads it.
    #[test]
    fn test_the_keys_follow_the_params_types_own_names() {
        fn leaves(value: &Value, out: &mut Vec<String>) {
            for (key, value) in value.as_object().unwrap() {
                match value {
                    Value::Object(_) => leaves(value, out),
                    _ => out.push(key.clone()),
                }
            }
        }
        let mut names = Vec::new();
        leaves(&serde_json::to_value(config().sequencer_registry).unwrap(), &mut names);
        leaves(&serde_json::to_value(config().protocol_limits).unwrap(), &mut names);
        let mut flat: Vec<String> = names
            .iter()
            .map(|name| format!("{PREFIX}{}{}", name[..1].to_uppercase(), &name[1..]))
            .collect();
        flat.push(SATIN_TIME_KEY.into());
        flat.sort();
        let mut keys = SATIN_CHAIN_CONFIG_KEYS.map(String::from).to_vec();
        keys.sort();
        assert_eq!(flat, keys);
    }

    /// The schedule the configuration makes runs Satin from its activation, with the two params
    /// types attached, and passes the schedule's own check.
    #[test]
    fn test_the_schedule_activates_satin_with_its_params() {
        let schedule = config().hardforks().unwrap();
        assert_eq!(schedule.spec_id(ACTIVATION - 1), None);
        assert_eq!(schedule.spec_id(ACTIVATION), Some(MegaSpecId::SATIN));
        assert_eq!(
            schedule.fork_params::<SequencerRegistryConfig>(),
            Some(&config().sequencer_registry)
        );
        assert_eq!(schedule.protocol_limits(ACTIVATION), Some(config().protocol_limits));
        assert_eq!(schedule.validate_schedule(), Ok(()));
    }

    /// A configuration without a Satin key does not activate Satin; one with a Satin key but not
    /// `satinTime` is refused.
    #[test]
    fn test_without_satin_time() {
        let mut other_forks = genesis_config();
        other_forks.as_object_mut().unwrap().retain(|key, _| !key.starts_with(PREFIX));
        assert_eq!(parse(&other_forks), Ok(None));
        assert_eq!(parse(&json!({})), Ok(None));

        let mut without_time = genesis_config();
        without_time.as_object_mut().unwrap().remove(SATIN_TIME_KEY);
        assert_eq!(parse(&without_time), Err(SatinChainConfigError::MissingKey(SATIN_TIME_KEY)));
    }

    /// Every key but `satinTime`, left out, is refused by name rather than read as a default.
    #[test]
    fn test_a_missing_key_is_refused() {
        for key in &SATIN_CHAIN_CONFIG_KEYS[1..] {
            let mut config = genesis_config();
            config.as_object_mut().unwrap().remove(*key);
            assert_eq!(parse(&config), Err(SatinChainConfigError::MissingKey(key)), "{key}");
        }
    }

    /// A key that starts with `satin`, in any case, and is not one of the keys is refused, so a
    /// misspelling cannot stand in for the key it meant; a key that does not is another fork's.
    #[test]
    fn test_an_unknown_satin_key_is_refused() {
        for unknown in [
            "satinTxDataSizeLimt",
            "SatinTime",
            "SATINTIME",
            "satin",
            "satinParams",
            "satin_time",
            "satinProtocolLimits",
        ] {
            let mut config = genesis_config();
            config.as_object_mut().unwrap().insert(unknown.into(), json!({ "txDataSizeLimit": 1 }));
            assert_eq!(
                parse(&config),
                Err(SatinChainConfigError::UnknownKey(unknown.into())),
                "{unknown}"
            );
        }
        let mut config = genesis_config();
        config.as_object_mut().unwrap().insert("sAtIn".into(), json!(1));
        assert_eq!(parse(&config), Err(SatinChainConfigError::UnknownKey("sAtIn".into())));
        let mut config = genesis_config();
        config.as_object_mut().unwrap().insert("xsatinTime".into(), json!("anything"));
        config.as_object_mut().unwrap().insert("sati".into(), json!(1));
        assert_eq!(parse(&config), Ok(Some(super::tests::config())), "another fork's keys");
    }

    /// A value of the wrong type is refused, naming its key.
    #[test]
    fn test_a_value_of_the_wrong_type_is_refused() {
        let integer = "an integer from 0 to 2^64 - 1";
        let address = "a hex address";
        let cases: [(&str, Value, &str); 13] = [
            ("satinTime", json!("1800000000"), integer),
            ("satinTime", json!(-1), integer),
            ("satinTime", json!(1.5), integer),
            ("satinTxDataSizeLimit", json!(1e20), integer),
            ("satinTxDataSizeLimit", json!(null), integer),
            ("satinTxDataSizeLimit", json!(true), integer),
            ("satinTxDataSizeLimit", json!([1]), integer),
            ("satinTxDataSizeLimit", json!({ "value": 1 }), integer),
            ("satinInitialFromBlock", json!("0x4"), integer),
            ("satinInitialAdmin", json!(3), address),
            ("satinInitialAdmin", json!("0x33"), address),
            ("satinInitialAdmin", json!("0xzz33333333333333333333333333333333333333"), address),
            ("satinInitialAdmin", json!(null), address),
        ];
        for (key, value, expected) in cases {
            let mut config = genesis_config();
            config.as_object_mut().unwrap().insert(key.into(), value.clone());
            let key = *SATIN_CHAIN_CONFIG_KEYS.iter().find(|known| **known == key).unwrap();
            assert_eq!(
                parse(&config),
                Err(SatinChainConfigError::InvalidValue { key, expected }),
                "{key}: {value}"
            );
        }
    }

    /// Values the params types refuse are refused, with the message their check gives.
    #[test]
    fn test_a_value_the_params_refuse_is_refused() {
        let refused = |key: &str, value: Value| {
            let mut config = genesis_config();
            config.as_object_mut().unwrap().insert(key.into(), value);
            match parse(&config) {
                Err(SatinChainConfigError::Schedule(ScheduleError::InvalidParams {
                    params,
                    message,
                    ..
                })) => (params, message),
                other => panic!("{key}: {other:?}"),
            }
        };
        let zero = Address::ZERO.to_string();
        assert_eq!(
            refused("satinInitialSequencer", json!(zero)),
            (
                "SequencerRegistryConfig",
                "SequencerRegistryConfig.initial_sequencer must not be zero".into()
            )
        );
        assert_eq!(refused("satinMinRotationDelay", json!(0)).0, "SequencerRegistryConfig");
        assert_eq!(refused("satinBlockKvUpdateLimit", json!(0)).0, "ProtocolLimits");
        assert_eq!(refused("satinTxDataSizeLimit", json!(TX_BODY_SIZE - 1)).0, "ProtocolLimits");
        assert_eq!(
            refused("satinBlockEnvAccessComputeGasLimit", json!(MAX_TX_COMPUTE_GAS)).0,
            "ProtocolLimits"
        );

        let invalid = SatinChainConfig {
            protocol_limits: ProtocolLimits {
                block_state_gas_limit: 0,
                ..config().protocol_limits
            },
            ..config()
        };
        assert!(matches!(invalid.hardforks(), Err(ScheduleError::InvalidParams { .. })));
    }

    /// A key given twice is refused where the deserializer shows both, as in the object's text; a
    /// `serde_json::Value` has already kept the last one, and a key of another fork given twice is
    /// not read.
    #[test]
    fn test_a_key_given_twice_is_refused() {
        let text = genesis_config().to_string();
        let twice = text.replacen("\"satinTime\":", "\"satinTime\":1,\"satinTime\":", 1);
        assert_eq!(
            SatinChainConfig::from_genesis_config(&mut serde_json::Deserializer::from_str(&twice)),
            Err(SatinChainConfigError::DuplicateKey(SATIN_TIME_KEY.into()))
        );
        let other = text.replacen("\"chainId\":", "\"chainId\":1,\"chainId\":", 1);
        assert_eq!(
            SatinChainConfig::from_genesis_config(&mut serde_json::Deserializer::from_str(&other)),
            Ok(Some(config()))
        );
    }

    /// A number `serde_json` hands over as its `arbitrary_precision` feature does — a map from
    /// its token to the number's text — reads as the number, and what the text does not hold as
    /// an integer in 64 bits is refused as the number would be; any other map is refused.
    #[test]
    fn test_an_arbitrary_precision_number_reads_as_the_number() {
        let with_time = |value: Value| {
            let mut config = genesis_config();
            config.as_object_mut().unwrap().insert(SATIN_TIME_KEY.into(), value);
            parse(&config)
        };
        let number = |text: &str| json!({ ARBITRARY_PRECISION_NUMBER: text });
        assert_eq!(with_time(number("1800000000")), Ok(Some(config())));
        let max = with_time(number("18446744073709551615")).unwrap().unwrap();
        assert_eq!(max.activation_time, u64::MAX);
        let refused = Err(SatinChainConfigError::InvalidValue {
            key: SATIN_TIME_KEY,
            expected: "an integer from 0 to 2^64 - 1",
        });
        for text in ["1.5", "-1", "18446744073709551616", "1e3"] {
            assert_eq!(with_time(number(text)), refused, "{text}");
        }
        let mut two = number("1");
        two.as_object_mut().unwrap().insert("more".into(), json!(1));
        assert_eq!(with_time(two), refused);
        assert_eq!(with_time(json!({ "value": 1 })), refused);
    }

    /// Anything but an object is refused, saying an object was expected.
    #[test]
    fn test_a_configuration_that_is_not_an_object_is_refused() {
        for config in [json!([]), json!(1), json!("config"), json!(null)] {
            match parse(&config) {
                Err(SatinChainConfigError::Malformed(message)) => assert!(
                    message.contains("expected a chain configuration object"),
                    "{config}: {message}"
                ),
                other => panic!("{config}: {other:?}"),
            }
        }
    }

    /// A deserializer other than `serde_json`'s can hand a key a value no JSON document holds, a
    /// byte array: the configuration is refused as malformed, saying what was expected.
    #[test]
    fn test_a_value_no_json_holds_is_refused_as_malformed() {
        use serde::de::value::{Error, MapDeserializer};
        let entries = [(SATIN_TIME_KEY, &b"\x01"[..])];
        let config = MapDeserializer::<_, Error>::new(entries.into_iter());
        match SatinChainConfig::from_genesis_config(config) {
            Err(SatinChainConfigError::Malformed(message)) => {
                assert!(message.contains("expected any value"), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// The errors read as what went wrong, naming the key.
    #[test]
    fn test_the_errors_name_the_key() {
        assert_eq!(
            SatinChainConfigError::MissingKey("satinTxKvUpdateLimit").to_string(),
            "the chain configuration lacks `satinTxKvUpdateLimit`"
        );
        assert_eq!(
            SatinChainConfigError::UnknownKey("satinTme".into()).to_string(),
            "`satinTme` is not a Satin chain-configuration key"
        );
        assert_eq!(
            SatinChainConfigError::InvalidValue { key: "satinTime", expected: "an integer" }
                .to_string(),
            "`satinTime` must be an integer"
        );
        assert_eq!(
            SatinChainConfigError::DuplicateKey("satinTime".into()).to_string(),
            "`satinTime` is given twice"
        );
    }
}
