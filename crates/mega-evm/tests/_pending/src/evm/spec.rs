//! Unit tests extracted from `crates/mega-evm/src/evm/spec.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/spec.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;

    /// The one golden spec table: every spec with its string identifier and its pinned ladder
    /// position. The spec column must be exactly [`MegaSpecId::ALL`] (asserted in the
    /// round-trip test); the name and position columns stay hand-written — deriving either
    /// from the code under test would make its check vacuous.
    const ALL_SPECS: [(MegaSpecId, &str, u8); 12] = [
        (MegaSpecId::EQUIVALENCE, name::EQUIVALENCE, 0),
        (MegaSpecId::MINI_REX, name::MINI_REX, 1),
        (MegaSpecId::MINI_REX_1, name::MINI_REX_1, 2),
        (MegaSpecId::MINI_REX_2, name::MINI_REX_2, 3),
        (MegaSpecId::REX, name::REX, 4),
        (MegaSpecId::REX1, name::REX1, 5),
        (MegaSpecId::REX2, name::REX2, 6),
        (MegaSpecId::REX3, name::REX3, 7),
        (MegaSpecId::REX4, name::REX4, 8),
        (MegaSpecId::REX5, name::REX5, 9),
        (MegaSpecId::REX6, name::REX6, 10),
        (MegaSpecId::REX7, name::REX7, 11),
    ];

    #[test]
    fn test_spec_names_roundtrip_and_display() {
        // The spec column must be exactly `MegaSpecId::ALL`, so a newly introduced spec cannot
        // be forgotten here.
        assert!(ALL_SPECS.iter().map(|(spec, _, _)| *spec).eq(MegaSpecId::ALL.iter().copied()));

        for (spec, expected_name, _) in ALL_SPECS {
            assert_eq!(<&'static str>::from(spec), expected_name);
            assert_eq!(MegaSpecId::from_str(expected_name).unwrap(), spec);
            assert_eq!(spec.to_string(), expected_name);
        }

        assert_eq!(MegaSpecId::default(), MegaSpecId::REX7);
        assert_eq!(MegaSpecId::from_str("unknown"), Err(UnknownHardfork));
    }

    /// The completeness anchor for [`MegaSpecId::ALL`]: `Default` tracks the latest spec (its
    /// own assertion above pins which), so a variant added without extending `ALL` fails here
    /// once the default advances. The const assertion on `ladder_index` covers order and gaps;
    /// this covers the tail.
    #[test]
    fn test_all_ends_at_the_latest_spec() {
        assert_eq!(*MegaSpecId::ALL.last().unwrap(), MegaSpecId::default());
    }

    #[test]
    fn test_all_specs_map_to_isthmus_and_prague() {
        for spec in MegaSpecId::ALL.iter().copied() {
            assert_eq!(spec.into_op_spec(), OpSpecId::ISTHMUS);
            assert_eq!(spec.into_eth_spec(), EthSpecId::PRAGUE);
            assert_eq!(revm::primitives::hardfork::SpecId::from(spec), EthSpecId::PRAGUE);
            assert_eq!(OpSpecId::from(spec), OpSpecId::ISTHMUS);
        }
    }

}
