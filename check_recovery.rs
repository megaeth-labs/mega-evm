use mega_evm::MegaTxEnvelope;
use mega_evm::revm::primitives::Address;

pub fn check(env: MegaTxEnvelope) -> Option<Address> {
    env.recover_signer().ok()
}
