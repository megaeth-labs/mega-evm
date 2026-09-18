//! Unit tests extracted from `crates/mega-evm/src/block/hardfork.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/block/hardfork.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SequencerRegistryConfig;

    #[test]
    fn test_mega_hardfork_spec_ids_match_expected_specs() {
        // Note: MiniRex1 and MiniRex2 map to alias rungs whose behavior reverts to earlier specs.
        let cases = [
            (MegaHardfork::MiniRex, MegaSpecId::MINI_REX),
            (MegaHardfork::MiniRex1, MegaSpecId::MINI_REX_1),
            (MegaHardfork::MiniRex2, MegaSpecId::MINI_REX_2),
            (MegaHardfork::Rex, MegaSpecId::REX),
            (MegaHardfork::Rex1, MegaSpecId::REX1),
            (MegaHardfork::Rex2, MegaSpecId::REX2),
            (MegaHardfork::Rex3, MegaSpecId::REX3),
            (MegaHardfork::Rex4, MegaSpecId::REX4),
            (MegaHardfork::Rex5, MegaSpecId::REX5),
            (MegaHardfork::Rex6, MegaSpecId::REX6),
            (MegaHardfork::Rex7, MegaSpecId::REX7),
        ];

        for (hardfork, expected_spec) in cases {
            assert_eq!(hardfork.spec_id(), expected_spec);
        }
    }

    #[test]
    fn test_default_config_contains_upstream_forks_and_no_mega_forks() {
        let config = MegaHardforkConfig::default();

        assert_eq!(
            config.ethereum_fork_activation(EthereumHardfork::Frontier),
            ForkCondition::Block(0)
        );
        assert_eq!(
            config.ethereum_fork_activation(EthereumHardfork::Prague),
            ForkCondition::Timestamp(0)
        );
        assert_eq!(config.op_fork_activation(OpHardfork::Isthmus), ForkCondition::Timestamp(0));
        assert_eq!(config.mega_fork_activation(MegaHardfork::MiniRex), ForkCondition::Never);
        assert!(config.fork_params::<SequencerRegistryConfig>().is_none());
    }

    #[test]
    fn test_config_builder_helpers_override_and_remove_hardforks() {
        let mut config = MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(10))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(80));

        assert_eq!(config.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(10)));
        assert_eq!(config.get(MegaHardfork::Rex4), Some(&ForkCondition::Timestamp(80)));

        config.insert(MegaHardfork::MiniRex, ForkCondition::Timestamp(20));
        assert_eq!(config.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(20)));

        let config = config.without(MegaHardfork::MiniRex);
        assert_eq!(config.get(MegaHardfork::MiniRex), None);

        let from_iter = MegaHardforkConfig::from(
            [
                (MegaHardfork::MiniRex, ForkCondition::Timestamp(1)),
                (MegaHardfork::Rex2, ForkCondition::Timestamp(2)),
            ]
            .into_iter(),
        );
        assert_eq!(from_iter.get(MegaHardfork::MiniRex), Some(&ForkCondition::Timestamp(1)));
        assert_eq!(from_iter.get(MegaHardfork::Rex2), Some(&ForkCondition::Timestamp(2)));
    }

    #[test]
    fn test_with_all_activated_enables_all_mega_hardforks() {
        let config = MegaHardforkConfig::default().with_all_activated();

        // Driven off `VARIANTS`, which the `hardfork!` macro generates from the same variant list
        // that declares the enum. A second hand-written list here would assert only that the
        // forks someone remembered to name are activated — a new fork missing from both the
        // builder and the list would fail neither.
        for hardfork in MegaHardfork::VARIANTS {
            assert_eq!(
                config.mega_fork_activation(*hardfork),
                ForkCondition::Timestamp(0),
                "{hardfork:?}"
            );
        }
    }

    #[test]
    fn test_fork_params_typed_access() {
        let params = SequencerRegistryConfig {
            rex5_initial_sequencer: alloy_primitives::address!(
                "0x2222222222222222222222222222222222222222"
            ),
            rex5_initial_admin: alloy_primitives::address!(
                "0x3333333333333333333333333333333333333333"
            ),
        };

        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(0))
            .with_params(params.clone());

        let retrieved = config
            .fork_params::<SequencerRegistryConfig>()
            .expect("should have SequencerRegistryConfig");
        assert_eq!(retrieved, &params);
    }

    #[test]
    fn test_fork_params_none_when_not_configured() {
        let config =
            MegaHardforkConfig::default().with(MegaHardfork::Rex5, ForkCondition::Timestamp(0));

        assert!(config.fork_params::<SequencerRegistryConfig>().is_none());
    }

    #[test]
    fn test_default_validate_accepts_any_value() {
        #[derive(Debug)]
        struct NullParams;

        impl HardforkParams for NullParams {
            const FORK: MegaHardfork = MegaHardfork::Rex4;
        }

        assert!(NullParams.validate().is_ok());
    }

    #[test]
    fn test_hardfork_params_error_display() {
        let e = HardforkParamsError { message: "something went wrong".into() };
        assert_eq!(e.to_string(), "something went wrong");
    }

    #[test]
    #[should_panic(expected = "Invalid params for fork")]
    fn test_with_params_panics_on_validation_error() {
        #[derive(Debug)]
        struct AlwaysErrParams;

        impl HardforkParams for AlwaysErrParams {
            const FORK: MegaHardfork = MegaHardfork::Rex4;

            fn validate(&self) -> Result<(), HardforkParamsError> {
                Err(HardforkParamsError { message: "intentional test error".into() })
            }
        }

        MegaHardforkConfig::default().with_all_activated().with_params(AlwaysErrParams);
    }

    /// Documented domain limit: resolution is timestamp-scoped, so a `MegaHardfork` registered
    /// by block number never contributes its own spec to it. `spec_id`/`hardfork` share this
    /// limitation; every canonical schedule uses `Timestamp` or `Never`, and
    /// `validate_schedule` rejects anything else.
    #[test]
    fn test_resolution_ignores_block_numbered_forks() {
        let hf = MegaHardforkConfig::new()
            .with(MegaHardfork::MiniRex, ForkCondition::Block(0))
            .with(MegaHardfork::Rex, ForkCondition::Timestamp(0));

        assert!(
            !hf.mega_fork_activation(MegaHardfork::MiniRex).active_at_timestamp(0),
            "block-numbered forks are not timestamped"
        );
        // The resolved spec comes from Rex alone; it still covers MINI_REX by ordinal
        // inclusion, so the projected predicate reports active even though the MiniRex event
        // itself never fires.
        assert_eq!(hf.spec_id(0), MegaSpecId::REX);
        assert!(hf.spec_id(0).is_enabled(MegaSpecId::MINI_REX));
        assert!(hf.is_mini_rex_active_at_timestamp(0));
        assert_eq!(
            hf.validate_schedule(),
            Err(ScheduleError::NonTimestampActivation { fork: MegaHardfork::MiniRex })
        );
    }

    /// A scheduled fork whose required params are missing fails at validation time instead of at
    /// the first block of the fork.
    #[test]
    fn test_validate_schedule_requires_scheduled_fork_params() {
        let rex5_no_params =
            MegaHardforkConfig::default().with_all_activated_through(MegaSpecId::REX5);
        assert_eq!(
            rex5_no_params.validate_schedule(),
            Err(ScheduleError::MissingParams {
                fork: MegaHardfork::Rex5,
                params: "SequencerRegistryConfig"
            })
        );

        let rex6_no_params = MegaHardforkConfig::default()
            .with_all_activated_through(MegaSpecId::REX6)
            .with_params(SequencerRegistryConfig {
                rex5_initial_sequencer: crate::MEGA_SYSTEM_ADDRESS,
                rex5_initial_admin: crate::MEGA_SYSTEM_ADDRESS,
            });
        assert_eq!(
            rex6_no_params.validate_schedule(),
            Err(ScheduleError::MissingParams {
                fork: MegaHardfork::Rex6,
                params: "SequencerRegistryRex6Config"
            })
        );
    }

    #[test]
    fn test_hardfork_and_spec_id_follow_latest_active_timestamp() {
        let config = MegaHardforkConfig::default()
            .with(MegaHardfork::MiniRex, ForkCondition::Timestamp(100))
            .with(MegaHardfork::Rex4, ForkCondition::Timestamp(200))
            .with(MegaHardfork::Rex5, ForkCondition::Timestamp(300))
            .with(MegaHardfork::Rex6, ForkCondition::Timestamp(400))
            .with(MegaHardfork::Rex7, ForkCondition::Timestamp(500));

        assert_eq!(config.hardfork(99), None);
        assert_eq!(config.hardfork(100), Some(MegaHardfork::MiniRex));
        assert_eq!(config.hardfork(200), Some(MegaHardfork::Rex4));
        assert_eq!(config.hardfork(300), Some(MegaHardfork::Rex5));
        assert_eq!(config.hardfork(400), Some(MegaHardfork::Rex6));
        assert_eq!(config.hardfork(500), Some(MegaHardfork::Rex7));
        assert_eq!(config.spec_id(99), MegaSpecId::EQUIVALENCE);
        assert_eq!(config.spec_id(100), MegaSpecId::MINI_REX);
        assert_eq!(config.spec_id(200), MegaSpecId::REX4);
        assert_eq!(config.spec_id(300), MegaSpecId::REX5);
        assert_eq!(config.spec_id(400), MegaSpecId::REX6);
        assert_eq!(config.spec_id(500), MegaSpecId::REX7);
    }
}
