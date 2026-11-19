//! The schedule a block runs on, and what is checked of it before the block starts.

use alloy_evm::block::BlockExecutor;
use alloy_hardforks::{EthereumHardfork, ForkCondition, Hardfork};
use alloy_op_hardforks::OpHardfork;
use alloy_primitives::Address;
use mega_evm::{
    chain_activation, mainnet_hardforks, system::SequencerRegistryBootstrap, testnet_hardforks,
    ChainActivation, MegaHardfork, MegaHardforkConfig, MegaHardforks, SatinChainConfig,
    ScheduleError, MAINNET_CHAIN_ID, REX5_INITIAL_ADMIN_KEY, REX5_INITIAL_SEQUENCER_KEY,
    REX6_MIN_ROTATION_DELAY_KEY, TESTNET_CHAIN_ID,
};

use serde::Serialize;

use crate::common::{self, empty_call_gas, user_tx};

/// The canonical schedules are the chain activation table: the two are one source, so a
/// timestamp cannot be right in one and wrong in the other. What the published upgrade pages
/// say about them is the specification's side, not this crate's.
#[test]
fn test_canonical_schedules_match_the_chain_activation_table() {
    for (chain_id, schedule) in
        [(MAINNET_CHAIN_ID, mainnet_hardforks()), (TESTNET_CHAIN_ID, testnet_hardforks())]
    {
        let activation = chain_activation(chain_id).expect("a known chain");
        for fork in MegaHardfork::VARIANTS {
            assert_eq!(
                schedule.mega_fork_activation(*fork),
                activation.condition(*fork),
                "{fork:?} on chain {chain_id}"
            );
            // The schedule carries an entry for every fork, including the ones the chain has not
            // scheduled.
            assert_eq!(
                schedule.get(*fork),
                Some(&activation.condition(*fork)),
                "{fork:?} is registered on chain {chain_id}"
            );
        }
        assert_eq!(schedule.validate_schedule(), Ok(()), "chain {chain_id} loads");
    }

    // Satin is not scheduled on either chain yet, so neither runs this engine.
    assert_eq!(mainnet_hardforks().spec_id(u64::MAX), None);
    assert_eq!(testnet_hardforks().spec_id(u64::MAX), None);
}

/// A schedule that activates Satin and carries the `SequencerRegistry`'s bootstrap values, its one
/// chain-configuration parameter, loads, and a block runs on it, over a state holding the registry
/// the legacy forks left. The limits are the spec's, not a parameter.
#[test]
fn test_a_schedule_activating_satin_with_its_bootstrap_values_loads_and_runs() {
    let satin = MegaHardforkConfig::default()
        .with(MegaHardfork::Satin, ForkCondition::Timestamp(0))
        .with_sequencer_registry_bootstrap(common::bootstrap());
    assert_eq!(satin.validate_schedule(), Ok(()));

    let mut state = common::state();
    let mut executor = common::executor_with_spec(&mut state, common::unlimited_ctx(), satin);
    executor.apply_pre_execution_changes().expect("the block starts");
    executor.execute_transaction(&user_tx(0, empty_call_gas())).expect("the transaction executes");
}

/// A schedule built in code that activates Satin without the `SequencerRegistry`'s bootstrap
/// values is refused when it is loaded, naming the values, not at the first block that must seed
/// or migrate the registry; so is one carrying any of them zero, naming the value's key, as the
/// genesis parser names it. A schedule that does not activate Satin needs none, and the live
/// networks' schedules, which carry Rex5's and Rex6's values, load whenever Satin is scheduled.
#[test]
fn test_a_schedule_activating_satin_without_its_bootstrap_values_is_refused_at_load() {
    for satin_time in [0, SATIN_TIME] {
        let satin = MegaHardforkConfig::new()
            .with(MegaHardfork::Satin, ForkCondition::Timestamp(satin_time));
        assert_eq!(
            satin.validate_schedule(),
            Err(ScheduleError::MissingRegistryBootstrap { fork: MegaHardfork::Satin }),
            "Satin at {satin_time}"
        );
        let message = satin.validate_schedule().unwrap_err().to_string();
        for key in [REX5_INITIAL_SEQUENCER_KEY, REX5_INITIAL_ADMIN_KEY, REX6_MIN_ROTATION_DELAY_KEY]
        {
            assert!(message.contains(key), "the refusal names {key}: {message}");
        }

        let values = common::bootstrap();
        for (key, zero) in [
            (
                REX5_INITIAL_SEQUENCER_KEY,
                SequencerRegistryBootstrap { initial_sequencer: Address::ZERO, ..values },
            ),
            (
                REX5_INITIAL_ADMIN_KEY,
                SequencerRegistryBootstrap { initial_admin: Address::ZERO, ..values },
            ),
            (
                REX6_MIN_ROTATION_DELAY_KEY,
                SequencerRegistryBootstrap { min_rotation_delay: 0, ..values },
            ),
        ] {
            let schedule = satin.clone().with_sequencer_registry_bootstrap(zero);
            assert_eq!(
                schedule.validate_schedule(),
                Err(ScheduleError::ZeroRegistryBootstrapValue { fork: MegaHardfork::Satin, key }),
                "{key}"
            );
            assert!(schedule.validate_schedule().unwrap_err().to_string().contains(key), "{key}");
        }
        assert_eq!(satin.with_sequencer_registry_bootstrap(values).validate_schedule(), Ok(()));
    }

    let without_satin = MegaHardforkConfig::new();
    assert_eq!(without_satin.sequencer_registry_bootstrap(), None);
    assert_eq!(without_satin.validate_schedule(), Ok(()), "no Satin, no values needed");
    for chain_id in [MAINNET_CHAIN_ID, TESTNET_CHAIN_ID] {
        let scheduled = ChainActivation {
            satin: Some(SATIN_TIME),
            ..chain_activation(chain_id).expect("a known chain")
        };
        assert_eq!(scheduled.hardforks().validate_schedule(), Ok(()), "chain {chain_id}");
    }
}

/// When Satin activates on the schedules of [`base_fork_case`].
const SATIN_TIME: u64 = 1_800_000_000;

/// The forks Satin runs on that a schedule activates by timestamp: the Ethereum forks from
/// Shanghai to Osaka and the Optimism forks from Regolith to Karst.
const ETHEREUM_BASE_FORKS: [EthereumHardfork; 4] = [
    EthereumHardfork::Shanghai,
    EthereumHardfork::Cancun,
    EthereumHardfork::Prague,
    EthereumHardfork::Osaka,
];
const OP_BASE_FORKS: [OpHardfork; 9] = [
    OpHardfork::Regolith,
    OpHardfork::Canyon,
    OpHardfork::Ecotone,
    OpHardfork::Fjord,
    OpHardfork::Granite,
    OpHardfork::Holocene,
    OpHardfork::Isthmus,
    OpHardfork::Jovian,
    OpHardfork::Karst,
];

/// A schedule that activates Satin at [`SATIN_TIME`], carrying its bootstrap values, every base
/// fork at genesis but `base`, which activates on `condition`.
fn base_fork_case(base: impl Hardfork, condition: ForkCondition) -> MegaHardforkConfig {
    MegaHardforkConfig::new()
        .with(MegaHardfork::Satin, ForkCondition::Timestamp(SATIN_TIME))
        .with_sequencer_registry_bootstrap(common::bootstrap())
        .with(base, condition)
}

/// The refusal of a schedule whose `base` fork is not active when Satin activates.
fn inactive(base: impl Hardfork) -> Result<(), ScheduleError> {
    Err(ScheduleError::InactiveBaseFork {
        fork: MegaHardfork::Satin,
        activation: SATIN_TIME,
        base: base.name(),
    })
}

/// A schedule that activates Karst after Satin fails when it is loaded: every Satin block runs on
/// Karst's rules, which the rest of the node would not apply to the blocks in between. So does
/// one that delays any other fork Satin runs on, by one second.
#[test]
fn test_a_base_fork_after_satin_is_refused_at_load() {
    let late = ForkCondition::Timestamp(SATIN_TIME + 1);
    let schedule = base_fork_case(OpHardfork::Karst, late);
    assert_eq!(schedule.validate_schedule(), inactive(OpHardfork::Karst));
    assert_eq!(
        schedule.validate_schedule().unwrap_err().to_string(),
        "hardfork Satin activates at timestamp 1800000000, where the Karst fork it runs on is \
         not active",
    );

    for fork in ETHEREUM_BASE_FORKS {
        assert_eq!(base_fork_case(fork, late).validate_schedule(), inactive(fork), "{fork:?}");
    }
    for fork in OP_BASE_FORKS {
        assert_eq!(base_fork_case(fork, late).validate_schedule(), inactive(fork), "{fork:?}");
    }
}

/// A base fork the schedule never activates, or activates by block number, is not active at any
/// timestamp, which is how block execution reads it, and is refused as a late one is.
#[test]
fn test_a_base_fork_not_scheduled_by_timestamp_is_refused_at_load() {
    for condition in [ForkCondition::Never, ForkCondition::Block(0)] {
        for fork in ETHEREUM_BASE_FORKS {
            let schedule = base_fork_case(fork, condition);
            assert_eq!(schedule.validate_schedule(), inactive(fork), "{fork:?} at {condition:?}");
        }
        for fork in OP_BASE_FORKS {
            let schedule = base_fork_case(fork, condition);
            assert_eq!(schedule.validate_schedule(), inactive(fork), "{fork:?} at {condition:?}");
        }
    }
}

/// A base fork that activates with Satin is active at its activation, and the schedule loads:
/// that is the timing a node's own configuration gives Jovian, Karst and Osaka. So does one
/// that activates every base fork with Satin.
#[test]
fn test_a_base_fork_at_satin_time_is_accepted() {
    let at_satin = ForkCondition::Timestamp(SATIN_TIME);
    assert_eq!(base_fork_case(OpHardfork::Karst, at_satin).validate_schedule(), Ok(()));
    for fork in ETHEREUM_BASE_FORKS {
        assert_eq!(base_fork_case(fork, at_satin).validate_schedule(), Ok(()), "{fork:?}");
    }
    for fork in OP_BASE_FORKS {
        assert_eq!(base_fork_case(fork, at_satin).validate_schedule(), Ok(()), "{fork:?}");
    }

    let mut all_at_satin = base_fork_case(OpHardfork::Karst, at_satin);
    for fork in ETHEREUM_BASE_FORKS {
        all_at_satin.insert(fork, at_satin);
    }
    for fork in OP_BASE_FORKS {
        all_at_satin.insert(fork, at_satin);
    }
    assert_eq!(all_at_satin.validate_schedule(), Ok(()));
}

/// The schedules the engine builds itself put every base fork at genesis, so they load whenever
/// Satin activates.
#[test]
fn test_engine_built_schedules_keep_their_base_forks_at_genesis() {
    for satin_time in [0, 1, SATIN_TIME] {
        let schedule = MegaHardforkConfig::new()
            .with(MegaHardfork::Satin, ForkCondition::Timestamp(satin_time))
            .with_sequencer_registry_bootstrap(common::bootstrap());
        assert_eq!(schedule.validate_schedule(), Ok(()), "Satin at {satin_time}");

        let activation = ChainActivation {
            satin: Some(satin_time),
            ..mega_evm::chain_activation(MAINNET_CHAIN_ID).expect("a known chain")
        };
        let schedule = activation.hardforks();
        assert_eq!(schedule.validate_schedule(), Ok(()), "the table's Satin at {satin_time}");
    }
}

/// Only Satin's activation is checked against: a schedule that does not activate Satin loads
/// whatever it times its base forks at.
#[test]
fn test_base_forks_are_not_checked_without_satin() {
    let schedule = MegaHardforkConfig::new()
        .with(OpHardfork::Karst, ForkCondition::Never)
        .with(EthereumHardfork::Osaka, ForkCondition::Timestamp(SATIN_TIME));
    assert_eq!(schedule.mega_fork_activation(MegaHardfork::Satin), ForkCondition::Never);
    assert_eq!(schedule.validate_schedule(), Ok(()));
}

/// The forks up to the Merge, and Bedrock, are scheduled by block number or total difficulty,
/// which a timestamp says nothing about: they are not checked against Satin's activation.
#[test]
fn test_forks_scheduled_by_block_are_not_checked() {
    for fork in [EthereumHardfork::London, EthereumHardfork::Paris] {
        let schedule = base_fork_case(fork, ForkCondition::Never);
        assert_eq!(schedule.validate_schedule(), Ok(()), "{fork:?}");
    }
    let schedule = base_fork_case(OpHardfork::Bedrock, ForkCondition::Block(u64::MAX));
    assert_eq!(schedule.validate_schedule(), Ok(()));
}

/// What a serializer is told of a map and given for it: the length the map announces before
/// its entries, and the entries that follow. JSON ignores the announced length; a format that
/// writes it ahead of the entries, as a length-prefixed binary format does, relies on it.
#[derive(Debug, Default)]
struct MapShape {
    announced: Option<usize>,
    entries: usize,
}

type ShapeError = serde::de::value::Error;
type NoCompound = serde::ser::Impossible<(), ShapeError>;

/// Refuses every value that is not a map: the configuration serializes as one map.
macro_rules! only_a_map {
    ($($method:ident($($arg:ty),*);)*) => {
        $(fn $method(self, $(_: $arg),*) -> Result<(), ShapeError> {
            Err(serde::ser::Error::custom("the configuration serializes as a map"))
        })*
    };
}

impl serde::Serializer for &mut MapShape {
    type Ok = ();
    type Error = ShapeError;
    type SerializeSeq = NoCompound;
    type SerializeTuple = NoCompound;
    type SerializeTupleStruct = NoCompound;
    type SerializeTupleVariant = NoCompound;
    type SerializeMap = Self;
    type SerializeStruct = NoCompound;
    type SerializeStructVariant = NoCompound;

    only_a_map! {
        serialize_bool(bool); serialize_i8(i8); serialize_i16(i16); serialize_i32(i32);
        serialize_i64(i64); serialize_u8(u8); serialize_u16(u16); serialize_u32(u32);
        serialize_u64(u64); serialize_f32(f32); serialize_f64(f64); serialize_char(char);
        serialize_str(&str); serialize_bytes(&[u8]); serialize_none(); serialize_unit();
        serialize_unit_struct(&'static str);
        serialize_unit_variant(&'static str, u32, &'static str);
    }

    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<(), ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<(), ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<(), ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<NoCompound, ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }

    fn serialize_tuple(self, _: usize) -> Result<NoCompound, ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }

    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<NoCompound, ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<NoCompound, ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Self, ShapeError> {
        self.announced = len;
        Ok(self)
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<NoCompound, ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<NoCompound, ShapeError> {
        Err(serde::ser::Error::custom("the configuration serializes as a map"))
    }
}

impl serde::ser::SerializeMap for &mut MapShape {
    type Ok = ();
    type Error = ShapeError;

    fn serialize_key<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), ShapeError> {
        self.entries += 1;
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(&mut self, _: &T) -> Result<(), ShapeError> {
        Ok(())
    }

    fn end(self) -> Result<(), ShapeError> {
        Ok(())
    }
}

/// The configuration announces the number of entries it writes, four — `satinTime` and the
/// three bootstrap keys — so a format that writes the length ahead of the entries encodes it
/// as it is.
#[test]
fn test_the_configuration_announces_the_entries_it_writes() {
    let mut shape = MapShape::default();
    let config = SatinChainConfig {
        activation_time: SATIN_TIME,
        sequencer_registry_bootstrap: common::bootstrap(),
    };
    config.serialize(&mut shape).unwrap();
    assert_eq!(shape.entries, 4, "satinTime and the three bootstrap keys");
    assert_eq!(shape.announced, Some(shape.entries), "the announced length");
}
