//! Unit tests extracted from `crates/mega-evm/src/evm/result.rs` when T2.1 replaced the legacy core.
//! The code they test is at `git show a8f8c7c9:crates/mega-evm/src/evm/result.rs`.
//! Owning tickets are listed in `tests/_pending/README.md`.

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::result::OutOfGasError;

    #[test]
    fn test_base_halt_reasons_convert_roundtrip() {
        let eth_reason = EthHaltReason::OutOfGas(OutOfGasError::Basic);
        let mega_from_eth = MegaHaltReason::from(eth_reason);
        let mega_from_op = MegaHaltReason::from(OpHaltReason::Base(eth_reason));

        assert_eq!(mega_from_eth, MegaHaltReason::Base(OpHaltReason::Base(eth_reason)));
        assert_eq!(mega_from_op, MegaHaltReason::Base(OpHaltReason::Base(eth_reason)));
        assert_eq!(EthHaltReason::try_from(mega_from_eth).unwrap(), eth_reason);
        assert_eq!(EthHaltReason::try_from(mega_from_op).unwrap(), eth_reason);
    }

    #[test]
    fn test_mega_specific_halt_reasons_do_not_convert_to_eth() {
        let reason = MegaHaltReason::VolatileDataAccessOutOfGas {
            access_type: VolatileDataAccess::ORACLE,
            limit: 100,
            actual: 101,
        };

        assert_eq!(EthHaltReason::try_from(reason.clone()), Err(reason));
    }

    #[test]
    fn test_op_specific_halt_reason_does_not_convert_to_eth() {
        let op_reason = OpHaltReason::FailedDeposit;
        let mega = MegaHaltReason::from(op_reason.clone());
        assert_eq!(mega, MegaHaltReason::Base(op_reason));
        assert!(EthHaltReason::try_from(mega).is_err());
    }

    #[test]
    fn test_all_mega_specific_variants_fail_eth_conversion() {
        let variants: Vec<MegaHaltReason> = vec![
            MegaHaltReason::DataLimitExceeded { limit: 1, actual: 2 },
            MegaHaltReason::KVUpdateLimitExceeded { limit: 1, actual: 2 },
            MegaHaltReason::ComputeGasLimitExceeded { limit: 1, actual: 2 },
            MegaHaltReason::StateGrowthLimitExceeded { limit: 1, actual: 2 },
            MegaHaltReason::SystemTxInvalidCallee { callee: Address::ZERO },
        ];
        for variant in variants {
            assert!(
                EthHaltReason::try_from(variant).is_err(),
                "mega-specific variant should not convert to EthHaltReason"
            );
        }
    }
}
