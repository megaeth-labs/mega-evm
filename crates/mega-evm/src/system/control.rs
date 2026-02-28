//! The access control system contract for the `MegaETH` EVM.
//!
//! This contract provides functions to control access restrictions during EVM execution.
//! The first function, `disableVolatileDataAccess()`, prevents inner calls from accessing
//! volatile data (block env fields, beneficiary balance, oracle).

use alloy_evm::Database;
use alloy_primitives::{address, Address};
use alloy_sol_types::SolError;
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState},
};

use crate::MegaHardforks;

/// The address of the access control system contract.
pub const ACCESS_CONTROL_ADDRESS: Address = address!("0x6342000000000000000000000000000000000004");

/// The code of the access control contract (version 1.0.0).
pub use mega_system_contracts::access_control::V1_0_0_CODE as ACCESS_CONTROL_CODE;

/// The code hash of the access control contract (version 1.0.0).
pub use mega_system_contracts::access_control::V1_0_0_CODE_HASH as ACCESS_CONTROL_CODE_HASH;

pub use mega_system_contracts::access_control::IMegaAccessControl;
pub use IMegaAccessControl::VolatileDataAccessType;

/// Selector for `VolatileDataAccessDisabled(uint8)` custom error.
/// Selector: `keccak256("VolatileDataAccessDisabled(uint8)")[0..4]`.
pub const VOLATILE_DATA_ACCESS_DISABLED_SELECTOR: [u8; 4] =
    IMegaAccessControl::VolatileDataAccessDisabled::SELECTOR;

/// Builds the full ABI-encoded revert data for `VolatileDataAccessDisabled(uint8 accessType)`.
pub fn volatile_data_access_disabled_revert_data(
    access_type: VolatileDataAccessType,
) -> alloy_primitives::Bytes {
    alloy_primitives::Bytes::from(
        <IMegaAccessControl::VolatileDataAccessDisabled as SolError>::abi_encode(
            &IMegaAccessControl::VolatileDataAccessDisabled { accessType: access_type },
        ),
    )
}

/// ABI-encoded revert data for `DisabledByParent()` custom error.
pub const DISABLED_BY_PARENT_REVERT_DATA: [u8; 4] = IMegaAccessControl::DisabledByParent::SELECTOR;

/// Ensures the access control contract is deployed in the designated address and returns the
/// state changes.
/// The caller is responsible for committing the returned `EvmState` changes to the database.
pub fn transact_deploy_access_control_contract<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_rex_4_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // Load the access control contract account from the cache
    let acc = db.load_cache_account(ACCESS_CONTROL_ADDRESS)?;

    // If the contract is already deployed with the correct code, return early
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == ACCESS_CONTROL_CODE_HASH {
            return Ok(Some(EvmState::from_iter([(
                ACCESS_CONTROL_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Update the account info with the contract code
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = ACCESS_CONTROL_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(ACCESS_CONTROL_CODE));

    // Convert the cache account back into a revm account and mark it as touched.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    Ok(Some(EvmState::from_iter([(ACCESS_CONTROL_ADDRESS, revm_acc)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    #[test]
    fn test_volatile_data_access_disabled_selector() {
        let expected: [u8; 4] =
            keccak256("VolatileDataAccessDisabled(uint8)")[..4].try_into().unwrap();
        assert_eq!(
            VOLATILE_DATA_ACCESS_DISABLED_SELECTOR, expected,
            "Selector must match keccak256(\"VolatileDataAccessDisabled(uint8)\")[0..4]"
        );
    }

    #[test]
    fn test_disabled_by_parent_revert_data() {
        let expected: [u8; 4] = keccak256("DisabledByParent()")[..4].try_into().unwrap();
        assert_eq!(
            DISABLED_BY_PARENT_REVERT_DATA, expected,
            "Selector must match keccak256(\"DisabledByParent()\")[0..4]"
        );
    }
}
