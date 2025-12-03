#[cfg(not(feature = "std"))]
use alloc as std;
use alloy_evm::{
    block::{BlockExecutionError, BlockValidationError},
    Evm,
};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{map::foldhash::HashMapExt, Address, B256, U256};
use revm::{
    context_interface::result::ResultAndState,
    database::State,
    state::{Account, EvmState},
    Database,
};
use std::string::ToString;

/// Applies the pre-block call to the [EIP-2935] blockhashes contract, using the given block,
/// chain specification, and EVM.
///
/// If Prague is not activated, or the block is the genesis block, then this is a no-op, and no
/// state changes are made.
///
/// Note: this does not commit the state changes to the database, it only transact the call.
///
/// Returns `None` if Prague is not active or the block is the genesis block, otherwise returns the
/// result of the call.
///
/// [EIP-2935]: https://eips.ethereum.org/EIPS/eip-2935
#[inline]
pub(crate) fn transact_blockhashes_contract_call<Halt>(
    spec: impl EthereumHardforks,
    parent_block_hash: B256,
    evm: &mut impl Evm<HaltReason = Halt>,
) -> Result<Option<ResultAndState<Halt>>, BlockExecutionError> {
    if !spec.is_prague_active_at_timestamp(evm.block().timestamp.saturating_to()) {
        return Ok(None);
    }

    // if the block number is zero (genesis block) then no system transaction may occur as per
    // EIP-2935
    if evm.block().number.is_zero() {
        return Ok(None);
    }

    let res = match evm.transact_system_call(
        alloy_eips::eip4788::SYSTEM_ADDRESS,
        alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
        parent_block_hash.0.into(),
    ) {
        Ok(res) => res,
        Err(e) => {
            return Err(BlockValidationError::BlockHashContractCall { message: e.to_string() }.into())
        }
    };

    Ok(Some(res))
}

/// Applies the pre-block call to the [EIP-4788] beacon block root contract, using the given block,
/// chain spec, EVM.
///
/// Note: this does not commit the state changes to the database, it only transact the call.
///
/// Returns `None` if Cancun is not active or the block is the genesis block, otherwise returns the
/// result of the call.
///
/// [EIP-4788]: https://eips.ethereum.org/EIPS/eip-4788
#[inline]
pub(crate) fn transact_beacon_root_contract_call<Halt>(
    spec: impl EthereumHardforks,
    parent_beacon_block_root: Option<B256>,
    evm: &mut impl Evm<HaltReason = Halt>,
) -> Result<Option<ResultAndState<Halt>>, BlockExecutionError> {
    if !spec.is_cancun_active_at_timestamp(evm.block().timestamp.saturating_to()) {
        return Ok(None);
    }

    let parent_beacon_block_root =
        parent_beacon_block_root.ok_or(BlockValidationError::MissingParentBeaconBlockRoot)?;

    // if the block number is zero (genesis block) then the parent beacon block root must
    // be 0x0 and no system transaction may occur as per EIP-4788
    if evm.block().number.is_zero() {
        if !parent_beacon_block_root.is_zero() {
            return Err(BlockValidationError::CancunGenesisParentBeaconBlockRootNotZero {
                parent_beacon_block_root,
            }
            .into());
        }
        return Ok(None);
    }

    let res = match evm.transact_system_call(
        alloy_eips::eip4788::SYSTEM_ADDRESS,
        alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
        parent_beacon_block_root.0.into(),
    ) {
        Ok(res) => res,
        Err(e) => {
            return Err(BlockValidationError::BeaconRootContractCall {
                parent_beacon_block_root: Box::new(parent_beacon_block_root),
                message: e.to_string(),
            }
            .into())
        }
    };

    Ok(Some(res))
}

/// Transacts the balance increments and returns the post evm state. Note that the changes are not
/// committed to the given db.
pub(crate) fn transact_balance_increments<DB: Database>(
    balances: impl IntoIterator<Item = (Address, u128)>,
    db: &mut State<DB>,
) -> Result<Option<EvmState>, DB::Error> {
    let balances = balances.into_iter();
    let mut state = EvmState::with_capacity(balances.size_hint().0);

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

    #[test]
    fn test_balance_increment_commit_equivalence() {
        // This is the most important test: verify that our approach is equivalent
        // to the original increment_balances + balance_increment_state approach

        // Setup: create two identical database states
        let addr1 = address!("0x1000000000000000000000000000000000000001");
        let addr2 = address!("0x2000000000000000000000000000000000000002");
        let addr3 = address!("0x3000000000000000000000000000000000000003");

        // Fund only addr1 and addr2, but leave addr3 empty (non-existent)
        let setup_db = |db: &mut InMemoryDB| {
            for (addr, balance, nonce) in [(addr1, 1000u64, 5u64), (addr2, 2000u64, 10u64)] {
                db.insert_account_info(
                    addr,
                    AccountInfo {
                        balance: U256::from(balance),
                        nonce,
                        code_hash: alloy_primitives::B256::ZERO,
                        code: None,
                    },
                );
            }
        };

        // Setup database 1: will use original approach (increment_balances)
        let mut db1 = InMemoryDB::default();
        setup_db(&mut db1);
        let mut state1 = State::builder().with_database(&mut db1).build();

        // Setup database 2: will use our approach (transact_balance_increments + commit)
        let mut db2 = InMemoryDB::default();
        setup_db(&mut db2);
        let mut state2 = State::builder().with_database(&mut db2).build();

        // Apply balance increments
        let balance_increments = vec![(addr1, 100u128), (addr2, 200u128), (addr3, 300u128)];

        // Method 1: Original approach
        state1
            .increment_balances(balance_increments.clone())
            .expect("increment_balances should succeed");

        // Method 2: Our approach
        let result_state = transact_balance_increments(balance_increments.clone(), &mut state2)
            .expect("transact_balance_increments should succeed")
            .expect("Should return state");
        state2.commit(result_state);

        // Now verify that both states have identical results
        for (addr, _expected_increment) in balance_increments {
            let account1 = state1.load_cache_account(addr).expect("Should load from state1");
            let account2 = state2.load_cache_account(addr).expect("Should load from state2");

            let info1 = account1.account_info().expect("Should have account info");
            let info2 = account2.account_info().expect("Should have account info");

            assert_eq!(
                info1.balance, info2.balance,
                "Balance for {:?} should be identical after both methods",
                addr
            );
            assert_eq!(info1.nonce, info2.nonce, "Nonce for {:?} should be identical", addr);
            assert_eq!(
                info1.code_hash, info2.code_hash,
                "Code hash for {:?} should be identical",
                addr
            );
        }
    }
}
