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

/// Merges the other [`EvmState`] into the current one.
///
/// # Assumption
/// This function assumes that Cancun hardfork is activated.
/// This function only works correctly post Cancun.
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
/// We merge the state even if the account is not marked as `Touched`. This is because we may need
/// to know which account is read but not written to obtain `ReadSet` for witness generation.
pub fn merge_evm_state(this: &mut EvmState, other: &EvmState) -> usize {
    let mut touched_slot: usize = 0;
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
            this.insert(*address, Account::default().with_touched_mark());
            continue;
        }

        // merge regardless of whether the account is touched or not
        match this.entry(*address) {
            Entry::Vacant(v) => {
                v.insert(account.clone());
            }
            Entry::Occupied(mut v) => {
                let this_account = v.get_mut();
                merge_account_state(this_account, account);
            }
        }
        if account.is_touched() {
            touched_slot += if account.storage.is_empty() { 1 } else { account.storage.len() };
        }
    }
    touched_slot
}

/// Merges the other [`Account`] into the current one.
///
/// # Assumption
///
/// The other account to merge is not flagged as `SelfDestructed`. See more details in
/// the [`merge_evm_state`] function.
fn merge_account_state(this: &mut Account, other: &Account) {
    assert!(
        !other.status.contains(AccountStatus::SelfDestructed),
        "Account is selfdestructed and should not be merged."
    );
    this.info = other.info.clone();
    merge_evm_storage(&mut this.storage, &other.storage);
    // Account status is merged.
    this.status |= other.status;
}

/// Merges the other [`EvmStorage`] into the current one.
///
/// # Warn
///
/// The [`EvmStorageSlot::is_cold`](revm::state::EvmStorageSlot::is_cold) is simply overwritten.
/// It may not reflect the actual status of the slot.
fn merge_evm_storage(this: &mut EvmStorage, other: &EvmStorage) {
    for (slot, slot_value) in other {
        match this.entry(*slot) {
            Entry::Vacant(v) => {
                v.insert(slot_value.clone());
            }
            Entry::Occupied(mut v) => {
                let this_slot = v.get_mut();
                this_slot.present_value = slot_value.present_value;
                this_slot.is_cold = slot_value.is_cold;
            }
        }
    }
}
