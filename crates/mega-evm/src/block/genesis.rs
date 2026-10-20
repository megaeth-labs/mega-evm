//! The Satin key of a chain configuration: when Satin activates.
//!
//! [`SatinChainConfig`] reads it and states the format, which is provisional.

#[cfg(not(feature = "std"))]
use alloc as std;
use core::fmt;
use std::{
    collections::BTreeMap,
    string::{String, ToString},
};

use alloy_hardforks::ForkCondition;
use serde::{
    de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor},
    ser::SerializeMap,
    Deserialize, Deserializer, Serialize, Serializer,
};

use crate::{MegaHardfork, MegaHardforkConfig, MegaHardforks, ScheduleError};

/// The key of Satin's activation timestamp.
pub const SATIN_TIME_KEY: &str = "satinTime";

/// Every Satin key of a chain configuration, in the order [`SatinChainConfig`] writes them.
pub const SATIN_CHAIN_CONFIG_KEYS: [&str; 1] = [SATIN_TIME_KEY];

/// The prefix every Satin key starts with, compared without regard to case.
const PREFIX: &str = "satin";

/// The key `serde_json` built with `arbitrary_precision` hands a number over under.
const ARBITRARY_PRECISION_NUMBER: &str = "$serde_json::private::Number";

/// A chain's Satin configuration, as its genesis file's `config` object carries it: when Satin
/// activates.
///
/// **The key format is provisional.** It is fixed when a network first publishes a genesis file
/// that carries it; until then a change to it is a change to this parser and to every genesis
/// file written against it.
///
/// Every reader of a chain's Satin configuration — a node, a stateless validator, a genesis
/// generator, a replay tool — parses it here ([`SatinChainConfig::from_genesis_config`]), so the
/// key names and the rules cannot drift apart between them: a reader that took another value for
/// the key would execute another chain.
///
/// Satin carries no parameter: a chain that activates it has run the legacy forks, which deployed
/// the `SequencerRegistry` and upgraded it to v2.0.0, and the registry's roles are what it holds.
/// The legacy forks' own keys (`rex5InitialSequencer`, `rex6MinRotationDelay`, ...) are theirs
/// and are not read here.
///
/// The key sits at the top level of the genesis file's `config` object, beside the other forks'
/// keys, camelCase:
///
/// | Key         | Value   | Field                                                  |
/// | ----------- | ------- | ------------------------------------------------------ |
/// | `satinTime` | integer | the activation timestamp ([`Self::activation_time`])   |
///
/// An integer is a JSON number that fits in 64 unsigned bits, `u64::MAX` included, which a writer
/// must emit exactly.
///
/// The rules:
///
/// - A configuration with no key starting with `satin` does not activate Satin.
/// - A key starting with `satin`, in any case, that is not `satinTime` is refused, so a misspelled
///   key cannot stand in for the one it meant, and a file written against an earlier version of the
///   format — one that named the registry's roles (`satinInitialSequencer`, ...) or the limits —
///   does not load. Every other key is another fork's and is not read.
/// - A key given twice is refused only where the deserializer still shows both copies, as the
///   object's text through a `serde_json::Deserializer` does. A `serde_json::Value` has already
///   kept the last copy, so a reader that parses the file into one first — `mega-evme --genesis`
///   does, and so does a node that serializes its chain-config type — reads the last value and
///   refuses nothing.
/// - A value of the wrong type is refused, naming its key, and so is a schedule
///   [`validate_schedule`](MegaHardforks::validate_schedule) refuses. One object is read as an
///   integer: `{"$serde_json::private::Number": "<digits>"}`, the form `serde_json` hands a number
///   over in when its `arbitrary_precision` feature is on. A deserializer shows that form and the
///   same object written into the file alike, so both are read as the number; every reader parses
///   here, so every reader reads it the same.
///
/// The parser reads no other fork's key. Satin runs on the OP Karst fork and the Ethereum forks
/// Karst includes, Osaka among them, and [`hardforks`](Self::hardforks) schedules those at genesis.
/// A node's own schedule times those forks from the file, and holds them to both halves of the
/// timing rule [`validate_schedule`](MegaHardforks::validate_schedule) states.
///
/// A flat key is one entry of the object, so a checker that freezes a configuration key by key
/// freezes `satinTime` on its own and names it when it changes. It does not stop a key being added
/// later; a rule that refuses a new key starting with `satin` once the configuration was first
/// loaded does.
///
/// It serializes to the key it is read from, so a generator that writes a genesis file with it
/// writes what every reader parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SatinChainConfig {
    /// The timestamp Satin activates at.
    pub activation_time: u64,
}

impl SatinChainConfig {
    /// Reads the Satin key of a genesis file's `config` object from `config`, which any
    /// self-describing deserializer of the object can supply: a `&serde_json::Value`, or the
    /// object's text through a `serde_json::Deserializer`, whose caller checks that the input ends
    /// with the object. A node's own chain-config type supplies one by serializing to a
    /// `serde_json::Value`.
    ///
    /// `Ok(None)` when the configuration does not activate Satin; otherwise the configuration,
    /// whose schedule [`validate_schedule`](MegaHardforks::validate_schedule) accepts.
    ///
    /// # Errors
    ///
    /// Every rule of the [format](Self) that the configuration breaks, the first one found.
    pub fn from_genesis_config<'de, D: Deserializer<'de>>(
        config: D,
    ) -> Result<Option<Self>, SatinChainConfigError> {
        let Entries { mut entries, duplicate } = config
            .deserialize_map(EntriesVisitor)
            .map_err(|error| SatinChainConfigError::Malformed(error.to_string()))?;
        if let Some(key) = duplicate {
            return Err(SatinChainConfigError::DuplicateKey(key));
        }
        if let Some(unknown) =
            entries.keys().find(|key| !SATIN_CHAIN_CONFIG_KEYS.contains(&key.as_str()))
        {
            return Err(SatinChainConfigError::UnknownKey(unknown.clone()));
        }
        let activation_time = match entries.remove(SATIN_TIME_KEY) {
            None => return Ok(None),
            Some(Entry::Unsigned(value)) => value,
            Some(Entry::Other) => {
                return Err(SatinChainConfigError::InvalidValue {
                    key: SATIN_TIME_KEY,
                    expected: "an integer from 0 to 2^64 - 1",
                })
            }
        };
        let config = Self { activation_time };
        config.hardforks()?;
        Ok(Some(config))
    }

    /// The hardfork schedule this configuration makes: [`MegaHardforkConfig::new`]'s base forks
    /// and Satin at [`activation_time`](Self::activation_time).
    ///
    /// The base forks, every OP and Ethereum fork up to Karst, are at genesis here whatever the
    /// genesis file times them at, so the schedule passes the base-fork check of
    /// [`validate_schedule`](MegaHardforks::validate_schedule) whenever Satin activates: a node
    /// checks the file's own times against `satinTime` on its own schedule (see the
    /// [format](Self)).
    ///
    /// # Errors
    ///
    /// A schedule [`validate_schedule`](MegaHardforks::validate_schedule) refuses.
    pub fn hardforks(&self) -> Result<MegaHardforkConfig, ScheduleError> {
        let schedule = MegaHardforkConfig::new()
            .with(MegaHardfork::Satin, ForkCondition::Timestamp(self.activation_time));
        schedule.validate_schedule()?;
        Ok(schedule)
    }
}

impl Serialize for SatinChainConfig {
    /// Writes the key, [`SATIN_TIME_KEY`].
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(SATIN_CHAIN_CONFIG_KEYS.len()))?;
        map.serialize_entry(SATIN_TIME_KEY, &self.activation_time)?;
        map.end()
    }
}

/// Why a chain configuration's Satin key cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SatinChainConfigError {
    /// The configuration is not an object with string keys.
    Malformed(String),
    /// A key that starts with `satin` but is not `satinTime`.
    UnknownKey(String),
    /// A key that starts with `satin`, given twice.
    DuplicateKey(String),
    /// A key whose value is not of its type.
    InvalidValue {
        /// The key.
        key: &'static str,
        /// What its value must be.
        expected: &'static str,
    },
    /// A schedule [`validate_schedule`](MegaHardforks::validate_schedule) refuses.
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

/// The value of a key, as far as reading the Satin key needs it: an unsigned integer, or anything
/// else.
#[derive(Debug)]
enum Entry {
    Unsigned(u64),
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

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Entry, E> {
        Ok(Entry::Other)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MegaSpecId;
    use serde_json::{json, Value};

    const ACTIVATION: u64 = 1_800_000_000;

    fn config() -> SatinChainConfig {
        SatinChainConfig { activation_time: ACTIVATION }
    }

    /// The keys of the configuration format's earlier versions: the `SequencerRegistry`'s roles and
    /// rotation delay, `_initialFromBlock`, and the limits.
    const OLD_KEYS: [&str; 10] = [
        "satinInitialSystemAddress",
        "satinInitialSequencer",
        "satinInitialAdmin",
        "satinMinRotationDelay",
        "satinInitialFromBlock",
        "satinTxDataSizeLimit",
        "satinFrameDataSizeLimit",
        "satinBlockEnvAccessComputeGasLimit",
        "satinOracleAccessComputeGasLimit",
        "satinBlockTxsDataLimit",
    ];

    /// A mainnet-shaped genesis `config` object carrying [`config`] among other forks' keys, the
    /// legacy forks' registry keys included.
    fn genesis_config() -> Value {
        let mut object = json!({
            "chainId": 4326,
            "homesteadBlock": 0,
            "shanghaiTime": 0,
            "rex5Time": 1_760_000_000,
            "rex6Time": 1_770_000_000,
            "rex5InitialSequencer": "0x4444444444444444444444444444444444444444",
            "rex5InitialAdmin": "0x5555555555555555555555555555555555555555",
            "rex6MinRotationDelay": 100,
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

    /// The configuration writes `satinTime` alone, and reading it back gives the same
    /// configuration, from a `serde_json::Value` and from the object's text alike, with the legacy
    /// forks' keys beside it read by nobody here.
    #[test]
    fn test_the_key_round_trips() {
        assert_eq!(serde_json::to_string(&config()).unwrap(), r#"{"satinTime":1800000000}"#);
        assert_eq!(SATIN_CHAIN_CONFIG_KEYS, [SATIN_TIME_KEY]);

        assert_eq!(parse(&genesis_config()), Ok(Some(config())));
        let text = genesis_config().to_string();
        assert_eq!(
            SatinChainConfig::from_genesis_config(&mut serde_json::Deserializer::from_str(&text)),
            Ok(Some(config()))
        );
    }

    /// The schedule the configuration makes runs Satin from its activation and passes the
    /// schedule's own check.
    #[test]
    fn test_the_schedule_activates_satin() {
        let schedule = config().hardforks().unwrap();
        assert_eq!(schedule.spec_id(ACTIVATION - 1), None);
        assert_eq!(schedule.spec_id(ACTIVATION), Some(MegaSpecId::SATIN));
        assert_eq!(schedule.validate_schedule(), Ok(()));
    }

    /// A configuration without a Satin key does not activate Satin. One whose only Satin key is
    /// not `satinTime` is refused for that key.
    #[test]
    fn test_without_satin_time() {
        let mut other_forks = genesis_config();
        other_forks.as_object_mut().unwrap().remove(SATIN_TIME_KEY);
        assert_eq!(parse(&other_forks), Ok(None));
        assert_eq!(parse(&json!({})), Ok(None));

        let mut without_time = other_forks;
        without_time.as_object_mut().unwrap().insert("satinInitialSequencer".into(), json!("0x22"));
        assert_eq!(
            parse(&without_time),
            Err(SatinChainConfigError::UnknownKey("satinInitialSequencer".into()))
        );
    }

    /// Every key of the format's earlier versions is refused by name, beside `satinTime` or
    /// without it, whatever its value: a file written against one does not load.
    #[test]
    fn test_the_old_keys_are_refused() {
        for key in OLD_KEYS {
            for with_time in [true, false] {
                let mut config = genesis_config();
                if !with_time {
                    config.as_object_mut().unwrap().remove(SATIN_TIME_KEY);
                }
                config.as_object_mut().unwrap().insert(key.into(), json!(1));
                assert_eq!(
                    parse(&config),
                    Err(SatinChainConfigError::UnknownKey(key.into())),
                    "{key}, satinTime given: {with_time}"
                );
            }
        }
    }

    /// A key that starts with `satin`, in any case, and is not `satinTime` is refused, so a
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

    /// A `satinTime` that is not an integer from 0 to 2^64 - 1 is refused, naming the key; both
    /// ends of the range are read.
    #[test]
    fn test_a_value_out_of_range_or_of_the_wrong_type_is_refused() {
        let with_time = |value: Value| {
            let mut config = genesis_config();
            config.as_object_mut().unwrap().insert(SATIN_TIME_KEY.into(), value);
            parse(&config)
        };
        assert_eq!(with_time(json!(0)).unwrap().unwrap().activation_time, 0);
        assert_eq!(with_time(json!(u64::MAX)).unwrap().unwrap().activation_time, u64::MAX);
        let refused = Err(SatinChainConfigError::InvalidValue {
            key: SATIN_TIME_KEY,
            expected: "an integer from 0 to 2^64 - 1",
        });
        let text = genesis_config().to_string();
        let past_max = text.replacen("1800000000", "18446744073709551616", 1);
        assert_eq!(
            SatinChainConfig::from_genesis_config(&mut serde_json::Deserializer::from_str(
                &past_max
            )),
            refused,
            "2^64"
        );
        for value in [
            json!("1800000000"),
            json!(-1),
            json!(1.5),
            json!(1e20),
            json!(null),
            json!(true),
            json!([1]),
            json!({ "value": 1 }),
            json!("0x4"),
        ] {
            assert_eq!(with_time(value.clone()), refused, "{value}");
        }
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
