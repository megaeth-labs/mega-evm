//! The remaining compute gas system contract for the `MegaETH` EVM.
//!
//! This contract provides a read-only query to return transaction-level remaining compute gas.
//! The runtime result is produced by EVM interception, not by executing on-chain bytecode.

use alloy_evm::Database;
use alloy_primitives::{address, Address};
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState},
};

use crate::MegaHardforks;

/// The address of the remaining compute gas system contract.
pub const REMAINING_COMPUTE_GAS_ADDRESS: Address =
    address!("0x6342000000000000000000000000000000000005");

/// The code of the remaining compute gas contract (version 1.0.0).
pub use mega_system_contracts::remaining_compute_gas::V1_0_0_CODE as REMAINING_COMPUTE_GAS_CODE;

/// The code hash of the remaining compute gas contract (version 1.0.0).
pub use mega_system_contracts::remaining_compute_gas::V1_0_0_CODE_HASH as REMAINING_COMPUTE_GAS_CODE_HASH;

pub use mega_system_contracts::remaining_compute_gas::IRemainingComputeGas;

/// Ensures the remaining compute gas contract is deployed in the designated address and returns
/// the state changes.
/// The caller is responsible for committing the returned `EvmState` changes to the database.
pub fn transact_deploy_remaining_compute_gas_contract<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_rex_4_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // Load the remaining compute gas contract account from the cache.
    let acc = db.load_cache_account(REMAINING_COMPUTE_GAS_ADDRESS)?;

    // If already deployed with the same code hash, return early and mark as read.
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == REMAINING_COMPUTE_GAS_CODE_HASH {
            return Ok(Some(EvmState::from_iter([(
                REMAINING_COMPUTE_GAS_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Update account with target bytecode.
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = REMAINING_COMPUTE_GAS_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(REMAINING_COMPUTE_GAS_CODE));

    // Convert back and mark as touched/created.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    Ok(Some(EvmState::from_iter([(REMAINING_COMPUTE_GAS_ADDRESS, revm_acc)])))
}
