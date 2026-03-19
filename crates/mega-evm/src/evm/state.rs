#[cfg(not(feature = "std"))]
use alloc as std;
use std::collections::BTreeMap;

use alloy_primitives::{map::hash_map::Entry, B256};
use revm::state::{Account, AccountStatus, EvmState, EvmStorage};

use auto_impl::auto_impl;
use revm::{database::State, Database};

/// A helper trait to get the block hashes used during transaction execution.
#[auto_impl(&, &mut, Box, Rc, Arc)]
pub trait BlockHashes {
    /// Get the block hashes used during transaction execution.
    fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256>;
}

impl<DB: Database> BlockHashes for State<DB> {
    fn get_accessed_block_hashes(&self) -> BTreeMap<u64, B256> {
        self.block_hashes.clone()
    }
}

/// Merges the other [`EvmState`] into the current one with account status also merged.
/// See more details in the [`merge_evm_state_optional_status`] function.
pub fn merge_evm_state(this: &mut EvmState, other: &EvmState) {
    merge_evm_state_optional_status(this, other, true);
}

/// Merges the other [`EvmState`] into the current one. The account status may or may not be merged
/// according to the `merge_status` parameter.
///
/// # Assumption
///
/// - The `other` `EvmState` is the result of the execution of a single transaction, or the merged
///   result of another [`merge_evm_state`] call. It must not be a partial `EvmState` in the middle
///   of a transaction execution.
/// - This function assumes that Cancun hardfork (EIP-6780) is activated.
/// - This function only works correctly post Cancun.
///
/// # Algorithm
///
/// Post Cancun (with EIP-6780), the account status is `SelfDestructed` only when the account is
/// created in the same transaction, i.e., if `SelfDestructed` flag is set, it must be "created ->
/// selfdestructed" in the same transaction and `Created` flag is also set. When merging `EvmState`s
/// (base `EvmState` <- new `EvmState`), we are doing this for each account:
/// - If the `SelfDestructed` (and `Created`) flag is set in the account status of the new
///   `EvmState`, override the account with an empty, touched account.
/// - Otherwise, we override the corresponding account and storage slots in the base `EvmState`.
///
/// # Coldness
///
/// When merging the state, if an account or storage slot exists in `this`, its coldness is
/// preserved; if an account or storage slot does not exist in `this`, it is marked as cold.
///
/// # Account Status
///
/// Optionally:
/// - `CreatedLocal` and `SelfDestructedLocal` on the `other` `EvmState` is cleared since they only
///   matter in the execution of the same transaction, while this merging between transactions.
/// - The `Cold` flag in the `this` `EvmState` is preserved.
/// - For other flags, they are merged from `other` into `this`.
///
/// We merge the state even if the account is not marked as `Touched`. This is because we may need
/// to know which account is read but not written to obtain `ReadSet` for witness generation.
pub fn merge_evm_state_optional_status(this: &mut EvmState, other: &EvmState, merge_status: bool) {
    for (address, account) in other {
        if account.is_selfdestructed() {
            // if the account is selfdestructed, we assert that the account is also created and do
            // nothing.
            assert!(
                account.is_created(),
                "Account is selfdestructed but not created. EIP-6780 must be applied."
            );
            // we will put an empty (equivalent to non-existent) account in the base state.
            // NOTE: we want to avoid marking an account as `SelfDestructed` since this may result
            // in a `BundleAccount` with `wipe_storage = true`. The underlying database in fact
            // cannot process such wiping storage action. Here, we do an early interpretation of
            // `SelfDestruct` by overriding the account with a default (empty) one and marking it as
            // `Touched`. The rationale is:
            // 1. The account is effectively selfdestructed, so its result state is equivalent to an
            //    empty account.
            // 2. There is no need to wipe storage in the database since this account is just
            //    created and destructed, there must be no storage data in the database.
            // 3. The account needs to be marked as `Touched` in case it pre-exists in the database
            //    and needs to be deleted.
            let mut empty_account = Account::default().with_touched_mark();
            match this.entry(*address) {
                Entry::Occupied(mut occupied_entry) => {
                    if occupied_entry.get().status.contains(AccountStatus::Cold) {
                        // if the account was cold, we need to preserve the coldness
                        empty_account.mark_cold();
                    }
                    occupied_entry.insert(empty_account);
                }
                Entry::Vacant(vacant_entry) => {
                    // if the account didn't exist, we mark it as cold
                    vacant_entry.insert(empty_account.with_cold_mark());
                }
            }
            continue;
        }

        // merge regardless of whether the account is touched or not
        match this.entry(*address) {
            Entry::Vacant(v) => {
                // if the account didn't exist, we mark it as cold
                let mut merged_account = account.clone().with_cold_mark();
                // all storage slots should be marked as cold
                for slot in merged_account.storage.values_mut() {
                    slot.mark_cold();
                }
                // `CreatedLocal` and `SelfDestructedLocal` are cleared since they only matter in
                // the execution of the same transaction, while this merging between transactions.
                merged_account.unmark_created_locally();
                merged_account.unmark_selfdestructed_locally();
                v.insert(merged_account);
            }
            Entry::Occupied(mut v) => {
                let this_account = v.get_mut();
                merge_account_state(this_account, account, merge_status);
            }
        }
    }
}

/// Merges the other [`Account`] into the current one.
///
/// # Assumption
///
/// The other account to merge is not flagged as `SelfDestructed`. See more details in
/// the [`merge_evm_state`] function.
fn merge_account_state(this: &mut Account, other: &Account, merge_status: bool) {
    this.info = other.info.clone();
    merge_evm_storage(&mut this.storage, &other.storage);
    if merge_status {
        merge_account_status(&mut this.status, other.status);
    }
}

/// Merges the other [`AccountStatus`] into the current one.
///
/// # Assumption
///
/// The other [`AccountStatus`] to merge is not flagged as `SelfDestructed`. See more details in
/// the [`merge_evm_state`] function.
fn merge_account_status(this: &mut AccountStatus, mut other: AccountStatus) {
    assert!(
        !other.contains(AccountStatus::SelfDestructed),
        "Account is selfdestructed and should not be merged."
    );
    // The coldness of `this` account should be preserved.
    // Remove the flags that only matter in the execution of the same transaction, while this
    // merging between transactions.
    other -= AccountStatus::Cold | AccountStatus::CreatedLocal | AccountStatus::SelfDestructedLocal;

    if this.contains(AccountStatus::SelfDestructed) && other.contains(AccountStatus::Created) {
        // if `this` account is selfdestructed, and `other` account is created,
        // we should no longer mark `this` as selfdestructed.
        *this -= AccountStatus::SelfDestructed;
    }

    // Other status flags are simply merged into `this`.
    *this |= other;
}

/// Merges the other [`EvmStorage`] into the current one.
///
/// See more details in the [`merge_evm_state`] function.
fn merge_evm_storage(this: &mut EvmStorage, other: &EvmStorage) {
    for (slot, slot_value) in other {
        match this.entry(*slot) {
            Entry::Vacant(v) => {
                let mut slot = slot_value.clone();
                // If this slot is not loaded, we mark it as cold.
                slot.mark_cold();
                v.insert(slot);
            }
            Entry::Occupied(mut v) => {
                let this_slot = v.get_mut();
                this_slot.present_value = slot_value.present_value;
                // The coldness of the slot is preserved.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, B256, U256};
    use revm::{
        database::{InMemoryDB, State},
        state::{AccountInfo, EvmStorageSlot},
    };

    const TEST_ADDRESS: Address = address!("1000000000000000000000000000000000000001");

    fn account_with_status(status: AccountStatus) -> Account {
        Account {
            info: AccountInfo {
                balance: U256::from(1),
                nonce: 1,
                code_hash: B256::ZERO,
                code: None,
            },
            transaction_id: 0,
            storage: Default::default(),
            status,
        }
    }

    #[test]
    fn test_state_exposes_accessed_block_hashes() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        state.block_hashes.insert(1, B256::ZERO);
        state.block_hashes.insert(2, B256::from([2_u8; 32]));

        let hashes = state.get_accessed_block_hashes();
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes.get(&1), Some(&B256::ZERO));
        assert_eq!(hashes.get(&2), Some(&B256::from([2_u8; 32])));
    }

    #[test]
    fn test_merge_evm_state_replaces_created_selfdestructed_accounts_with_empty_touched_accounts() {
        let mut this = EvmState::default();
        this.insert(TEST_ADDRESS, Account::default().with_cold_mark());

        let mut other_account = account_with_status(AccountStatus::Created);
        other_account.mark_selfdestruct();
        let other = EvmState::from_iter([(TEST_ADDRESS, other_account)]);

        merge_evm_state(&mut this, &other);

        let merged = this.get_mut(&TEST_ADDRESS).unwrap();
        assert!(merged.is_touched());
        assert!(!merged.is_selfdestructed());
        assert_eq!(merged.info, AccountInfo::default());
        assert!(merged.storage.is_empty());
        assert!(merged.mark_warm_with_transaction_id(0), "coldness should be preserved");
    }

    #[test]
    fn test_merge_evm_state_marks_new_accounts_and_storage_as_cold_and_clears_local_flags() {
        let mut other_account =
            account_with_status(AccountStatus::Created | AccountStatus::CreatedLocal);
        other_account.status |= AccountStatus::SelfDestructedLocal;
        other_account
            .storage
            .insert(U256::from(1), EvmStorageSlot::new_changed(U256::ZERO, U256::from(5), 0));
        let other = EvmState::from_iter([(TEST_ADDRESS, other_account)]);

        let mut this = EvmState::default();
        merge_evm_state(&mut this, &other);

        let merged = this.get(&TEST_ADDRESS).unwrap();
        assert!(merged.status.contains(AccountStatus::Cold));
        assert!(merged.status.contains(AccountStatus::Created));
        assert!(!merged.status.contains(AccountStatus::CreatedLocal));
        assert!(!merged.status.contains(AccountStatus::SelfDestructedLocal));

        let mut slot = merged.storage.get(&U256::from(1)).unwrap().clone();
        assert_eq!(slot.present_value(), U256::from(5));
        assert!(slot.mark_warm_with_transaction_id(0), "new slots should be marked cold");
    }

    #[test]
    fn test_merge_evm_state_optional_status_can_skip_or_merge_status_flags() {
        let mut base_account =
            account_with_status(AccountStatus::SelfDestructed | AccountStatus::Cold);
        let mut base_slot = EvmStorageSlot::new(U256::from(1), 0);
        base_slot.mark_cold();
        base_account.storage.insert(U256::from(1), base_slot);
        let base_state = EvmState::from_iter([(TEST_ADDRESS, base_account)]);

        let mut other_account =
            account_with_status(AccountStatus::Created | AccountStatus::CreatedLocal);
        other_account.info.balance = U256::from(9);
        other_account
            .storage
            .insert(U256::from(1), EvmStorageSlot::new_changed(U256::ZERO, U256::from(9), 0));
        let other_state = EvmState::from_iter([(TEST_ADDRESS, other_account)]);

        let mut without_status_merge = base_state.clone();
        merge_evm_state_optional_status(&mut without_status_merge, &other_state, false);
        let without_status_merge_account = without_status_merge.get(&TEST_ADDRESS).unwrap();
        assert!(without_status_merge_account.status.contains(AccountStatus::SelfDestructed));
        assert!(without_status_merge_account.status.contains(AccountStatus::Cold));
        assert!(!without_status_merge_account.status.contains(AccountStatus::Created));
        assert_eq!(
            without_status_merge_account.storage.get(&U256::from(1)).unwrap().present_value(),
            U256::from(9)
        );
        let mut preserved_slot =
            without_status_merge_account.storage.get(&U256::from(1)).unwrap().clone();
        assert!(preserved_slot.mark_warm_with_transaction_id(0));

        let mut with_status_merge = base_state;
        merge_evm_state(&mut with_status_merge, &other_state);
        let with_status_merge_account = with_status_merge.get(&TEST_ADDRESS).unwrap();
        assert!(!with_status_merge_account.is_selfdestructed());
        assert!(with_status_merge_account.status.contains(AccountStatus::Created));
        assert!(with_status_merge_account.status.contains(AccountStatus::Cold));
    }

    #[test]
    fn test_merge_empty_other_state_is_noop() {
        let mut this =
            EvmState::from_iter([(TEST_ADDRESS, account_with_status(AccountStatus::Cold))]);
        let other = EvmState::default();

        merge_evm_state(&mut this, &other);

        assert_eq!(this.len(), 1);
        let account = this.get(&TEST_ADDRESS).unwrap();
        assert_eq!(account.info.balance, U256::from(1));
    }

    #[test]
    fn test_merge_evm_state_overwrites_existing_account_storage_and_preserves_slot_coldness() {
        let addr2 = address!("2000000000000000000000000000000000000001");

        let mut base_account = account_with_status(AccountStatus::Touched | AccountStatus::Cold);
        let mut base_slot = EvmStorageSlot::new(U256::from(10), 0);
        base_slot.mark_cold();
        base_account.storage.insert(U256::from(1), base_slot);
        base_account.storage.insert(U256::from(2), EvmStorageSlot::new(U256::from(20), 0));
        let mut this = EvmState::from_iter([(TEST_ADDRESS, base_account)]);

        let mut other_account = account_with_status(AccountStatus::Touched);
        other_account.info.balance = U256::from(99);
        other_account
            .storage
            .insert(U256::from(1), EvmStorageSlot::new_changed(U256::from(10), U256::from(42), 0));
        other_account
            .storage
            .insert(U256::from(3), EvmStorageSlot::new_changed(U256::ZERO, U256::from(77), 0));
        let other = EvmState::from_iter([
            (TEST_ADDRESS, other_account),
            (addr2, account_with_status(AccountStatus::Touched)),
        ]);

        merge_evm_state(&mut this, &other);

        assert_eq!(this.len(), 2);

        let merged = this.get(&TEST_ADDRESS).unwrap();
        assert_eq!(merged.info.balance, U256::from(99));
        assert_eq!(merged.storage.get(&U256::from(1)).unwrap().present_value(), U256::from(42),);
        {
            let mut existing_slot = merged.storage.get(&U256::from(1)).unwrap().clone();
            assert!(
                existing_slot.mark_warm_with_transaction_id(0),
                "existing slot should have preserved original coldness"
            );
        }
        assert_eq!(merged.storage.get(&U256::from(2)).unwrap().present_value(), U256::from(20),);
        let mut new_slot = merged.storage.get(&U256::from(3)).unwrap().clone();
        assert_eq!(new_slot.present_value(), U256::from(77));
        assert!(new_slot.mark_warm_with_transaction_id(0), "new slot from other should be cold");

        let new_account = this.get(&addr2).unwrap();
        assert!(new_account.status.contains(AccountStatus::Cold));
    }

    #[test]
    fn test_merge_selfdestructed_account_not_in_base_is_inserted_as_cold_empty() {
        let mut other_account = account_with_status(AccountStatus::Created);
        other_account.mark_selfdestruct();
        other_account.info.balance = U256::from(999);
        let other = EvmState::from_iter([(TEST_ADDRESS, other_account)]);

        let mut this = EvmState::default();
        merge_evm_state(&mut this, &other);

        let merged = this.get(&TEST_ADDRESS).unwrap();
        assert!(merged.is_touched());
        assert!(!merged.is_selfdestructed());
        assert_eq!(merged.info, AccountInfo::default());
        assert!(merged.status.contains(AccountStatus::Cold));
    }

    #[test]
    #[should_panic(expected = "EIP-6780 must be applied")]
    fn test_merge_evm_state_panics_for_selfdestructed_account_without_created_flag() {
        let mut other_account = account_with_status(AccountStatus::Touched);
        other_account.mark_selfdestruct();
        other_account.status.remove(AccountStatus::Created);
        let other = EvmState::from_iter([(TEST_ADDRESS, other_account)]);

        let mut this = EvmState::default();
        merge_evm_state(&mut this, &other);
    }
}
