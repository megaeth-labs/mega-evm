//! Sandbox state merge logic.
//!
//! Keeps the execution entry point focused on validation and sandbox orchestration while this
//! module owns the replay-sensitive details of applying sandbox state to the parent journal.

#[cfg(not(feature = "std"))]
use alloc as std;
use std::vec::Vec;

use alloy_evm::Database as AlloyDatabase;
use alloy_primitives::{Address, U256};
use revm::{context::ContextTr, primitives::KECCAK_EMPTY, state::EvmState, Journal, JournalEntry};
use tracing::error;

use crate::{
    merge_evm_state_optional_status, ExternalEnvTypes, JournalInspectTr, MegaContext, MegaSpecId,
};

use super::error::KeylessDeployError;

/// Applies all state changes from sandbox execution to the parent journal.
///
/// All accounts from the sandbox view are merged into the parent journal — including accounts that
/// were only read and not written — because the `ReadSet` for stateless witness generation requires
/// knowing which accounts a sandbox touched.
///
/// Coldness metadata is NOT merged: sandbox state changes are treated as a silent database update
/// that does not affect the parent transaction's own observable coldness-dependent gas costs.
/// Pre-Rex5 also keeps the legacy "do not merge status into existing parent accounts" behavior.
/// Rex5 records only the status entries needed for stateful diffs, such as created/touched account
/// markers, so normal journal commit/revert semantics stay correct.
///
/// The sandbox runs with the signer's nonce overridden to `0` (Nick's Method), and the post-CREATE
/// nonce of `1` is intentionally preserved in the merged state to record that the signer has been
/// used for a keyless deploy.
///
/// Pre-Rex5 keeps the legacy direct merge for replay compatibility. Rex5 records state-changing
/// diffs as journal entries so that an outer-frame revert or halt unwinds the sandbox's effects via
/// the normal checkpoint-revert mechanism. The Rex5 merge still keeps read-only accounts, bytecode,
/// and storage slots in the parent journal cache for witness generation; revm's default journal
/// entries can mark those cache entries cold again but cannot remove them from the cache map.
pub(super) fn apply_sandbox_state<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    sandbox_state: EvmState,
    _deploy_signer: Address,
) -> Result<(), KeylessDeployError> {
    if ctx.spec.is_enabled(MegaSpecId::REX5) {
        apply_sandbox_state_journaled(ctx, sandbox_state)
    } else {
        apply_sandbox_state_legacy(ctx, &sandbox_state);
        Ok(())
    }
}

/// Applies sandbox state with the pre-Rex5 direct merge.
///
/// This intentionally bypasses revm journal entries to preserve historical replay semantics for
/// stable specs.
fn apply_sandbox_state_legacy<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    sandbox_state: &EvmState,
) {
    let journal = ctx.journal_mut();
    merge_evm_state_optional_status(&mut journal.state, sandbox_state, false);
}

/// Applies sandbox state through an internal journal checkpoint.
///
/// The checkpoint makes defensive merge errors atomic: partial journal entries are reverted before
/// the error is returned, while successful entries are committed into the caller's active
/// checkpoint.
fn apply_sandbox_state_journaled<DB: AlloyDatabase, ExtEnvs: ExternalEnvTypes>(
    ctx: &mut MegaContext<DB, ExtEnvs>,
    sandbox_state: EvmState,
) -> Result<(), KeylessDeployError> {
    let journal = ctx.journal_mut();
    let checkpoint = journal.inner.checkpoint();
    match apply_sandbox_state_journaled_inner(journal, sandbox_state) {
        Ok(()) => {
            journal.inner.checkpoint_commit();
            Ok(())
        }
        Err(error) => {
            journal.inner.checkpoint_revert(checkpoint);
            Err(error)
        }
    }
}

/// Replays sandbox account diffs into the parent journal as revm journal entries.
///
/// Stateful diffs become revertible by any enclosing frame checkpoint. Read/cache-only accounts,
/// bytecode, and storage slots may remain in the parent journal cache for witness generation.
fn apply_sandbox_state_journaled_inner<DB: AlloyDatabase>(
    journal: &mut Journal<DB>,
    sandbox_state: EvmState,
) -> Result<(), KeylessDeployError> {
    for (address, sandbox_account) in &sandbox_state {
        // Ensure the account is loaded into the parent journal cache without warming it.
        // Sandbox state merge must preserve the parent's own observable coldness-dependent gas
        // semantics. `load_code = false` — this path doesn't read `info.code`.
        journal.inspect_account(*address, false).map_err(|e| {
            error!(
                error = %e,
                address = ?address,
                "sandbox merge inspect_account failed",
            );
            KeylessDeployError::InternalError
        })?;

        if sandbox_account.is_selfdestructed() {
            apply_sandbox_created_selfdestruct(journal, *address, sandbox_account)?;
            continue;
        }

        // Snapshot parent state before applying diffs.
        let parent_balance = journal.inner.state.get(address).unwrap().info.balance;
        let parent_nonce = journal.inner.state.get(address).unwrap().info.nonce;
        let parent_code_hash = journal.inner.state.get(address).unwrap().info.code_hash;

        if sandbox_account.is_created() {
            ensure_sandbox_create_can_merge(parent_nonce, parent_code_hash)?;
            mark_account_created_for_sandbox_merge(journal, *address);
            zero_parent_storage_absent_from_sandbox(journal, *address, sandbox_account);
        }

        // Balance diff.
        if sandbox_account.info.balance != parent_balance {
            journal.inner.journal.push(JournalEntry::BalanceChange {
                address: *address,
                old_balance: parent_balance,
            });
            journal.inner.state.get_mut(address).unwrap().info.balance =
                sandbox_account.info.balance;
        }

        if sandbox_account.info.nonce < parent_nonce {
            error!(
                address = ?address,
                parent_nonce,
                sandbox_nonce = sandbox_account.info.nonce,
                "sandbox merge would decrease parent account nonce",
            );
            return Err(KeylessDeployError::InternalError);
        }

        // Nonce diff (one NonceChange entry per increment for correct revert).
        let nonce_diff = sandbox_account.info.nonce - parent_nonce;
        for _ in 0..nonce_diff {
            journal.inner.journal.push(JournalEntry::NonceChange { address: *address });
        }
        if nonce_diff > 0 {
            journal.inner.state.get_mut(address).unwrap().info.nonce = sandbox_account.info.nonce;
        }

        // Code diff.
        if sandbox_account.info.code_hash == parent_code_hash {
            // Treat empty cached bytecode as missing: when `code_hash != KECCAK_EMPTY` but the
            // parent's loaded `info.code` is empty, the cache is stale (e.g. an `EmptyDB` returned
            // a default Bytecode for the hash) and should be replaced by the sandbox's real load.
            let parent_code_missing = journal
                .inner
                .state
                .get(address)
                .unwrap()
                .info
                .code
                .as_ref()
                .is_none_or(|code| code.is_empty());
            if parent_code_hash != KECCAK_EMPTY && parent_code_missing {
                if let Some(code) = &sandbox_account.info.code {
                    // Preserve the old merge behavior for read-only code loads: bytecode cached by
                    // the sandbox is witness/cache metadata, not a stateful code change.
                    journal.inner.state.get_mut(address).unwrap().info.code = Some(code.clone());
                }
            }
        } else {
            if parent_code_hash != KECCAK_EMPTY {
                error!(
                    address = ?address,
                    "sandbox merge would replace non-empty parent code",
                );
                return Err(KeylessDeployError::InternalError);
            }
            let Some(code) = &sandbox_account.info.code else {
                error!(
                    address = ?address,
                    "sandbox account changed code hash without bytecode",
                );
                return Err(KeylessDeployError::InternalError);
            };
            journal.inner.set_code_with_hash(
                *address,
                code.clone(),
                sandbox_account.info.code_hash,
            );
        }

        // Touch — sandbox-modified accounts are genuinely touched in the parent tx.
        if sandbox_account.is_touched() {
            journal.inner.touch(*address);
        }

        // Storage merge mirrors `merge_evm_state_optional_status(..., false)`: existing
        // parent slots keep their original/cold metadata and only `present_value` changes;
        // slots absent from the parent are inserted as cold, including read-only slots.
        for (key, sandbox_slot) in &sandbox_account.storage {
            let parent_slot =
                journal.inner.state.get(address).and_then(|a| a.storage.get(key)).cloned();
            let had_value = parent_slot
                .as_ref()
                .map(|slot| slot.present_value)
                .unwrap_or_else(|| sandbox_slot.original_value());
            if sandbox_slot.present_value != had_value {
                journal.inner.journal.push(JournalEntry::StorageChanged {
                    key: *key,
                    had_value,
                    address: *address,
                });
            }

            if let Some(parent_slot) =
                journal.inner.state.get_mut(address).unwrap().storage.get_mut(key)
            {
                parent_slot.present_value = sandbox_slot.present_value;
            } else {
                let mut merged_slot = sandbox_slot.clone();
                merged_slot.mark_cold();
                journal.inner.state.get_mut(address).unwrap().storage.insert(*key, merged_slot);
            }
        }
    }

    Ok(())
}

/// Validates that a sandbox-created account can be merged as a new parent account.
///
/// Creating over an account with nonce or code would not match EVM `CREATE` semantics and is
/// treated as an internal merge invariant violation.
fn ensure_sandbox_create_can_merge(
    parent_nonce: u64,
    parent_code_hash: alloy_primitives::B256,
) -> Result<(), KeylessDeployError> {
    if parent_nonce != 0 || parent_code_hash != KECCAK_EMPTY {
        error!(
            parent_nonce,
            parent_code_hash = ?parent_code_hash,
            "sandbox merge would create over non-empty parent account",
        );
        return Err(KeylessDeployError::InternalError);
    }
    Ok(())
}

/// Marks an account as locally created and records the matching journal entry.
///
/// The `AccountCreated` entry is required so final commit clears stale database storage and
/// checkpoint revert restores the previous account status.
fn mark_account_created_for_sandbox_merge<DB: AlloyDatabase>(
    journal: &mut Journal<DB>,
    address: Address,
) {
    let is_created_globally = journal.inner.state.get_mut(&address).unwrap().mark_created_locally();
    journal.inner.journal.push(JournalEntry::AccountCreated { address, is_created_globally });
}

/// Clears loaded parent storage slots that are absent from a sandbox-created account.
///
/// A created account replaces any stale storage at the same address, so parent slots not present in
/// the sandbox result must be journaled to zero before commit.
fn zero_parent_storage_absent_from_sandbox<DB: AlloyDatabase>(
    journal: &mut Journal<DB>,
    address: Address,
    sandbox_account: &revm::state::Account,
) {
    let parent_keys =
        journal.inner.state.get(&address).unwrap().storage.keys().copied().collect::<Vec<_>>();

    for key in parent_keys {
        if sandbox_account.storage.contains_key(&key) {
            continue;
        }
        zero_parent_storage_slot(journal, address, key);
    }
}

/// Journals a loaded parent storage slot to zero when it still has a non-zero value.
fn zero_parent_storage_slot<DB: AlloyDatabase>(
    journal: &mut Journal<DB>,
    address: Address,
    key: U256,
) {
    let had_value = journal
        .inner
        .state
        .get(&address)
        .and_then(|account| account.storage.get(&key))
        .map(|slot| slot.present_value())
        .unwrap_or_default();
    if had_value.is_zero() {
        return;
    }
    journal.inner.journal.push(JournalEntry::StorageChanged { key, had_value, address });
    journal.inner.state.get_mut(&address).unwrap().storage.get_mut(&key).unwrap().present_value =
        U256::ZERO;
}

/// Applies the merge result for an account created and selfdestructed inside the sandbox.
///
/// The parent commit must observe an empty created account: existing balance and loaded storage are
/// journaled to zero while the created marker is preserved.
fn apply_sandbox_created_selfdestruct<DB: AlloyDatabase>(
    journal: &mut Journal<DB>,
    address: Address,
    sandbox_account: &revm::state::Account,
) -> Result<(), KeylessDeployError> {
    if !sandbox_account.is_created() {
        error!(
            address = ?address,
            "sandbox selfdestructed account was not created in the sandbox",
        );
        return Err(KeylessDeployError::InternalError);
    }

    let parent = journal.inner.state.get(&address).unwrap();
    ensure_sandbox_create_can_merge(parent.info.nonce, parent.info.code_hash)?;

    mark_account_created_for_sandbox_merge(journal, address);
    journal.inner.touch(address);

    let old_balance = journal.inner.state.get(&address).unwrap().info.balance;
    if !old_balance.is_zero() {
        journal.inner.journal.push(JournalEntry::BalanceChange { address, old_balance });
        journal.inner.state.get_mut(&address).unwrap().info.balance = U256::ZERO;
    }

    let parent_keys =
        journal.inner.state.get(&address).unwrap().storage.keys().copied().collect::<Vec<_>>();
    for key in parent_keys {
        zero_parent_storage_slot(journal, address, key);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Bytes};
    use revm::{
        context_interface::journaled_state::JournalTr,
        database::EmptyDB,
        state::{Account, AccountInfo, EvmStorageSlot},
        Database, DatabaseCommit,
    };

    use crate::{test_utils::MemoryDatabase, EmptyExternalEnv};

    fn sandbox_created_account(code: Bytes) -> Account {
        let code = revm::bytecode::Bytecode::new_raw(code);
        let code_hash = revm::primitives::keccak256(code.bytes_slice());
        let mut account = Account::from(AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash,
            code: Some(code),
        });
        account.mark_touch();
        account.mark_created();
        account
    }

    #[test]
    fn test_rex4_apply_sandbox_state_uses_legacy_direct_merge() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let deploy_addr = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");
        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(EmptyDB::default(), MegaSpecId::REX4);
        let checkpoint = JournalTr::checkpoint(ctx.journal_mut());

        let mut sandbox_state = EvmState::default();
        sandbox_state.insert(deploy_addr, {
            let mut account = sandbox_created_account(Bytes::from_static(&[0x60, 0x00]));
            account
                .storage
                .insert(U256::from(0), EvmStorageSlot::new_changed(U256::ZERO, U256::from(42), 0));
            account
        });

        apply_sandbox_state(&mut ctx, sandbox_state, signer).expect("apply should succeed");
        let journal = ctx.journal_mut();
        assert!(
            journal.inner.state.get(&deploy_addr).unwrap().is_created(),
            "legacy merge should preserve created status for a newly inserted account"
        );

        JournalTr::checkpoint_revert(journal, checkpoint);
        assert!(
            journal.inner.state.contains_key(&deploy_addr),
            "pre-Rex5 keeps the legacy direct-merge semantics instead of journal rollback"
        );
    }

    /// Proves that `apply_sandbox_state` records journal entries for stateful diffs:
    /// after a checkpoint-revert the parent values are restored, while revm may keep
    /// cold cache/read-set entries that are not committed as state changes.
    #[test]
    fn test_rex5_apply_sandbox_state_revert_restores_parent() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let deploy_addr = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");
        let getter_addr = address!("cccccccccccccccccccccccccccccccccccc0003");
        let original_balance = U256::from(1_000_000u64);
        let read_only_slot = U256::from(7);
        let read_only_value = U256::from(123);

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(EmptyDB::default(), MegaSpecId::REX5);
        let journal = ctx.journal_mut();

        // Seed signer in parent state with known balance and nonce.
        journal.inner.state.insert(signer, {
            let mut acc = Account::from(AccountInfo {
                balance: original_balance,
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: None,
            });
            acc.mark_touch();
            acc
        });
        let getter_code = revm::bytecode::Bytecode::new_raw(Bytes::from_static(&[0x60, 0x01]));
        let getter_code_hash = revm::primitives::keccak256(getter_code.bytes_slice());
        journal.inner.state.insert(getter_addr, {
            let mut acc = Account::from(AccountInfo {
                balance: U256::ZERO,
                nonce: 1,
                code_hash: getter_code_hash,
                code: None,
            });
            acc.mark_cold();
            acc
        });

        // Take a checkpoint before applying sandbox state.
        let checkpoint = JournalTr::checkpoint(journal);

        // Build sandbox state: signer paid gas (lower balance, nonce bumped),
        // deploy_addr is a new account with code and a storage slot.
        let mut sandbox_state = EvmState::default();

        // Signer: balance decreased, nonce = 1.
        sandbox_state.insert(signer, {
            let mut acc = Account::from(AccountInfo {
                balance: U256::from(500_000u64),
                nonce: 1,
                code_hash: KECCAK_EMPTY,
                code: None,
            });
            acc.mark_touch();
            acc
        });

        // Deploy address: new account with code and storage.
        sandbox_state.insert(deploy_addr, {
            let mut acc = sandbox_created_account(Bytes::from_static(&[0x60, 0x00]));
            acc.storage
                .insert(U256::from(0), EvmStorageSlot::new_changed(U256::ZERO, U256::from(42), 0));
            acc
        });
        sandbox_state.insert(getter_addr, {
            let mut acc = Account::from(AccountInfo {
                balance: U256::ZERO,
                nonce: 1,
                code_hash: getter_code_hash,
                code: Some(getter_code),
            });
            acc.storage.insert(read_only_slot, EvmStorageSlot::new(read_only_value, 0));
            acc
        });

        // Apply sandbox state — should record journal entries.
        apply_sandbox_state(&mut ctx, sandbox_state, signer).expect("apply should succeed");

        // Verify merge applied.
        let journal = ctx.journal_mut();
        assert_eq!(
            journal.inner.state.get(&signer).unwrap().info.balance,
            U256::from(500_000u64),
            "signer balance should reflect sandbox deduction"
        );
        assert_eq!(
            journal.inner.state.get(&signer).unwrap().info.nonce,
            1,
            "signer nonce should be bumped"
        );
        assert!(
            journal.inner.state.contains_key(&deploy_addr),
            "deploy address should exist after merge"
        );
        assert!(
            journal.inner.state.get(&deploy_addr).unwrap().is_created(),
            "created account status must be preserved for final commit"
        );
        assert!(
            journal.inner.state.get(&signer).unwrap().is_touched(),
            "stateful signer diff should leave signer touched"
        );
        assert!(
            journal.inner.state.get(&deploy_addr).unwrap().is_touched(),
            "stateful deploy diff should leave deployed account touched"
        );
        assert!(
            journal
                .inner
                .state
                .get(&getter_addr)
                .unwrap()
                .info
                .code
                .as_ref()
                .is_some_and(|code| !code.is_empty()),
            "read-only bytecode loaded by sandbox should be cached for witnesses"
        );
        assert_eq!(
            journal
                .inner
                .state
                .get(&getter_addr)
                .unwrap()
                .storage
                .get(&read_only_slot)
                .map(|slot| slot.present_value()),
            Some(read_only_value),
            "read-only storage loaded by sandbox should be cached for witnesses"
        );

        // Revert to checkpoint — should undo all sandbox state changes.
        JournalTr::checkpoint_revert(journal, checkpoint);

        // Verify parent state is restored.
        assert_eq!(
            journal.inner.state.get(&signer).unwrap().info.balance,
            original_balance,
            "signer balance should be restored after revert"
        );
        assert_eq!(
            journal.inner.state.get(&signer).unwrap().info.nonce,
            0,
            "signer nonce should be restored after revert"
        );
        // deploy_addr was loaded from EmptyDB by apply_sandbox_state, so it can remain in
        // the cache (AccountWarmed revert marks it cold, but does not remove it). Its
        // stateful values must still be reverted.
        if let Some(deploy_account) = journal.inner.state.get(&deploy_addr) {
            assert_eq!(
                deploy_account.info.code_hash, KECCAK_EMPTY,
                "deployed code should be reverted"
            );
            assert_eq!(deploy_account.info.nonce, 0, "deploy address nonce should be reverted");
            assert_eq!(
                deploy_account.storage.get(&U256::from(0)).map(|slot| slot.present_value()),
                Some(U256::ZERO),
                "deployed storage should be reverted to the pre-sandbox value"
            );
        }
        assert!(
            journal
                .inner
                .state
                .get(&getter_addr)
                .unwrap()
                .info
                .code
                .as_ref()
                .is_some_and(|code| !code.is_empty()),
            "read-only bytecode cache may remain after revert"
        );
        assert_eq!(
            journal
                .inner
                .state
                .get(&getter_addr)
                .unwrap()
                .storage
                .get(&read_only_slot)
                .map(|slot| slot.present_value()),
            Some(read_only_value),
            "read-only storage cache may remain after revert without becoming a stateful diff"
        );
    }

    #[test]
    fn test_rex5_apply_sandbox_state_error_reverts_partial_merge() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let target = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");
        let original_balance = U256::from(1_000_000u64);

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(EmptyDB::default(), MegaSpecId::REX5);
        let journal = ctx.journal_mut();
        journal.inner.state.insert(target, {
            Account::from(AccountInfo {
                balance: original_balance,
                nonce: 1,
                code_hash: KECCAK_EMPTY,
                code: None,
            })
        });
        let journal_len_before = journal.inner.journal.len();
        let depth_before = journal.inner.depth;

        let mut sandbox_state = EvmState::default();
        sandbox_state.insert(target, {
            let mut account = Account::from(AccountInfo {
                balance: U256::from(500_000u64),
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: None,
            });
            account.mark_touch();
            account
        });

        let error = apply_sandbox_state(&mut ctx, sandbox_state, signer)
            .expect_err("nonce decrease should fail defensively");
        assert!(matches!(error, KeylessDeployError::InternalError), "unexpected error: {error:?}");

        let journal = ctx.journal_mut();
        assert_eq!(
            journal.inner.state.get(&target).unwrap().info.balance,
            original_balance,
            "partial balance diff must be reverted on merge error"
        );
        assert_eq!(
            journal.inner.state.get(&target).unwrap().info.nonce,
            1,
            "parent nonce must be restored on merge error"
        );
        assert_eq!(
            journal.inner.journal.len(),
            journal_len_before,
            "partial journal entries must be removed on merge error"
        );
        assert_eq!(journal.inner.depth, depth_before, "checkpoint depth must be balanced");
    }

    #[test]
    fn test_rex5_apply_sandbox_state_created_commit_clears_stale_storage() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let deploy_addr = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");
        let stale_slot = U256::from(9);
        let deployed_slot = U256::from(0);
        let mut db = MemoryDatabase::default();
        db.set_account_storage(deploy_addr, stale_slot, U256::from(99));

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(db, MegaSpecId::REX5);
        let mut sandbox_state = EvmState::default();
        sandbox_state.insert(deploy_addr, {
            let mut account = sandbox_created_account(Bytes::from_static(&[0x60, 0x00]));
            account
                .storage
                .insert(deployed_slot, EvmStorageSlot::new_changed(U256::ZERO, U256::from(42), 0));
            account
        });

        apply_sandbox_state(&mut ctx, sandbox_state, signer).expect("apply should succeed");
        let state = JournalTr::finalize(ctx.journal_mut());
        ctx.db_mut().commit(state);

        assert_eq!(
            ctx.db_mut().storage(deploy_addr, deployed_slot).unwrap(),
            U256::from(42),
            "deployed storage should be committed"
        );
        assert_eq!(
            ctx.db_mut().storage(deploy_addr, stale_slot).unwrap(),
            U256::ZERO,
            "created-account commit must clear stale database storage"
        );
    }

    #[test]
    fn test_rex5_apply_sandbox_state_created_selfdestruct_revert_restores_parent() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let deploy_addr = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");
        let original_balance = U256::from(7);
        let storage_slot = U256::from(3);
        let original_storage = U256::from(11);

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(EmptyDB::default(), MegaSpecId::REX5);
        let journal = ctx.journal_mut();
        journal.inner.state.insert(deploy_addr, {
            let mut account = Account::from(AccountInfo {
                balance: original_balance,
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: None,
            });
            account.storage.insert(storage_slot, EvmStorageSlot::new(original_storage, 0));
            account
        });
        let checkpoint = JournalTr::checkpoint(journal);

        let mut sandbox_state = EvmState::default();
        sandbox_state.insert(deploy_addr, {
            let mut account = Account::from(AccountInfo::default());
            account.mark_touch();
            account.mark_created();
            account.mark_selfdestruct();
            account
        });

        apply_sandbox_state(&mut ctx, sandbox_state, signer).expect("apply should succeed");
        let journal = ctx.journal_mut();
        assert_eq!(
            journal.inner.state.get(&deploy_addr).unwrap().info.balance,
            U256::ZERO,
            "created+selfdestructed account should be empty after merge"
        );
        assert_eq!(
            journal
                .inner
                .state
                .get(&deploy_addr)
                .unwrap()
                .storage
                .get(&storage_slot)
                .unwrap()
                .present_value(),
            U256::ZERO,
            "created+selfdestructed account should clear loaded parent storage"
        );

        JournalTr::checkpoint_revert(journal, checkpoint);

        let restored = journal.inner.state.get(&deploy_addr).unwrap();
        assert_eq!(
            restored.info.balance, original_balance,
            "balance should be restored after revert"
        );
        assert_eq!(
            restored.storage.get(&storage_slot).unwrap().present_value(),
            original_storage,
            "storage should be restored after revert"
        );
        assert!(!restored.is_created(), "created marker should be reverted with the checkpoint");
    }

    /// Parent has non-empty code and the sandbox attempts to install a different code
    /// hash — defensive merge MUST reject. Pinned because silently overwriting code on a
    /// live account would let a keyless deploy hijack any address whose code happened to
    /// be loaded into the cache during the parent's prior steps.
    #[test]
    fn test_rex5_apply_sandbox_state_rejects_non_empty_parent_code_replacement() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let target = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");

        // Parent: an existing contract with non-empty code.
        let parent_code = revm::bytecode::Bytecode::new_raw(Bytes::from_static(&[0x60, 0x01]));
        let parent_code_hash = revm::primitives::keccak256(parent_code.bytes_slice());

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(EmptyDB::default(), MegaSpecId::REX5);
        ctx.journal_mut().inner.state.insert(target, {
            Account::from(AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: parent_code_hash,
                code: Some(parent_code),
            })
        });

        // Sandbox: tries to replace with a different code hash.
        let sandbox_code = revm::bytecode::Bytecode::new_raw(Bytes::from_static(&[0x60, 0x02]));
        let sandbox_code_hash = revm::primitives::keccak256(sandbox_code.bytes_slice());
        let mut sandbox_state = EvmState::default();
        sandbox_state.insert(target, {
            let mut acc = Account::from(AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: sandbox_code_hash,
                code: Some(sandbox_code),
            });
            acc.mark_touch();
            acc
        });

        let error = apply_sandbox_state(&mut ctx, sandbox_state, signer)
            .expect_err("merge must reject code replacement");
        assert!(matches!(error, KeylessDeployError::InternalError), "unexpected error: {error:?}");
    }

    /// Sandbox account reports a non-empty `code_hash` but no `info.code` payload.
    /// Defensive merge MUST reject because the inner `set_code_with_hash` call would
    /// otherwise panic / silently install no bytecode.
    #[test]
    fn test_rex5_apply_sandbox_state_rejects_code_hash_without_bytecode() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let target = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(EmptyDB::default(), MegaSpecId::REX5);
        // Parent: empty (KECCAK_EMPTY) so the "different code_hash, parent is empty" branch fires.

        let mut sandbox_state = EvmState::default();
        sandbox_state.insert(target, {
            let mut acc = Account::from(AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                // Non-empty code_hash but `code: None` — exactly the invariant violation.
                code_hash: revm::primitives::keccak256([0x60u8, 0x00]),
                code: None,
            });
            acc.mark_touch();
            acc
        });

        let error = apply_sandbox_state(&mut ctx, sandbox_state, signer)
            .expect_err("merge must reject mismatched code_hash without bytecode");
        assert!(matches!(error, KeylessDeployError::InternalError), "unexpected error: {error:?}");
    }

    /// `ensure_sandbox_create_can_merge`: sandbox is marked `is_created()` but the
    /// parent already has a non-zero nonce (not an empty slot). Defensive merge MUST
    /// reject — committing a CREATE over an existing account would silently overwrite
    /// its on-chain state.
    #[test]
    fn test_rex5_apply_sandbox_state_rejects_create_over_non_empty_parent_nonce() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let deploy_addr = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(EmptyDB::default(), MegaSpecId::REX5);
        ctx.journal_mut().inner.state.insert(deploy_addr, {
            Account::from(AccountInfo {
                balance: U256::ZERO,
                // Non-zero nonce triggers the rejection.
                nonce: 5,
                code_hash: KECCAK_EMPTY,
                code: None,
            })
        });

        let mut sandbox_state = EvmState::default();
        sandbox_state
            .insert(deploy_addr, sandbox_created_account(Bytes::from_static(&[0x60, 0x00])));

        let error = apply_sandbox_state(&mut ctx, sandbox_state, signer)
            .expect_err("merge must reject CREATE over non-empty parent");
        assert!(matches!(error, KeylessDeployError::InternalError), "unexpected error: {error:?}");
    }

    /// `inspect_account` may surface a `DBError` (the DB call fails mid-merge). The
    /// merge MUST translate that into the selector-only `InternalError` and revert any
    /// partial journal entries via the inner checkpoint. Without this guard a DB blip
    /// could leave a partially merged sandbox state in the parent journal.
    #[test]
    fn test_rex5_apply_sandbox_state_db_error_during_inspect_maps_to_internal_error() {
        use crate::test_utils::ErrorInjectingDatabase;

        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let target = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");

        let mut db = ErrorInjectingDatabase::new(MemoryDatabase::default());
        // The merge's first DB-touching step is the `inspect_account` call for each
        // sandbox-modified address. Fail that lookup for `target`.
        db.fail_on_account = Some(target);

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(db, MegaSpecId::REX5);

        let mut sandbox_state = EvmState::default();
        sandbox_state.insert(target, {
            let mut acc = Account::from(AccountInfo::default());
            acc.mark_touch();
            acc
        });

        let error = apply_sandbox_state(&mut ctx, sandbox_state, signer)
            .expect_err("inspect_account DB error must surface as InternalError");
        assert!(matches!(error, KeylessDeployError::InternalError), "unexpected error: {error:?}");
    }

    /// `apply_sandbox_created_selfdestruct` guard: sandbox account is selfdestructed
    /// but NOT marked `is_created()`. That's not a same-tx CREATE+SELFDESTRUCT — it's
    /// the sandbox trying to wipe an account that existed before the keyless tx.
    /// Defensive merge MUST reject.
    #[test]
    fn test_rex5_apply_sandbox_state_rejects_selfdestruct_without_created_marker() {
        let signer = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0001");
        let target = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb0002");

        let mut ctx = MegaContext::<_, EmptyExternalEnv>::new(EmptyDB::default(), MegaSpecId::REX5);
        ctx.journal_mut().inner.state.insert(target, {
            Account::from(AccountInfo {
                balance: U256::from(1u64),
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: None,
            })
        });

        let mut sandbox_state = EvmState::default();
        sandbox_state.insert(target, {
            let mut acc = Account::from(AccountInfo::default());
            acc.mark_touch();
            // No `mark_created()`: this is the invariant violation.
            acc.mark_selfdestruct();
            acc
        });

        let error = apply_sandbox_state(&mut ctx, sandbox_state, signer)
            .expect_err("merge must reject selfdestruct without created marker");
        assert!(matches!(error, KeylessDeployError::InternalError), "unexpected error: {error:?}");
    }
}
