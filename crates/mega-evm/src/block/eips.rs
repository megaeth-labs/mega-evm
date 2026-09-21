//! The pre-block and post-block calls a block makes before and after its transactions.
//!
//! Each helper runs the call and hands back the state it produced; none of them commits. A
//! node's witness generator needs to see the read and write set of every step of block
//! execution, and a helper that committed straight to the database would hide its own.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::{boxed::Box, string::ToString};

use alloy_eips::{eip2935::HISTORY_STORAGE_ADDRESS, eip4788::BEACON_ROOTS_ADDRESS};
use alloy_evm::{
    block::{BlockExecutionError, BlockValidationError},
    Evm,
};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{Address, B256, U256};
use revm::{
    context::{result::ResultAndState, Block},
    database::State,
    state::{Account, EvmState},
    Database,
};

/// The caller every pre-block system call runs as.
pub use alloy_eips::eip4788::SYSTEM_ADDRESS;

/// Runs the pre-block call to the [EIP-2935] block hashes contract.
///
/// Answers `None`, having run nothing, when Prague is not active or the block is the genesis
/// block, where EIP-2935 makes no call. The state is not committed.
///
/// [EIP-2935]: https://eips.ethereum.org/EIPS/eip-2935
pub fn transact_blockhashes_contract_call<H: EthereumHardforks, E: Evm>(
    hardforks: H,
    parent_block_hash: B256,
    evm: &mut E,
) -> Result<Option<ResultAndState<E::HaltReason>>, BlockExecutionError> {
    if !hardforks.is_prague_active_at_timestamp(evm.block().timestamp().saturating_to()) {
        return Ok(None);
    }

    // The genesis block has no parent to record.
    if evm.block().number().is_zero() {
        return Ok(None);
    }

    evm.transact_system_call(SYSTEM_ADDRESS, HISTORY_STORAGE_ADDRESS, parent_block_hash.0.into())
        .map(Some)
        .map_err(|e| BlockValidationError::BlockHashContractCall { message: e.to_string() }.into())
}

/// Runs the pre-block call to the [EIP-4788] beacon block root contract.
///
/// Answers `None`, having run nothing, when Cancun is not active or the block is the genesis
/// block, where EIP-4788 makes no call and requires a zero root. The state is not committed.
///
/// [EIP-4788]: https://eips.ethereum.org/EIPS/eip-4788
pub fn transact_beacon_root_contract_call<H: EthereumHardforks, E: Evm>(
    hardforks: H,
    parent_beacon_block_root: Option<B256>,
    evm: &mut E,
) -> Result<Option<ResultAndState<E::HaltReason>>, BlockExecutionError> {
    if !hardforks.is_cancun_active_at_timestamp(evm.block().timestamp().saturating_to()) {
        return Ok(None);
    }

    let parent_beacon_block_root =
        parent_beacon_block_root.ok_or(BlockValidationError::MissingParentBeaconBlockRoot)?;

    if evm.block().number().is_zero() {
        if !parent_beacon_block_root.is_zero() {
            return Err(BlockValidationError::CancunGenesisParentBeaconBlockRootNotZero {
                parent_beacon_block_root,
            }
            .into());
        }
        return Ok(None);
    }

    evm.transact_system_call(
        SYSTEM_ADDRESS,
        BEACON_ROOTS_ADDRESS,
        parent_beacon_block_root.0.into(),
    )
    .map(Some)
    .map_err(|e| {
        BlockValidationError::BeaconRootContractCall {
            parent_beacon_block_root: Box::new(parent_beacon_block_root),
            message: e.to_string(),
        }
        .into()
    })
}

/// The state that applying `balances` to `db` would produce. Nothing is committed.
///
/// Equivalent to revm's own `increment_balances`, in state rather than in place: committing what
/// this returns leaves the same accounts, balances and account statuses behind, and a witness
/// generator gets to see the delta in between.
pub fn transact_balance_increments<DB: Database>(
    balances: impl IntoIterator<Item = (Address, u128)>,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    let mut state = EvmState::default();

    for (address, balance_increment) in balances {
        if balance_increment == 0 {
            continue;
        }
        let cache_account = db.load_cache_account(address)?;
        let account_info = cache_account.account_info().unwrap_or_default();
        let mut account = Account::default().with_info(account_info);
        account.info.balance += U256::from(balance_increment);
        account.mark_touch();
        state.insert(address, account);
    }

    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use revm::{database::InMemoryDB, state::AccountInfo, DatabaseCommit};
    use std::vec;

    #[test]
    fn test_balance_increment_commit_equivalence() {
        // The property that matters: this is equivalent to revm's `increment_balances`, which
        // the block executor would otherwise call in place.

        let addr1 = address!("0x1000000000000000000000000000000000000001");
        let addr2 = address!("0x2000000000000000000000000000000000000002");
        let addr3 = address!("0x3000000000000000000000000000000000000003");

        // Fund only addr1 and addr2, but leave addr3 empty (non-existent)
        let setup_db = |db: &mut InMemoryDB| {
            for (addr, balance, nonce) in [(addr1, 1000u64, 5u64), (addr2, 2000u64, 10u64)] {
                db.insert_account_info(
                    addr,
                    AccountInfo { balance: U256::from(balance), nonce, ..Default::default() },
                );
            }
        };

        let mut db1 = InMemoryDB::default();
        setup_db(&mut db1);
        let mut state1 = State::builder().with_database(&mut db1).build();

        let mut db2 = InMemoryDB::default();
        setup_db(&mut db2);
        let mut state2 = State::builder().with_database(&mut db2).build();

        let balance_increments = vec![(addr1, 100u128), (addr2, 200u128), (addr3, 300u128)];

        // In place, as revm does it.
        revm::database_interface::DatabaseCommitExt::increment_balances(
            &mut state1,
            balance_increments.clone(),
        )
        .expect("increment_balances should succeed");

        // As state, then committed: the commit is what applies the status transitions, so the
        // two paths must land on the same accounts.
        let result_state = transact_balance_increments(balance_increments.clone(), &mut state2)
            .expect("transact_balance_increments should succeed")
            .expect("Should return state");
        state2.commit(result_state);

        for (addr, _expected_increment) in balance_increments {
            let account1 = state1.load_cache_account(addr).expect("Should load from state1");
            let account2 = state2.load_cache_account(addr).expect("Should load from state2");

            let info1 = account1.account_info().expect("Should have account info");
            let info2 = account2.account_info().expect("Should have account info");

            assert_eq!(
                info1.balance, info2.balance,
                "Balance for {addr:?} should be identical after both methods"
            );
            assert_eq!(info1.nonce, info2.nonce, "Nonce for {addr:?} should be identical");
            assert_eq!(
                info1.code_hash, info2.code_hash,
                "Code hash for {addr:?} should be identical"
            );
            assert_eq!(
                account1.status, account2.status,
                "Account status for {addr:?} should be identical after both methods"
            );
        }
    }

    /// A zero increment is not a touch: it leaves no account in the state at all.
    #[test]
    fn test_zero_increments_leave_no_account_behind() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let produced = transact_balance_increments(
            [(address!("0x1000000000000000000000000000000000000001"), 0u128)],
            &mut state,
        )
        .expect("no database error")
        .expect("Should return state");

        assert!(produced.is_empty());
    }
}
