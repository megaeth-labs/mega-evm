//! What a keyless deployment deploys, and what it reports.

use alloy_primitives::U256;
use mega_evm::system::keyless::tests::{
    CREATE2_FACTORY_CODE_HASH, CREATE2_FACTORY_CONTRACT, CREATE2_FACTORY_DEPLOYER,
    CREATE2_FACTORY_TX,
};

use super::*;

/// The canonical `CREATE2` factory deploys at its canonical address, from its canonical signer.
#[test]
fn test_the_create2_factory_deploys_at_its_address() {
    for gas_limit in GAS_LIMITS {
        let data = keyless_deploy_call(CREATE2_FACTORY_TX, U256::from(LARGE_OVERRIDE));
        let outcome = run_with(system_db(), data, gas_limit, EvmTxRuntimeLimits::no_limits());
        let ret = returned(&outcome);
        assert_eq!(ret.deployedAddress, CREATE2_FACTORY_CONTRACT, "at {gas_limit}");
        assert!(ret.errorData.is_empty());
        assert!(ret.gasUsed > 0);
        assert_eq!(code_hash(&outcome, CREATE2_FACTORY_CONTRACT), Some(CREATE2_FACTORY_CODE_HASH));
        assert_eq!(nonce(&outcome, CREATE2_FACTORY_DEPLOYER), 1, "the signer's nonce is spent");
    }
}
