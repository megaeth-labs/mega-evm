//! The neutral configuration: an Ethereum fork's own pricing on Satin's machinery.
//!
//! The execution-spec gate runs Ethereum's fixtures through `MegaEvm` with every dimension of
//! pricing only `MegaETH` has turned off, so that a fixture fails on what the machinery does and
//! not on what `MegaETH` charges for. [`MegaContext::with_neutral_cfg`] is the switch; this module
//! builds what it is switched to for a fixture fork: the fork's gas schedule, its EIP switches,
//! its execution cap and code-size limits ([`neutral_cfg`]), and its precompile set
//! ([`neutral_precompiles`]), which the EVM carries rather than the context.
//!
//! Satin's base spec is Osaka, and a configuration cannot take a rule the base spec gates on its
//! own id away or add one: an Osaka rule stays on under an older fork's fixtures, and an
//! Amsterdam rule gated on the spec id stays off under Amsterdam's. So only Osaka and the fork
//! after it have a neutral configuration; what Amsterdam's cannot express is the gate's to
//! register, not this module's to hide.

use alloy_evm::precompiles::PrecompilesMap;
use revm::{
    context::CfgEnv,
    context_interface::cfg::GasParams,
    precompile::{PrecompileSpecId, Precompiles},
    primitives::eip7954,
};

use crate::{EthSpecId, MegaSpecId};

/// The configuration `fork`'s fixtures run under on Satin's machinery, or `None` for a fork
/// Satin's Osaka base cannot be configured to.
///
/// The fields [`MegaContext::with_cfg`](crate::MegaContext::with_cfg) fixes from the spec are
/// the fork's here, as the fixtures' reference runner has them (revm's
/// `CfgEnv::set_spec_and_mainnet_gas_params`):
///
/// - **Osaka**: Osaka's schedule; EIP-8037, EIP-2780 and EIP-7708 off; EIP-7825's execution cap and
///   the EIP-170 / EIP-3860 code-size limits, which revm derives from the base spec when the
///   configuration names none.
/// - **Amsterdam**: Amsterdam's schedule; EIP-8037 and EIP-2780 on; EIP-7708 on, which revm
///   otherwise keys on the spec id; EIP-7825's execution cap; the EIP-7954 code-size limits, set
///   explicitly for the same reason.
///
/// Every other field is revm's default: chain id 1, which a caller replaces with the fixture's.
pub fn neutral_cfg(fork: EthSpecId) -> Option<CfgEnv<MegaSpecId>> {
    let amsterdam = match fork {
        EthSpecId::OSAKA => false,
        EthSpecId::AMSTERDAM => true,
        _ => return None,
    };
    let mut cfg =
        CfgEnv::new_with_spec_and_gas_params(MegaSpecId::SATIN, GasParams::new_spec(fork));
    cfg.enable_amsterdam_eip8037 = amsterdam;
    cfg.enable_amsterdam_eip2780 = amsterdam;
    cfg.enable_amsterdam_eip7708 = amsterdam;
    cfg.tx_gas_limit_cap = None;
    cfg.system_call_state_gas_margin_in_reservoir = false;
    if amsterdam {
        cfg.limit_contract_code_size = Some(eip7954::MAX_CODE_SIZE);
        cfg.limit_contract_initcode_size = Some(eip7954::MAX_INITCODE_SIZE);
    } else {
        cfg.limit_contract_code_size = None;
        cfg.limit_contract_initcode_size = None;
    }
    Some(cfg)
}

/// `fork`'s own precompile set — Ethereum's, not op-revm's Karst set with `MegaETH`'s KZG price
/// — as the map a [`MegaEvm`](crate::MegaEvm) carries, or `None` where [`neutral_cfg`] has none.
pub fn neutral_precompiles(fork: EthSpecId) -> Option<PrecompilesMap> {
    neutral_cfg(fork)?;
    Some(PrecompilesMap::from_static(Precompiles::new(PrecompileSpecId::from_spec_id(fork))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{satin_gas_params, satin_precompiles, MegaContext};
    use revm::{
        context::{Cfg, ContextTr},
        database::EmptyDB,
        precompile::kzg_point_evaluation,
        primitives::{eip170, eip3860, eip7825},
    };
    use std::collections::BTreeSet;

    fn context(fork: EthSpecId) -> MegaContext<EmptyDB> {
        MegaContext::new(EmptyDB::default(), MegaSpecId::SATIN)
            .with_neutral_cfg(neutral_cfg(fork).expect("a neutral fork"))
    }

    /// What revm's own runner configures a fork's fixtures with, on the Ethereum spec.
    fn reference(fork: EthSpecId) -> CfgEnv<EthSpecId> {
        let mut cfg = CfgEnv::new();
        cfg.set_spec_and_mainnet_gas_params(fork);
        cfg
    }

    /// Osaka's configuration on the Karst base resolves every limit to what revm's runner
    /// resolves on Osaka.
    #[test]
    fn test_neutral_osaka_resolves_like_revm_on_osaka() {
        let ctx = context(EthSpecId::OSAKA);
        let (cfg, osaka) = (ctx.cfg(), reference(EthSpecId::OSAKA));
        assert_eq!(cfg.gas_params.table(), osaka.gas_params.table());
        assert!(!cfg.is_amsterdam_eip8037_enabled());
        assert!(!cfg.is_amsterdam_eip2780_enabled());
        assert!(!cfg.enable_amsterdam_eip7708);
        assert_eq!(cfg.tx_gas_limit_cap(), eip7825::TX_GAS_LIMIT_CAP);
        assert_eq!(cfg.tx_gas_limit_cap(), osaka.tx_gas_limit_cap());
        assert_eq!(cfg.max_code_size(), eip170::MAX_CODE_SIZE);
        assert_eq!(cfg.max_initcode_size(), eip3860::MAX_INITCODE_SIZE);
        assert_eq!(cfg.max_code_size(), osaka.max_code_size());
        assert_eq!(cfg.max_initcode_size(), osaka.max_initcode_size());
    }

    /// Amsterdam's configuration carries what revm's runner derives from the Amsterdam spec id,
    /// set explicitly because the Karst base does not derive it.
    #[test]
    fn test_neutral_amsterdam_resolves_like_revm_on_amsterdam() {
        let ctx = context(EthSpecId::AMSTERDAM);
        let (cfg, amsterdam) = (ctx.cfg(), reference(EthSpecId::AMSTERDAM));
        assert_eq!(cfg.gas_params.table(), amsterdam.gas_params.table());
        assert!(cfg.is_amsterdam_eip8037_enabled() && amsterdam.is_amsterdam_eip8037_enabled());
        assert!(cfg.is_amsterdam_eip2780_enabled() && amsterdam.is_amsterdam_eip2780_enabled());
        assert!(cfg.enable_amsterdam_eip7708);
        assert_eq!(cfg.tx_gas_limit_cap(), amsterdam.tx_gas_limit_cap());
        assert_eq!(cfg.max_code_size(), amsterdam.max_code_size());
        assert_eq!(cfg.max_initcode_size(), amsterdam.max_initcode_size());
        assert_eq!(cfg.max_code_size(), eip7954::MAX_CODE_SIZE);
    }

    /// Neither configuration is Satin's, and both run Satin's spec.
    #[test]
    fn test_neutral_configurations_are_not_satin_s() {
        for fork in [EthSpecId::OSAKA, EthSpecId::AMSTERDAM] {
            let ctx = context(fork);
            assert_eq!(ctx.spec(), MegaSpecId::SATIN);
            assert!(ctx.is_neutral());
            assert_ne!(ctx.cfg().gas_params.table(), satin_gas_params().table());
            assert_ne!(ctx.cfg().tx_gas_limit_cap, Some(crate::constants::TX_GAS_LIMIT_CAP));
        }
    }

    /// A fork before Osaka, or one Satin does not know, has no neutral configuration: the Osaka
    /// base cannot be configured back to it.
    #[test]
    fn test_only_osaka_and_amsterdam_are_neutral_forks() {
        for fork in [EthSpecId::FRONTIER, EthSpecId::CANCUN, EthSpecId::PRAGUE] {
            assert!(neutral_cfg(fork).is_none(), "{fork:?}");
            assert!(neutral_precompiles(fork).is_none(), "{fork:?}");
        }
    }

    /// The fork's precompile set is Ethereum's: the same addresses as the Satin set, with KZG
    /// point evaluation at upstream's 50,000 rather than `MegaETH`'s price.
    #[test]
    fn test_neutral_precompiles_are_ethereum_s() {
        let addresses = |p: &Precompiles| p.addresses().copied().collect::<BTreeSet<_>>();
        // Garbage input of the right length: upstream prices it before it fails to verify.
        let input = [0u8; 192];
        for fork in [EthSpecId::OSAKA, EthSpecId::AMSTERDAM] {
            let map = neutral_precompiles(fork).expect("a neutral fork");
            assert_eq!(
                map.addresses().copied().collect::<BTreeSet<_>>(),
                addresses(Precompiles::osaka())
            );
            assert_eq!(addresses(satin_precompiles()), addresses(Precompiles::osaka()));

            let out_of_gas = |set: &Precompiles| {
                let output = set
                    .get(&kzg_point_evaluation::ADDRESS)
                    .expect("KZG point evaluation")
                    .execute(&input, 60_000, 0)
                    .expect("no fatal error");
                output.status.halt_reason().is_some_and(|halt| halt.is_oog())
            };
            assert!(!out_of_gas(Precompiles::osaka()), "upstream's price fits in 60,000");
            assert!(out_of_gas(satin_precompiles()), "MegaETH's does not");
        }
    }
}
