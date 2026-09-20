//! The system contracts of the Satin engine.
//!
//! Six contracts live at fixed addresses under `0x6342…`: the [`Oracle`](oracle),
//! the [`High-Precision Timestamp`](timestamp) wrapper, [`KeylessDeploy`](keyless),
//! [`MegaAccessControl`](control), [`MegaLimitControl`](limit_control) and the
//! [`SequencerRegistry`](sequencer_registry). Each module carries its address, the code and code
//! hash the `mega-system-contracts` crate ships, and its ABI.
//!
//! Four of them have an interceptor: a `CALL` or `STATICCALL` to one of their intercepted
//! selectors is answered by the engine instead of by the contract's code (see the `intercept`
//! module for the dispatch order and the shape of an answer). The timestamp wrapper and the
//! `SequencerRegistry` run their bytecode.
//!
//! [`tx`](tx) holds the system-address transaction: the deposit-like transaction the sequencer
//! maintains the protocol's own state with.
//!
//! Deploying the contracts at the fork that activates them belongs to system contract
//! deployment; it is not here.

pub mod keyless;

mod control;
mod intercept;
mod limit_control;
mod oracle;
mod sequencer_registry;
mod timestamp;
mod tx;

pub use control::*;
pub use intercept::NON_ZERO_TRANSFER_REVERT_DATA;
pub use limit_control::*;
pub use oracle::*;
pub use sequencer_registry::*;
pub use timestamp::*;
pub use tx::*;

pub(crate) use intercept::intercept;
pub(crate) use tx::validate_and_promote;

#[cfg(test)]
mod tests {
    use super::{keyless::*, *};
    use alloy_primitives::{keccak256, Address, Bytes, B256};

    /// Every system contract, as `(address, code, code hash)`.
    fn system_contracts() -> [(Address, Bytes, B256); 6] {
        [
            (ORACLE_CONTRACT_ADDRESS, ORACLE_CONTRACT_CODE, ORACLE_CONTRACT_CODE_HASH),
            (
                HIGH_PRECISION_TIMESTAMP_ORACLE_ADDRESS,
                HIGH_PRECISION_TIMESTAMP_ORACLE_CODE,
                HIGH_PRECISION_TIMESTAMP_ORACLE_CODE_HASH,
            ),
            (KEYLESS_DEPLOY_ADDRESS, KEYLESS_DEPLOY_CODE, KEYLESS_DEPLOY_CODE_HASH),
            (ACCESS_CONTROL_ADDRESS, ACCESS_CONTROL_CODE, ACCESS_CONTROL_CODE_HASH),
            (LIMIT_CONTROL_ADDRESS, LIMIT_CONTROL_CODE, LIMIT_CONTROL_CODE_HASH),
            (SEQUENCER_REGISTRY_ADDRESS, SEQUENCER_REGISTRY_CODE, SEQUENCER_REGISTRY_CODE_HASH),
        ]
    }

    /// The code hash of every contract is the hash of the code it ships with: Satin deploys the
    /// contracts as they are, so a Solidity change that moved a hash shows up here.
    #[test]
    fn test_every_system_contract_code_hashes_to_its_pinned_hash() {
        for (address, code, hash) in system_contracts() {
            assert!(!code.is_empty(), "{address} has no code");
            assert_eq!(keccak256(&code), hash, "the code hash of {address} moved");
        }
    }

    /// The six addresses are the six of the `0x6342…` range, each used once.
    #[test]
    fn test_the_system_contract_addresses_are_the_pinned_ones() {
        let addresses: [Address; 6] = core::array::from_fn(|i| system_contracts()[i].0);
        assert_eq!(
            addresses.map(|address| address.to_string()),
            [
                "0x6342000000000000000000000000000000000001",
                "0x6342000000000000000000000000000000000002",
                "0x6342000000000000000000000000000000000003",
                "0x6342000000000000000000000000000000000004",
                "0x6342000000000000000000000000000000000005",
                "0x6342000000000000000000000000000000000006",
            ]
            .map(str::to_string),
        );
    }
}
