//! The `SequencerRegistry` system contract for the `MegaETH` EVM.
//!
//! Tracks two independent roles: system address (Oracle/system-tx authority) and
//! sequencer (mini-block signing). Each role has its own change lifecycle.
//!
//! Unlike intercepted system contracts (`LimitControl`, `AccessControl`), this contract runs
//! normal on-chain bytecode. It does not have an interceptor.
//!
//! Deployed via raw state patch with initial storage seeded at deploy time.
//! Due changes are applied via a single pre-block EVM system call to
//! `applyPendingChanges()`, following the same pattern as EIP-2935 and EIP-4788.
//! The current system transaction sender should always be read through
//! [`resolve_system_address`], which returns the legacy constant for pre-REX5 and
//! reads the on-chain `SequencerRegistry` state for REX5+.

use alloy_evm::{
    block::{BlockExecutionError, BlockValidationError},
    Database,
};
use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::SolCall;
use mega_system_contracts::sequencer_registry::storage_slots::{
    ADMIN, CURRENT_SEQUENCER, CURRENT_SYSTEM_ADDRESS, INITIAL_FROM_BLOCK, INITIAL_SEQUENCER,
    INITIAL_SYSTEM_ADDRESS, PENDING_SEQUENCER, PENDING_SYSTEM_ADDRESS, SEQUENCER_ACTIVATION_BLOCK,
    SYSTEM_ADDRESS_ACTIVATION_BLOCK,
};
use revm::{
    context_interface::result::ResultAndState,
    database::State,
    primitives::KECCAK_EMPTY,
    state::{Account, Bytecode, EvmState, EvmStorageSlot},
    Database as RevmDatabase,
};

use crate::{HardforkParams, MegaHardfork, MegaHardforks, MEGA_SYSTEM_ADDRESS};

/// The address of the `SequencerRegistry` system contract.
pub const SEQUENCER_REGISTRY_ADDRESS: Address =
    address!("0x6342000000000000000000000000000000000006");

/// The code of the `SequencerRegistry` contract (version 1.0.0).
pub use mega_system_contracts::sequencer_registry::V1_0_0_CODE as SEQUENCER_REGISTRY_CODE;

/// The code hash of the `SequencerRegistry` contract (version 1.0.0).
pub use mega_system_contracts::sequencer_registry::V1_0_0_CODE_HASH as SEQUENCER_REGISTRY_CODE_HASH;

pub use mega_system_contracts::sequencer_registry::ISequencerRegistry;

/// Bootstrap configuration for `SequencerRegistry` (attached to Rex5 via [`HardforkParams`]).
///
/// All three addresses are required. There is no `Default`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencerRegistryConfig {
    /// The initial system address (Oracle/system-tx sender).
    pub initial_system_address: Address,
    /// The initial sequencer (mini-block signing key).
    pub initial_sequencer: Address,
    /// The initial admin for the registry.
    pub initial_admin: Address,
}

impl HardforkParams for SequencerRegistryConfig {
    const FORK: MegaHardfork = MegaHardfork::Rex5;
}

/// Encodes an address into its `U256` storage representation (standard Solidity address-in-slot).
fn address_to_storage_value(address: Address) -> U256 {
    U256::from_be_bytes(address.into_word().0)
}

/// Reads a committed `SequencerRegistry` storage slot.
fn read_registry_storage<DB: Database>(
    db: &mut State<DB>,
    slot: U256,
) -> Result<U256, BlockExecutionError> {
    RevmDatabase::storage(db, SEQUENCER_REGISTRY_ADDRESS, slot).map_err(BlockExecutionError::other)
}

/// Returns whether a pending role change is due and records every storage read into the witness
/// account.
fn is_role_due<DB: Database>(
    db: &mut State<DB>,
    account: &mut Account,
    pending_slot: U256,
    activation_slot: U256,
    block_number: u64,
) -> Result<bool, BlockExecutionError> {
    let pending = read_registry_storage(db, pending_slot)?;
    // Read-only witness entry: record the slot access without marking it as changed.
    account.storage.insert(pending_slot, EvmStorageSlot::new(pending, 0));
    if pending.is_zero() {
        return Ok(false);
    }

    let activation_block = read_registry_storage(db, activation_slot)?;
    account.storage.insert(activation_slot, EvmStorageSlot::new(activation_block, 0));

    Ok(block_number >= activation_block.saturating_to::<u64>())
}

/// Deploys the `SequencerRegistry` contract and seeds initial storage.
///
/// On first deploy, writes 6 flat storage slots:
/// - `_currentSystemAddress`, `_currentSequencer`, `_admin`
/// - `_initialSystemAddress`, `_initialSequencer`, `_initialFromBlock`
///
/// The dynamic change-history arrays remain empty on bootstrap and grow only when
/// `applyPendingChanges()` commits a due change.
/// If already deployed with the correct code hash, returns the account as-is.
pub fn transact_deploy_sequencer_registry<DB: Database>(
    hardforks: impl MegaHardforks,
    block_timestamp: u64,
    current_block_number: u64,
    db: &mut State<DB>,
    config: &SequencerRegistryConfig,
) -> Result<Option<EvmState>, BlockExecutionError> {
    if !hardforks.is_rex_5_active_at_timestamp(block_timestamp) {
        return Ok(None);
    }

    let acc =
        db.load_cache_account(SEQUENCER_REGISTRY_ADDRESS).map_err(BlockExecutionError::other)?;

    if let Some(account_info) = acc.account_info() {
        if account_info.code_hash == SEQUENCER_REGISTRY_CODE_HASH {
            // Already deployed with correct code — no action needed.
            return Ok(Some(EvmState::from_iter([(
                SEQUENCER_REGISTRY_ADDRESS,
                Account { info: account_info, ..Default::default() },
            )])));
        }
        if account_info.code_hash != KECCAK_EMPTY {
            // Account has actual contract code that doesn't match ours.
            // Silently overwriting would destroy change history and pending state.
            return Err(BlockValidationError::BlockHashContractCall {
                message: format!(
                    "SequencerRegistry at {} has unexpected code hash {} (expected {}); \
                     refusing to overwrite stateful contract storage without migration",
                    SEQUENCER_REGISTRY_ADDRESS,
                    account_info.code_hash,
                    SEQUENCER_REGISTRY_CODE_HASH,
                ),
            }
            .into());
        }
        // EOA with balance (KECCAK_EMPTY code hash) — safe to deploy on top.
    }

    // First deploy (or EOA-only account).
    let mut acc_info = acc.account_info().unwrap_or_default();
    acc_info.code_hash = SEQUENCER_REGISTRY_CODE_HASH;
    acc_info.code = Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE));

    let mut revm_acc: Account = acc_info.into();
    revm_acc.mark_touch();
    revm_acc.mark_created();

    // Seed initial storage (flat slots only, no dynamic arrays).
    let initial_system_address = address_to_storage_value(config.initial_system_address);
    let initial_sequencer = address_to_storage_value(config.initial_sequencer);
    let initial_admin = address_to_storage_value(config.initial_admin);
    let initial_from_block = U256::from(current_block_number);

    for (slot, value) in [
        (CURRENT_SYSTEM_ADDRESS, initial_system_address),
        (CURRENT_SEQUENCER, initial_sequencer),
        (ADMIN, initial_admin),
        (INITIAL_SYSTEM_ADDRESS, initial_system_address),
        (INITIAL_SEQUENCER, initial_sequencer),
        (INITIAL_FROM_BLOCK, initial_from_block),
    ] {
        revm_acc
            .storage
            .insert(slot, revm::state::EvmStorageSlot::new_changed(U256::ZERO, value, 0));
    }

    Ok(Some(EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, revm_acc)])))
}

/// Returns `(due, witness_state)` where `due` indicates whether the caller should issue
/// the pre-block `applyPendingChanges()` system call.
///
/// The returned `EvmState` captures all reads (account + storage slots) as a witness record.
/// The executor MUST push this into outcomes regardless of `due` so that the reads enter
/// the stateless witness via `system_caller.on_state()`.
pub(crate) fn is_apply_pending_changes_due<DB: Database>(
    db: &mut State<DB>,
    block_number: u64,
) -> Result<(bool, EvmState), BlockExecutionError> {
    let acc =
        db.load_cache_account(SEQUENCER_REGISTRY_ADDRESS).map_err(BlockExecutionError::other)?;

    let Some(info) = acc.account_info() else {
        // Account does not exist — record a not-existing account entry for the witness.
        let account = Account::new_not_existing(0);
        let state = EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, account)]);
        return Ok((false, state));
    };

    // Account exists — build a read-only account entry to record all slot reads.
    let mut account = Account { info, ..Default::default() };

    let system_address_due = is_role_due(
        db,
        &mut account,
        PENDING_SYSTEM_ADDRESS,
        SYSTEM_ADDRESS_ACTIVATION_BLOCK,
        block_number,
    )?;
    let sequencer_due =
        is_role_due(db, &mut account, PENDING_SEQUENCER, SEQUENCER_ACTIVATION_BLOCK, block_number)?;

    let state = EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, account)]);
    Ok((system_address_due || sequencer_due, state))
}

/// Executes the pre-block `applyPendingChanges()` system call on the `SequencerRegistry`.
///
/// This single system call applies both the system address change and the sequencer
/// change if they are due in the current block.
/// Caller should gate this with [`is_apply_pending_changes_due`] to avoid an EVM
/// call on every block.
///
/// The system call is issued with `max(block.gas_limit, SYSTEM_CALL_GAS_LIMIT_FLOOR)`
/// instead of the upstream-fixed 30M. `applyPendingChanges()` writes role-rotation
/// slots whose actual cost depends on REX dynamic storage gas, so the upstream default
/// is no longer guaranteed to be enough on activation blocks.
pub(crate) fn transact_apply_pending_changes<DB, INSP, ExtEnvs>(
    evm: &mut crate::MegaEvm<DB, INSP, ExtEnvs>,
) -> Result<ResultAndState<crate::MegaHaltReason>, BlockExecutionError>
where
    DB: alloy_evm::Database,
    ExtEnvs: crate::ExternalEnvTypes,
{
    let calldata = ISequencerRegistry::applyPendingChangesCall {}.abi_encode();
    let gas_limit =
        evm.block_env_ref().gas_limit.max(crate::constants::rex5::SYSTEM_CALL_GAS_LIMIT_FLOOR);
    let result_and_state = match evm.transact_system_call_with_gas_limit(
        alloy_eips::eip4788::SYSTEM_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
        Bytes::from(calldata),
        gas_limit,
    ) {
        Ok(res) => res,
        Err(e) => {
            return Err(BlockValidationError::BlockHashContractCall {
                message: format!("SequencerRegistry applyPendingChanges() system call failed: {e}"),
            }
            .into());
        }
    };

    if !result_and_state.result.is_success() {
        return Err(BlockValidationError::BlockHashContractCall {
            message: "SequencerRegistry applyPendingChanges() reverted or halted".into(),
        }
        .into());
    }

    Ok(result_and_state)
}

/// Resolves the current system address.
///
/// Must be called after `commit_system_call_outcomes()` so that `SequencerRegistry` is
/// already deployed and its storage committed.
///
/// - Pre-REX5: returns `(MEGA_SYSTEM_ADDRESS, None)`.
/// - REX5: reads `_currentSystemAddress` from committed registry storage.
///
/// The optional `EvmState` captures account + slot reads as a witness record.
/// The executor MUST commit this via `system_caller.on_state()` + `db.commit()`.
pub fn resolve_system_address<DB: Database>(
    hardforks: impl MegaHardforks,
    spec: crate::MegaSpecId,
    db: &mut State<DB>,
) -> Result<(Address, Option<EvmState>), BlockExecutionError> {
    if !spec.is_enabled(crate::MegaSpecId::REX5) {
        return Ok((MEGA_SYSTEM_ADDRESS, None));
    }

    // Fail fast if Rex5 is active but params are not configured.
    hardforks.fork_params::<SequencerRegistryConfig>().ok_or_else(|| {
        BlockValidationError::BlockHashContractCall {
            message: "Rex5 active but SequencerRegistryConfig not configured".into(),
        }
    })?;

    let acc =
        db.load_cache_account(SEQUENCER_REGISTRY_ADDRESS).map_err(BlockExecutionError::other)?;

    // Unreachable: deploy always runs and commits before resolve.
    let Some(info) = acc.account_info() else {
        return Err(BlockValidationError::BlockHashContractCall {
            message: "Rex5 active but SequencerRegistry account does not exist".into(),
        }
        .into());
    };

    // Unreachable: deploy verifies the code hash before seeding storage.
    if info.code_hash != SEQUENCER_REGISTRY_CODE_HASH {
        return Err(BlockValidationError::BlockHashContractCall {
            message: format!(
                "SequencerRegistry code hash mismatch: expected {}, got {}",
                SEQUENCER_REGISTRY_CODE_HASH, info.code_hash
            ),
        }
        .into());
    }

    // Build read-only witness: account entry + slot read.
    let mut account = Account { info, ..Default::default() };
    let value = read_registry_storage(db, CURRENT_SYSTEM_ADDRESS)?;
    // Read-only witness entry: record the slot access without marking it as changed.
    account.storage.insert(CURRENT_SYSTEM_ADDRESS, EvmStorageSlot::new(value, 0));

    // Unreachable: deploy seeds a non-zero initial system address.
    if value.is_zero() {
        return Err(BlockValidationError::BlockHashContractCall {
            message: "SequencerRegistry deployed but _currentSystemAddress is zero".into(),
        }
        .into());
    }

    let state = EvmState::from_iter([(SEQUENCER_REGISTRY_ADDRESS, account)]);

    Ok((Address::from_word(value.into()), Some(state)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256, B256};
    use revm::{context::BlockEnv, database::InMemoryDB, state::AccountInfo};

    use crate::{MegaHardforkConfig, MegaSpecId};

    const TEST_SYSTEM_ADDRESS: Address = address!("0xA887dCB9D5f39Ef79272801d05Abdf707CFBbD1d");
    const TEST_SEQUENCER: Address = address!("0xBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB");
    const TEST_ADMIN: Address = address!("0xCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC");

    fn test_config() -> SequencerRegistryConfig {
        SequencerRegistryConfig {
            initial_system_address: TEST_SYSTEM_ADDRESS,
            initial_sequencer: TEST_SEQUENCER,
            initial_admin: TEST_ADMIN,
        }
    }

    fn rex5_hardforks() -> MegaHardforkConfig {
        MegaHardforkConfig::default().with_all_activated().with_params(test_config())
    }

    #[test]
    fn test_code_hash_matches() {
        let computed_hash = keccak256(&SEQUENCER_REGISTRY_CODE);
        assert_eq!(computed_hash, SEQUENCER_REGISTRY_CODE_HASH);
    }

    /// Verifies that Rust slot constants match the Solidity storage layout.
    /// These values come from `forge inspect SequencerRegistry storage-layout`.
    /// If the Solidity field order changes, this test MUST be updated.
    #[test]
    fn test_slot_constants_match_solidity_layout() {
        assert_eq!(CURRENT_SYSTEM_ADDRESS, U256::from(0), "_currentSystemAddress = slot 0");
        assert_eq!(CURRENT_SEQUENCER, U256::from(1), "_currentSequencer = slot 1");
        assert_eq!(ADMIN, U256::from(2), "_admin = slot 2");
        assert_eq!(INITIAL_SYSTEM_ADDRESS, U256::from(3), "_initialSystemAddress = slot 3");
        assert_eq!(INITIAL_SEQUENCER, U256::from(4), "_initialSequencer = slot 4");
        assert_eq!(INITIAL_FROM_BLOCK, U256::from(5), "_initialFromBlock = slot 5");
        assert_eq!(PENDING_SYSTEM_ADDRESS, U256::from(6), "_pendingSystemAddress = slot 6");
        assert_eq!(
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(7),
            "_systemAddressActivationBlock = slot 7"
        );
        assert_eq!(PENDING_SEQUENCER, U256::from(8), "_pendingSequencer = slot 8");
        assert_eq!(SEQUENCER_ACTIVATION_BLOCK, U256::from(9), "_sequencerActivationBlock = slot 9");
    }

    #[test]
    fn test_deploy_requires_rex5() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let result = transact_deploy_sequencer_registry(
            MegaHardforkConfig::default(),
            0,
            1000,
            &mut state,
            &test_config(),
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_deploy_seeds_storage() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result =
            transact_deploy_sequencer_registry(&hardforks, 0, 1000, &mut state, &test_config())
                .unwrap()
                .unwrap();

        let account = result.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert!(account.is_created());
        assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
        assert_eq!(account.storage.len(), 6, "should write exactly 6 flat slots");

        assert_eq!(
            account.storage.get(&CURRENT_SYSTEM_ADDRESS).unwrap().present_value(),
            U256::from_be_bytes(TEST_SYSTEM_ADDRESS.into_word().0),
        );
        assert_eq!(
            account.storage.get(&CURRENT_SEQUENCER).unwrap().present_value(),
            U256::from_be_bytes(TEST_SEQUENCER.into_word().0),
        );
        assert_eq!(
            account.storage.get(&ADMIN).unwrap().present_value(),
            U256::from_be_bytes(TEST_ADMIN.into_word().0),
        );
        assert_eq!(
            account.storage.get(&INITIAL_FROM_BLOCK).unwrap().present_value(),
            U256::from(1000),
        );
    }

    #[test]
    fn test_deploy_is_idempotent() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result =
            transact_deploy_sequencer_registry(&hardforks, 0, 2000, &mut state, &test_config())
                .unwrap()
                .unwrap();

        let account = result.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert!(!account.is_created(), "idempotent deploy should not re-create");
        assert!(account.storage.is_empty(), "idempotent deploy should not write storage");
    }

    #[test]
    fn test_deploy_wrong_existing_code_hash_returns_error() {
        let wrong_code = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]));
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: wrong_code.hash_slow(),
                code: Some(wrong_code),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let err =
            transact_deploy_sequencer_registry(&hardforks, 0, 2000, &mut state, &test_config())
                .expect_err("wrong code hash must fail closed");

        let msg = err.to_string();
        assert!(msg.contains("unexpected code hash"), "unexpected message: {msg}");
        assert!(msg.contains("refusing to overwrite"), "unexpected message: {msg}");
    }

    #[test]
    fn test_deploy_on_top_of_eoa_with_balance() {
        let mut db = InMemoryDB::default();
        // Simulate an EOA that received ETH before Rex5.
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default() // code_hash = KECCAK_EMPTY
            },
        );
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let result =
            transact_deploy_sequencer_registry(&hardforks, 0, 1000, &mut state, &test_config())
                .unwrap()
                .unwrap();

        let account = result.get(&SEQUENCER_REGISTRY_ADDRESS).unwrap();
        assert!(account.is_created());
        assert_eq!(account.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
        // Pre-existing balance is preserved.
        assert_eq!(account.info.balance, U256::from(1_000_000));
    }

    #[test]
    fn test_resolve_pre_rex5_returns_legacy() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let (addr, _) =
            resolve_system_address(MegaHardforkConfig::default(), MegaSpecId::REX4, &mut state)
                .unwrap();
        assert_eq!(addr, MEGA_SYSTEM_ADDRESS);
    }

    #[test]
    fn test_resolve_rex5_returns_stored_system_address_with_witness() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SYSTEM_ADDRESS,
            TEST_SYSTEM_ADDRESS.into_word().into(),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let (addr, witness) =
            resolve_system_address(&rex5_hardforks(), MegaSpecId::REX5, &mut state).unwrap();
        assert_eq!(addr, TEST_SYSTEM_ADDRESS);

        // Witness must capture the registry account and the CURRENT_SYSTEM_ADDRESS slot.
        let witness = witness.expect("post-deploy resolve must produce witness");
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("registry account in witness");
        assert_eq!(acc.info.code_hash, SEQUENCER_REGISTRY_CODE_HASH);
        assert!(
            acc.storage.contains_key(&CURRENT_SYSTEM_ADDRESS),
            "witness must include CURRENT_SYSTEM_ADDRESS slot"
        );
        assert_eq!(acc.storage.len(), 1, "witness should contain exactly one slot");
        let slot = acc.storage.get(&CURRENT_SYSTEM_ADDRESS).expect("slot must exist");
        assert!(!slot.is_changed(), "read-only witness slot must not be marked changed");
        assert_eq!(slot.original_value(), slot.present_value());
    }

    #[test]
    fn test_resolve_rex5_zero_slot_errors() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let result = resolve_system_address(&rex5_hardforks(), MegaSpecId::REX5, &mut state);
        assert!(result.is_err(), "zero _currentSystemAddress should be an error");
    }

    #[test]
    fn test_resolve_rex5_missing_registry_errors() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = rex5_hardforks();

        let err = resolve_system_address(&hardforks, MegaSpecId::REX5, &mut state)
            .expect_err("missing registry at Rex5 must fail closed");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn test_resolve_rex5_wrong_code_hash_errors() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: B256::ZERO,
                code: Some(Bytecode::new_raw(Bytes::from_static(&[0x60, 0x00]))),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let err = resolve_system_address(&rex5_hardforks(), MegaSpecId::REX5, &mut state)
            .expect_err("wrong code hash must fail closed");

        assert!(err.to_string().contains("code hash mismatch"));
    }

    #[test]
    fn test_is_apply_pending_changes_due_no_registry() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let (due, witness) = is_apply_pending_changes_due(&mut state, 1000).unwrap();
        assert!(!due);
        // Witness must contain a not-existing account entry.
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness should exist");
        assert_eq!(acc.status, revm::state::AccountStatus::LoadedAsNotExisting);
        assert!(acc.storage.is_empty(), "no-registry witness should have no slots");
    }

    #[test]
    fn test_is_apply_pending_changes_due_no_pending() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let mut state = State::builder().with_database(&mut db).build();

        let (due, witness) = is_apply_pending_changes_due(&mut state, 1000).unwrap();
        assert!(!due);
        // Witness must include both PENDING_* slots (both zero = no change pending).
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness should exist");
        assert!(
            acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS),
            "witness must include PENDING_SYSTEM_ADDRESS"
        );
        assert!(
            acc.storage.contains_key(&PENDING_SEQUENCER),
            "witness must include PENDING_SEQUENCER"
        );
        // No activation slots needed because both pending values are zero.
        assert_eq!(acc.storage.len(), 2, "no-pending witness should have exactly 2 slots");
    }

    #[test]
    fn test_is_apply_pending_changes_due_system_address_due() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let new_addr = address!("0x1111111111111111111111111111111111111111");
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SYSTEM_ADDRESS,
            new_addr.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(1000),
        )
        .unwrap();
        // Not yet due at block 999.
        let mut state = State::builder().with_database(&mut db).build();
        let (due, witness) = is_apply_pending_changes_due(&mut state, 999).unwrap();
        assert!(!due, "not yet due");
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness");
        assert!(acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(acc.storage.contains_key(&SYSTEM_ADDRESS_ACTIVATION_BLOCK));

        // Exactly due at block 1000 (fresh State to avoid cache from prior call).
        let mut state = State::builder().with_database(&mut db).build();
        let (due, witness) = is_apply_pending_changes_due(&mut state, 1000).unwrap();
        assert!(due, "exactly due");
        // Both roles' pending slots are always read into the witness.
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness");
        assert!(acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(acc.storage.contains_key(&SYSTEM_ADDRESS_ACTIVATION_BLOCK));
        assert!(acc.storage.contains_key(&PENDING_SEQUENCER));
    }

    #[test]
    fn test_is_apply_pending_changes_due_sequencer_due() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let new_seq = address!("0x2222222222222222222222222222222222222222");
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SEQUENCER,
            new_seq.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SEQUENCER_ACTIVATION_BLOCK,
            U256::from(500),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let (due, witness) = is_apply_pending_changes_due(&mut state, 500).unwrap();
        assert!(due);
        // System address has no pending change (slot is zero) so only PENDING_SYSTEM_ADDRESS
        // is read. Then sequencer path reads PENDING_SEQUENCER + SEQUENCER_ACTIVATION_BLOCK.
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness");
        assert!(acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(acc.storage.contains_key(&PENDING_SEQUENCER));
        assert!(acc.storage.contains_key(&SEQUENCER_ACTIVATION_BLOCK));
        assert_eq!(acc.storage.len(), 3);
    }

    #[test]
    fn test_is_apply_pending_changes_due_checks_sequencer_when_system_not_due() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );
        let new_sys = address!("0x1111111111111111111111111111111111111111");
        let new_seq = address!("0x2222222222222222222222222222222222222222");
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SYSTEM_ADDRESS,
            new_sys.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(1001),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SEQUENCER,
            new_seq.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SEQUENCER_ACTIVATION_BLOCK,
            U256::from(1000),
        )
        .unwrap();
        let mut state = State::builder().with_database(&mut db).build();

        let (due, witness) = is_apply_pending_changes_due(&mut state, 1000).unwrap();
        assert!(
            due,
            "sequencer change should still trigger the pre-block call when the system address is not due yet"
        );
        // System address is not due (activation at 1001) but its pending slot + activation
        // slot are still read.  Then the sequencer path reads its own pending + activation.
        let acc = witness.get(&SEQUENCER_REGISTRY_ADDRESS).expect("witness");
        assert!(acc.storage.contains_key(&PENDING_SYSTEM_ADDRESS));
        assert!(acc.storage.contains_key(&SYSTEM_ADDRESS_ACTIVATION_BLOCK));
        assert!(acc.storage.contains_key(&PENDING_SEQUENCER));
        assert!(acc.storage.contains_key(&SEQUENCER_ACTIVATION_BLOCK));
        assert_eq!(acc.storage.len(), 4, "both roles' slots should be in the witness");
        for slot_key in [
            PENDING_SYSTEM_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            PENDING_SEQUENCER,
            SEQUENCER_ACTIVATION_BLOCK,
        ] {
            let slot = acc.storage.get(&slot_key).expect("slot must exist");
            assert!(!slot.is_changed(), "read-only witness slot must not be marked changed");
            assert_eq!(slot.original_value(), slot.present_value());
        }
    }

    #[test]
    fn test_transact_apply_pending_changes_updates_and_clears_due_roles() {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );

        let next_system_address = address!("0x1111111111111111111111111111111111111111");
        let next_sequencer = address!("0x2222222222222222222222222222222222222222");

        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SYSTEM_ADDRESS,
            TEST_SYSTEM_ADDRESS.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            CURRENT_SEQUENCER,
            TEST_SEQUENCER.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SYSTEM_ADDRESS,
            next_system_address.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            U256::from(1000),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            PENDING_SEQUENCER,
            next_sequencer.into_word().into(),
        )
        .unwrap();
        db.insert_account_storage(
            SEQUENCER_REGISTRY_ADDRESS,
            SEQUENCER_ACTIVATION_BLOCK,
            U256::from(1000),
        )
        .unwrap();

        let block =
            BlockEnv { number: U256::from(1000), gas_limit: 30_000_000, ..Default::default() };
        let mut context = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(context);

        let result =
            transact_apply_pending_changes(&mut evm).expect("applyPendingChanges() should succeed");
        let state = result.state;
        drop(evm);

        revm::DatabaseCommit::commit(&mut db, state);

        assert_eq!(
            revm::Database::storage(&mut db, SEQUENCER_REGISTRY_ADDRESS, CURRENT_SYSTEM_ADDRESS)
                .unwrap(),
            address_to_storage_value(next_system_address),
        );
        assert_eq!(
            revm::Database::storage(&mut db, SEQUENCER_REGISTRY_ADDRESS, CURRENT_SEQUENCER)
                .unwrap(),
            address_to_storage_value(next_sequencer),
        );
        assert_eq!(
            revm::Database::storage(&mut db, SEQUENCER_REGISTRY_ADDRESS, PENDING_SYSTEM_ADDRESS)
                .unwrap(),
            U256::ZERO,
        );
        assert_eq!(
            revm::Database::storage(&mut db, SEQUENCER_REGISTRY_ADDRESS, PENDING_SEQUENCER)
                .unwrap(),
            U256::ZERO,
        );
        assert_eq!(
            revm::Database::storage(
                &mut db,
                SEQUENCER_REGISTRY_ADDRESS,
                SYSTEM_ADDRESS_ACTIVATION_BLOCK,
            )
            .unwrap(),
            U256::ZERO,
        );
        assert_eq!(
            revm::Database::storage(
                &mut db,
                SEQUENCER_REGISTRY_ADDRESS,
                SEQUENCER_ACTIVATION_BLOCK,
            )
            .unwrap(),
            U256::ZERO,
        );
    }

    #[test]
    fn test_transact_apply_pending_changes_uses_block_gas_limit() {
        // Block gas_limit > 30M must be passed through to the system call so that
        // applyPendingChanges() can absorb the variable cost from REX dynamic
        // storage gas instead of being capped at the upstream-fixed 30M.
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );

        let block =
            BlockEnv { number: U256::from(1000), gas_limit: 250_000_000, ..Default::default() };
        let mut context = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(context);

        transact_apply_pending_changes(&mut evm).expect("system call should succeed");
        assert_eq!(revm::handler::EvmTr::ctx_ref(&evm).tx.base.gas_limit, 250_000_000);
    }

    #[test]
    fn test_transact_apply_pending_changes_respects_30m_floor() {
        // When the block gas limit is below the 30M floor, the system call must
        // still receive at least the historical default budget.
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: SEQUENCER_REGISTRY_CODE_HASH,
                code: Some(Bytecode::new_raw(SEQUENCER_REGISTRY_CODE)),
                ..Default::default()
            },
        );

        let block =
            BlockEnv { number: U256::from(1000), gas_limit: 1_000_000, ..Default::default() };
        let mut context = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(context);

        transact_apply_pending_changes(&mut evm).expect("system call should succeed");
        assert_eq!(
            revm::handler::EvmTr::ctx_ref(&evm).tx.base.gas_limit,
            crate::constants::rex5::SYSTEM_CALL_GAS_LIMIT_FLOOR,
        );
    }

    #[test]
    fn test_transact_apply_pending_changes_errors_when_registry_reverts() {
        let revert_code = Bytecode::new_legacy(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]));

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SEQUENCER_REGISTRY_ADDRESS,
            AccountInfo {
                code_hash: revert_code.hash_slow(),
                code: Some(revert_code),
                ..Default::default()
            },
        );

        let block =
            BlockEnv { number: U256::from(1000), gas_limit: 30_000_000, ..Default::default() };
        let mut context = crate::MegaContext::new(&mut db, MegaSpecId::REX5).with_block(block);
        context.modify_chain(|chain| {
            chain.operator_fee_scalar = Some(U256::ZERO);
            chain.operator_fee_constant = Some(U256::ZERO);
        });
        let mut evm = crate::MegaEvm::new(context);

        let err = transact_apply_pending_changes(&mut evm)
            .expect_err("reverting registry bytecode must fail closed");

        assert!(err.to_string().contains("reverted or halted"));
    }
}
