//! The `MegaLimitControl` system contract for the `MegaETH` EVM.
//!
//! This contract currently provides a read-only query to return transaction-level remaining
//! compute gas.
//! The runtime result is produced by EVM interception, not by executing on-chain bytecode.

use alloy_evm::Database;
use alloy_primitives::{address, Address};
use revm::{
    database::State,
    state::{Account, Bytecode, EvmState},
};

use crate::MegaHardforks;

/// The address of the `MegaLimitControl` system contract.
pub const LIMIT_CONTROL_ADDRESS: Address = address!("0x6342000000000000000000000000000000000005");

/// The code of the `MegaLimitControl` contract (version 1.0.0).
pub use mega_system_contracts::limit_control::V1_0_0_CODE as LIMIT_CONTROL_CODE;

/// The code hash of the `MegaLimitControl` contract (version 1.0.0).
pub use mega_system_contracts::limit_control::V1_0_0_CODE_HASH as LIMIT_CONTROL_CODE_HASH;

pub use mega_system_contracts::limit_control::IMegaLimitControl;

/// Ensures the `MegaLimitControl` contract is deployed in the designated address and returns
/// the state changes.
/// The caller is responsible for committing the returned `EvmState` changes to the database.
pub fn transact_deploy_limit_control_contract<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    if !hardforks.is_rex_4_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // Load the MegaLimitControl contract account from the cache.
    let acc = db.load_cache_account(LIMIT_CONTROL_ADDRESS)?;

    // If already deployed with the same code hash, return early and mark as read.
    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == LIMIT_CONTROL_CODE_HASH {
            return Ok(Some(EvmState::from_iter([(
                LIMIT_CONTROL_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
    }

    // Update account with target bytecode.
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = LIMIT_CONTROL_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(LIMIT_CONTROL_CODE));

    // Convert back and mark as touched/created.
    let mut revm_acc: revm::state::Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    Ok(Some(EvmState::from_iter([(LIMIT_CONTROL_ADDRESS, revm_acc)])))
}
