#[cfg(not(feature = "std"))]
use alloc as std;
use std::{boxed::Box, string::ToString};

use alloy_evm::{
    block::{BlockExecutionError, BlockValidationError},
    Evm,
};
use alloy_primitives::{Address, B256, U256};
use revm::{
    context_interface::result::ResultAndState,
    state::{Account, EvmState, TransactionId},
    Database, Inspector,
};

use crate::{
    block::hardfork::MegaHardforks, ExternalEnvTypes, MegaContext, MegaEvm, MegaHaltReason,
    MegaSpecId,
};

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
/// Rex5+: the system call is issued with `max(block.gas_limit, SYSTEM_CALL_GAS_LIMIT_FLOOR)`
/// (matching `SequencerRegistry` pre-block helpers) instead of the frozen pre-REX5 30M
/// default. The EIP-2935 history-storage write cost depends on Rex5 dynamic storage gas, so
/// the 30M default is no longer guaranteed to be enough on high-SALT-bucket blocks.
/// Pre-Rex5 keeps [`crate::constants::PRE_REX5_SYSTEM_CALL_GAS_LIMIT`] (30M) for replay
/// parity — not upstream `SYSTEM_CALL_GAS_LIMIT`, which revm 40 raised to `31_566_720`.
///
/// [EIP-2935]: https://eips.ethereum.org/EIPS/eip-2935
#[inline]
pub(crate) fn transact_blockhashes_contract_call<H, DB, INSP, ExtEnvs>(
    hardforks: H,
    setup_spec: MegaSpecId,
    parent_block_hash: B256,
    evm: &mut MegaEvm<DB, INSP, ExtEnvs>,
) -> Result<Option<ResultAndState<MegaHaltReason>>, BlockExecutionError>
where
    H: MegaHardforks,
    DB: alloy_evm::Database,
    ExtEnvs: ExternalEnvTypes,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
{
    let block_timestamp: u64 = evm.block().timestamp.saturating_to();
    if !hardforks.is_prague_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    // if the block number is zero (genesis block) then no system transaction may occur as per
    // EIP-2935
    if evm.block().number.is_zero() {
        return Ok(None);
    }

    let res = if setup_spec.reaches(MegaSpecId::REX5) {
        let gas_limit =
            evm.block().gas_limit.max(crate::constants::rex5::SYSTEM_CALL_GAS_LIMIT_FLOOR);
        evm.transact_system_call_with_gas_limit(
            alloy_eips::eip4788::SYSTEM_ADDRESS,
            alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
            parent_block_hash.0.into(),
            gas_limit,
        )
    } else {
        // Use the inherent entry point so both branches share the same error type; the
        // `alloy_evm::Evm` method now reports `OpTxError`.
        // Pre-REX5 keeps MegaETH's frozen 30M system-call gas limit (not upstream's
        // revm 40 `SYSTEM_CALL_GAS_LIMIT`, which includes a state-gas reservoir).
        evm.transact_system_call_with_gas_limit(
            alloy_eips::eip4788::SYSTEM_ADDRESS,
            alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
            parent_block_hash.0.into(),
            crate::constants::PRE_REX5_SYSTEM_CALL_GAS_LIMIT,
        )
    };

    match res {
        Ok(res) => Ok(Some(res)),
        Err(e) => {
            Err(BlockValidationError::BlockHashContractCall { message: e.to_string() }.into())
        }
    }
}

/// Applies the pre-block call to the [EIP-4788] beacon block root contract, using the given block,
/// chain spec, EVM.
///
/// Note: this does not commit the state changes to the database, it only transact the call.
///
/// Returns `None` if Cancun is not active or the block is the genesis block, otherwise returns the
/// result of the call.
///
/// Rex5+: see [`transact_blockhashes_contract_call`] for the gas-limit override rationale —
/// the beacon-root SSTORE shares the same dynamic-storage-gas exposure.
///
/// [EIP-4788]: https://eips.ethereum.org/EIPS/eip-4788
#[inline]
pub(crate) fn transact_beacon_root_contract_call<H, DB, INSP, ExtEnvs>(
    hardforks: H,
    setup_spec: MegaSpecId,
    parent_beacon_block_root: Option<B256>,
    evm: &mut MegaEvm<DB, INSP, ExtEnvs>,
) -> Result<Option<ResultAndState<MegaHaltReason>>, BlockExecutionError>
where
    H: MegaHardforks,
    DB: alloy_evm::Database,
    ExtEnvs: ExternalEnvTypes,
    INSP: Inspector<MegaContext<DB, ExtEnvs>>,
{
    let block_timestamp: u64 = evm.block().timestamp.saturating_to();
    if !hardforks.is_cancun_active_at_timestamp(block_timestamp) {
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

    let res = if setup_spec.reaches(MegaSpecId::REX5) {
        let gas_limit =
            evm.block().gas_limit.max(crate::constants::rex5::SYSTEM_CALL_GAS_LIMIT_FLOOR);
        evm.transact_system_call_with_gas_limit(
            alloy_eips::eip4788::SYSTEM_ADDRESS,
            alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
            parent_beacon_block_root.0.into(),
            gas_limit,
        )
    } else {
        // Pre-REX5 keeps MegaETH's frozen 30M system-call gas limit; see EIP-2935 helper.
        evm.transact_system_call_with_gas_limit(
            alloy_eips::eip4788::SYSTEM_ADDRESS,
            alloy_eips::eip4788::BEACON_ROOTS_ADDRESS,
            parent_beacon_block_root.0.into(),
            crate::constants::PRE_REX5_SYSTEM_CALL_GAS_LIMIT,
        )
    };

    match res {
        Ok(res) => Ok(Some(res)),
        Err(e) => Err(BlockValidationError::BeaconRootContractCall {
            parent_beacon_block_root: Box::new(parent_beacon_block_root),
            message: e.to_string(),
        }
        .into()),
    }
}

/// Transacts the balance increments and returns the post evm state. Note that the changes are not
/// committed to the given db.
///
/// This is [`revm::database_interface::DatabaseCommitExt::increment_balances`] with the commit
/// removed: pre-block helpers return their state so the executor commits it through the one path
/// the state hook is installed on. Everything else follows the upstream body, including how each
/// account is built — an account that exists carries its pre-increment info as the original, and
/// one that does not is marked as loaded-not-existing. `CacheState::apply_account_state` reads
/// both when the account is not already cached, to decide the baseline it records for the block,
/// so building them any other way would make the returned state describe a different prior state
/// than the chain had.
pub(crate) fn transact_balance_increments<DB: Database>(
    balances: impl IntoIterator<Item = (Address, u128)>,
    db: &mut DB,
) -> Result<Option<EvmState>, DB::Error> {
    let balances = balances.into_iter();
    let mut state = EvmState::default();

    for (address, balance_increment) in balances {
        if balance_increment == 0 {
            continue;
        }
        let mut account = match db.basic(address)? {
            Some(info) => Account::from(info),
            None => Account::new_not_existing(TransactionId::ZERO),
        };
        account.info.balance = account.info.balance.saturating_add(U256::from(balance_increment));
        account.mark_touch();
        state.insert(address, account);
    }

    Ok(Some(state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;
    use revm::{
        database::{states::AccountStatus, InMemoryDB, State},
        database_interface::DatabaseCommitExt,
        state::AccountInfo,
        DatabaseCommit,
    };

    /// The returned state has to describe the chain's prior state on its own, because
    /// `CacheState::apply_account_state` reads the account's original info and its
    /// loaded-not-existing marker whenever the account is not already in the cache — the branch
    /// the executor's own call never reaches, since it reads and commits through the same
    /// `State`.
    ///
    /// Committing into a `State` that has never seen these accounts takes exactly that branch. An
    /// account that held a balance must come out with that balance as its baseline, and one that
    /// never existed must come out as not existing, rather than both collapsing to
    /// "existed and was empty".
    #[test]
    fn test_returned_state_carries_the_prior_state_into_an_uncached_commit() {
        let funded = address!("0x1000000000000000000000000000000000000001");
        let absent = address!("0x3000000000000000000000000000000000000003");

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            funded,
            AccountInfo { balance: U256::from(1000u64), nonce: 5, ..Default::default() },
        );

        // Read through one `State`, commit through another that has cached nothing.
        let mut read_state = State::builder().with_database(&mut db).build();
        let increments = vec![(funded, 100u128), (absent, 300u128)];
        let produced = transact_balance_increments(increments, &mut read_state)
            .expect("balance increments should succeed")
            .expect("balance increments always produce a state");

        let mut fresh_db = InMemoryDB::default();
        let mut commit_state =
            State::builder().with_database(&mut fresh_db).with_bundle_update().build();
        commit_state.commit(produced);

        let transitions = commit_state
            .transition_state
            .as_ref()
            .expect("bundle updates record the transitions this assertion reads");

        let funded_previous = transitions.transitions[&funded]
            .previous_info
            .as_ref()
            .expect("an account that existed must keep its prior info as the baseline");
        assert_eq!(
            funded_previous.balance,
            U256::from(1000u64),
            "the baseline must be the balance before the increment, not an empty account",
        );
        assert_eq!(funded_previous.nonce, 5, "the baseline must carry the account's prior nonce");
        assert_eq!(
            transitions.transitions[&funded].previous_status,
            AccountStatus::Loaded,
            "an account that existed must not be recorded as previously empty",
        );

        assert_eq!(
            transitions.transitions[&absent].previous_status,
            AccountStatus::LoadedNotExisting,
            "an account that never existed must not be recorded as previously empty",
        );
        assert!(
            transitions.transitions[&absent].previous_info.is_none(),
            "an account that never existed must not gain a prior state",
        );
    }

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
                        account_id: None,
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

        // Method 2: Refactored approach (transact + commit, matching the actual usage pattern)
        // The refactoring separates increment_balances into two steps:
        // 1. transact_balance_increments() - produces EvmState delta
        // 2. commit() - integrates the delta and fixes status transitions
        // This lets the executor collect the intermediate state as a pre-block outcome and
        // commit it, which is what fires the witness-recording state hook.
        let result_state = transact_balance_increments(balance_increments.clone(), &mut state2)
            .expect("transact_balance_increments should succeed")
            .expect("Should return state");

        // Commit the state changes (this applies proper status transitions via apply_evm_state)
        state2.commit(result_state);

        // Verify final states match after both approaches
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
            assert_eq!(
                account1.status, account2.status,
                "Account status for {:?} should be identical after both methods",
                addr
            );
        }
    }
}
