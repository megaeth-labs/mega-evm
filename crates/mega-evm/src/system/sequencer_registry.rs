//! The `SequencerRegistry` system contract for the `MegaETH` EVM.
//!
//! Tracks two independent roles: system address (Oracle/system-tx authority) and
//! sequencer (mini-block signing). Each role has its own rotation lifecycle.
//!
//! Unlike intercepted system contracts (`LimitControl`, `AccessControl`), this contract runs
//! normal on-chain bytecode. It does not have an interceptor.
//!
//! Deployed via raw state patch with initial storage seeded at deploy time.
//! Due rotations are applied via a single pre-block EVM system call to
//! `applyPendingChanges()`, following the same pattern as EIP-2935 and EIP-4788.
//! The current system transaction sender should always be read through
//! [`resolve_system_address`], which handles both the bootstrap fallback and the
//! on-chain `SequencerRegistry` state.

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
    state::{Account, Bytecode, EvmState},
    Database as RevmDatabase,
};

use crate::{MegaHardforks, MEGA_SYSTEM_ADDRESS};

/// The address of the `SequencerRegistry` system contract.
pub const SEQUENCER_REGISTRY_ADDRESS: Address =
    address!("0x6342000000000000000000000000000000000006");

/// The code of the `SequencerRegistry` contract (version 1.0.0).
pub use mega_system_contracts::sequencer_registry::V1_0_0_CODE as SEQUENCER_REGISTRY_CODE;

/// The code hash of the `SequencerRegistry` contract (version 1.0.0).
pub use mega_system_contracts::sequencer_registry::V1_0_0_CODE_HASH as SEQUENCER_REGISTRY_CODE_HASH;

pub use mega_system_contracts::sequencer_registry::ISequencerRegistry;

/// Bootstrap configuration for the initial `SequencerRegistry` deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencerRegistryConfig {
    /// The initial system address (Oracle/system-tx sender).
    pub initial_system_address: Address,
    /// The initial sequencer (mini-block signing key).
    pub initial_sequencer: Address,
    /// The initial admin for the registry.
    pub initial_admin: Address,
}

impl Default for SequencerRegistryConfig {
    fn default() -> Self {
        Self {
            // Preserve the legacy single-address behavior unless the chain spec overrides it.
            initial_system_address: MEGA_SYSTEM_ADDRESS,
            initial_sequencer: MEGA_SYSTEM_ADDRESS,
            initial_admin: MEGA_SYSTEM_ADDRESS,
        }
    }
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

/// Deploys the `SequencerRegistry` contract and seeds initial storage.
///
/// On first deploy, writes 6 flat storage slots:
/// - `_currentSystemAddress`, `_currentSequencer`, `_admin`
/// - `_initialSystemAddress`, `_initialSequencer`, `_initialFromBlock`
///
/// The dynamic rotation arrays remain empty on bootstrap and grow only when
/// `applyPendingChanges()` commits a due rotation.
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
        // Account exists but has wrong code hash. This is a stateful contract —
        // silently overwriting would destroy rotation state and pending changes.
        // This indicates state corruption or an unsupported migration path.
        return Err(BlockValidationError::BlockHashContractCall {
            message: format!(
                "SequencerRegistry at {} has unexpected code hash {} (expected {}); \
                 refusing to overwrite stateful contract storage without migration",
                SEQUENCER_REGISTRY_ADDRESS, account_info.code_hash, SEQUENCER_REGISTRY_CODE_HASH,
            ),
        }
        .into());
    }

    // First deploy — account does not exist yet.
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

/// Checks whether any pending role rotation is due by reading committed storage.
///
/// Returns `true` when the role has a non-zero pending value and its activation block
/// has been reached in the current block.
fn is_rotation_due<DB: Database>(
    db: &mut State<DB>,
    pending_slot: U256,
    activation_slot: U256,
    block_number: u64,
) -> Result<bool, BlockExecutionError> {
    let pending = read_registry_storage(db, pending_slot)?;
    if pending.is_zero() {
        return Ok(false);
    }

    let activation = read_registry_storage(db, activation_slot)?;
    Ok(block_number >= activation.saturating_to::<u64>())
}

/// Returns `true` if the caller should issue the single pre-block
/// `applyPendingChanges()` system call.
///
/// This is only a pre-check to avoid an EVM system call on every block.
/// The contract-side `applyPendingChanges()` call applies both roles, so the caller
/// only needs to know whether any role is due.
pub(crate) fn is_apply_pending_changes_due<DB: Database>(
    db: &mut State<DB>,
    block_number: u64,
) -> Result<bool, BlockExecutionError> {
    let acc =
        db.load_cache_account(SEQUENCER_REGISTRY_ADDRESS).map_err(BlockExecutionError::other)?;
    if acc.account_info().is_none() {
        return Ok(false);
    }

    if is_rotation_due(db, PENDING_SYSTEM_ADDRESS, SYSTEM_ADDRESS_ACTIVATION_BLOCK, block_number)? {
        return Ok(true);
    }

    is_rotation_due(db, PENDING_SEQUENCER, SEQUENCER_ACTIVATION_BLOCK, block_number)
}

/// Executes the pre-block `applyPendingChanges()` system call on the `SequencerRegistry`.
///
/// This single system call applies both the system address rotation and the sequencer
/// rotation if they are due in the current block.
/// Caller should gate this with [`is_apply_pending_changes_due`] to avoid an EVM
/// call on every block.
pub(crate) fn transact_apply_pending_changes<Halt>(
    evm: &mut impl alloy_evm::Evm<HaltReason = Halt>,
) -> Result<Option<ResultAndState<Halt>>, BlockExecutionError> {
    let calldata = ISequencerRegistry::applyPendingChangesCall {}.abi_encode();
    let result_and_state = match evm.transact_system_call(
        alloy_eips::eip4788::SYSTEM_ADDRESS,
        SEQUENCER_REGISTRY_ADDRESS,
        Bytes::from(calldata),
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

    Ok(Some(result_and_state))
}

/// Resolves the current system address.
///
/// Must be called after `commit_system_call_outcomes()`.
///
/// - Pre-REX5: returns `MEGA_SYSTEM_ADDRESS`.
/// - REX5 before `SequencerRegistry` deployment: returns the configured bootstrap system address.
/// - REX5 after deployment: returns `_currentSystemAddress` from committed registry storage.
///
/// This helper is the only supported way for runtime code to resolve the current
/// system transaction sender across the bootstrap and post-bootstrap phases.
pub fn resolve_system_address<DB: Database>(
    hardforks: impl MegaHardforks,
    spec: crate::MegaSpecId,
    db: &mut State<DB>,
) -> Result<Address, BlockExecutionError> {
    if !spec.is_enabled(crate::MegaSpecId::REX5) {
        return Ok(MEGA_SYSTEM_ADDRESS);
    }

    let bootstrap_system_address = hardforks.sequencer_registry_config().initial_system_address;

    let acc =
        db.load_cache_account(SEQUENCER_REGISTRY_ADDRESS).map_err(BlockExecutionError::other)?;

    let Some(info) = acc.account_info() else {
        return Ok(bootstrap_system_address);
    };

    if info.code_hash != SEQUENCER_REGISTRY_CODE_HASH {
        return Err(BlockValidationError::BlockHashContractCall {
            message: format!(
                "SequencerRegistry code hash mismatch: expected {}, got {}",
                SEQUENCER_REGISTRY_CODE_HASH, info.code_hash
            ),
        }
        .into());
    }

    let value = read_registry_storage(db, CURRENT_SYSTEM_ADDRESS)?;

    if value.is_zero() {
        return Err(BlockValidationError::BlockHashContractCall {
            message: "SequencerRegistry deployed but _currentSystemAddress is zero".into(),
        }
        .into());
    }

    Ok(Address::from_word(value.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256, B256};
    use revm::{database::InMemoryDB, state::AccountInfo};

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
        let hardforks = MegaHardforkConfig::default().with_all_activated();

        let err =
            transact_deploy_sequencer_registry(&hardforks, 0, 2000, &mut state, &test_config())
                .expect_err("wrong code hash must fail closed");

        let msg = err.to_string();
        assert!(msg.contains("unexpected code hash"), "unexpected message: {msg}");
        assert!(msg.contains("refusing to overwrite"), "unexpected message: {msg}");
    }

    #[test]
    fn test_resolve_pre_rex5_returns_legacy() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        let addr =
            resolve_system_address(MegaHardforkConfig::default(), MegaSpecId::REX4, &mut state)
                .unwrap();
        assert_eq!(addr, MEGA_SYSTEM_ADDRESS);
    }

    #[test]
    fn test_resolve_rex5_returns_stored_system_address() {
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

        let addr =
            resolve_system_address(MegaHardforkConfig::default(), MegaSpecId::REX5, &mut state)
                .unwrap();
        assert_eq!(addr, TEST_SYSTEM_ADDRESS);
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

        let result =
            resolve_system_address(MegaHardforkConfig::default(), MegaSpecId::REX5, &mut state);
        assert!(result.is_err(), "zero _currentSystemAddress should be an error");
    }

    #[test]
    fn test_resolve_rex5_missing_registry_returns_bootstrap_system_address() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();
        let hardforks = MegaHardforkConfig::default().with_sequencer_registry_config(test_config());

        let addr = resolve_system_address(&hardforks, MegaSpecId::REX5, &mut state).unwrap();
        assert_eq!(addr, TEST_SYSTEM_ADDRESS);
    }

    #[test]
    fn test_is_apply_pending_changes_due_no_registry() {
        let mut db = InMemoryDB::default();
        let mut state = State::builder().with_database(&mut db).build();

        assert!(!is_apply_pending_changes_due(&mut state, 1000).unwrap());
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

        assert!(!is_apply_pending_changes_due(&mut state, 1000).unwrap());
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
        let mut state = State::builder().with_database(&mut db).build();

        assert!(!is_apply_pending_changes_due(&mut state, 999).unwrap(), "not yet due");
        assert!(is_apply_pending_changes_due(&mut state, 1000).unwrap(), "exactly due");
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

        assert!(is_apply_pending_changes_due(&mut state, 500).unwrap());
    }
}
